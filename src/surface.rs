//! The `SceneContent` that hosts a terminal session: input routing (keyboard,
//! IME, pointer, scroll), the PTY event drain, and resize bookkeeping. Drawing
//! goes through the shared `Scene2D` facilities — the same path math/chart
//! use — inside `build_scene`; rendering itself belongs to the backend.
//!
//! The PTY parser runs on its own thread and cannot touch the main-thread
//! `SceneInvalidator` (`Rc<dyn Fn()>`). Wake-ups therefore cross threads once —
//! a `try_send` into a local channel — where a future spawned on the local
//! executor drains the queue and calls the invalidator on the main thread.
//! The winit event loop notices the local-task ping and turns it into a patch
//! request, so this never needs an extra polling timer.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::{TermMode, viewport_to_point};
use alacritty_terminal::vte::ansi::Processor;
use nami::{Binding, Signal, binding};
use waterui::cursor::CursorStyle;
use waterui::task::spawn_local;
use waterui_core::Str;
use waterui_graphics::input::{ScrollUnit, SurfaceInputEvent, SurfacePointerButton};
use waterui_graphics::scene2d::Scene2D;
use waterui_graphics::scene_view::{SceneContent, SceneInvalidator};
use waterui_graphics::{Code, Key, Modifiers, NamedKey};
use waterui_text::FontCollection;

use crate::app::{AppState, FONT_SIZE, Session};
use crate::fonts::TermFonts;
use crate::keys::{TermAction, action_chord, key_release_bytes, key_to_bytes, tab_chord};
use crate::mouse::{self, CellPos, MouseAction};
use crate::osctap::TapEvent;
use crate::palette::Palette;
use crate::scene::{self, CursorInfo, DrawContext, HintSpan, PADDING, ScrollInfo, cursor_info};
use crate::terminal::TermEvent;

/// Blink half-period for the cursor.
const BLINK_HALF: Duration = Duration::from_millis(530);
/// Bell flash decay time.
const BELL_FLASH_SECS: f32 = 0.15;
/// How long the cols×rows resize badge stays after the last change.
const RESIZE_LABEL_MS: Duration = Duration::from_millis(900);
/// Overlay alpha of a bell flash: a step flash at full strength for the
/// whole `BELL_FLASH_SECS` duration — kitty's `visual_bell` semantics,
/// clearly visible rather than a faint decaying hint.
const BELL_FLASH_ALPHA: f32 = 0.35;

fn bell_flash_alpha(bell_at: Option<Instant>, now: Instant) -> f32 {
    bell_at
        .map(|t| {
            if now.duration_since(t).as_secs_f32() < BELL_FLASH_SECS {
                BELL_FLASH_ALPHA
            } else {
                0.0
            }
        })
        .unwrap_or(0.0)
}
/// Time window for double/triple click detection.
/// One soft-wrap chain joined into a logical line, plus a map back to
/// the grid cells that produced each char.
struct LineMap {
    /// Joined cell text — one char per `cell_text` item; wide-char
    /// spacers contribute nothing and a cell's zerowidths follow its
    /// base char.
    chars: Vec<char>,
    /// `(index into `chars`, grid line, start col, cell width)` per
    /// contributing cell, in document order.
    marks: Vec<(usize, i32, usize, usize)>,
}

/// The grid cell that produced `lm.chars[i]` as `(line, col, width)`.
/// `lm.marks` must be non-empty.
fn line_cell(lm: &LineMap, i: usize) -> (i32, usize, usize) {
    let m = match lm.marks.binary_search_by_key(&i, |m| m.0) {
        Ok(k) => lm.marks[k],
        Err(0) => lm.marks[0],
        Err(k) => lm.marks[k - 1],
    };
    (m.1, m.2, m.3)
}

/// A grid `line`'s row carries the soft-wrap flag on its last cell.
fn row_wraps(grid: &alacritty_terminal::grid::Grid<alacritty_terminal::term::cell::Cell>, line: i32) -> bool {
    grid[Line(line)]
        .last()
        .is_some_and(|c| c.flags.contains(Flags::WRAPLINE))
}

/// Join `top..=bottom` grid rows into logical lines: a row whose last
/// cell is flagged WRAPLINE continues into the next row.
fn logical_lines(
    grid: &alacritty_terminal::grid::Grid<alacritty_terminal::term::cell::Cell>,
    top: i32,
    bottom: i32,
) -> Vec<LineMap> {
    let mut out = Vec::new();
    let mut lm = LineMap { chars: Vec::new(), marks: Vec::new() };
    for line in top..=bottom {
        let row = &grid[Line(line)];
        let mut col = 0;
        for cell in &row[..] {
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                col += 1;
                continue;
            }
            let width = usize::from(cell.flags.contains(Flags::WIDE_CHAR)) + 1;
            lm.marks.push((lm.chars.len(), line, col, width));
            lm.chars.extend(crate::scene::cell_text(cell).chars());
            col += 1;
        }
        if !row_wraps(grid, line) {
            out.push(lm);
            lm = LineMap { chars: Vec::new(), marks: Vec::new() };
        }
    }
    if !lm.marks.is_empty() {
        out.push(lm);
    }
    out
}

/// The logical line covering grid `line` — walks to the chain's edges.
fn logical_line_at(
    grid: &alacritty_terminal::grid::Grid<alacritty_terminal::term::cell::Cell>,
    line: i32,
) -> LineMap {
    let top = -(grid.history_size() as i32);
    let bottom = grid.screen_lines() as i32 - 1;
    let mut first = line.clamp(top, bottom);
    while first > top && row_wraps(grid, first - 1) {
        first -= 1;
    }
    let mut last = line.clamp(top, bottom);
    while last < bottom && row_wraps(grid, last) {
        last += 1;
    }
    logical_lines(grid, first, last).into_iter().next().unwrap()
}

/// `(start col, end col exclusive, grid line)` highlight segments
/// covering `lm.chars[s..e]` — one entry per covered row, so a match
/// crossing a soft wrap highlights both parts.
fn span_segments(lm: &LineMap, s: usize, e: usize, cols: usize) -> Vec<(usize, usize, i32)> {
    debug_assert!(s < e);
    let (l0, c0, _) = line_cell(lm, s);
    let (l1, c1, w1) = line_cell(lm, e - 1);
    if l0 == l1 {
        vec![(c0, c1 + w1, l0)]
    } else {
        let mut segs = vec![(c0, cols, l0)];
        for l in l0 + 1..l1 {
            segs.push((0, cols, l));
        }
        segs.push((0, c1 + w1, l1));
        segs
    }
}

/// Every `query` hit in one logical line — each match is a list of
/// `(start col, end col exclusive, grid line)` segments (more than one
/// when the match crosses a soft wrap). `query` is lowercase chars.
fn line_map_matches(
    lm: &LineMap,
    query: &[char],
    cols: usize,
) -> Vec<Vec<(usize, usize, i32)>> {
    if lm.marks.is_empty() || query.is_empty() {
        return Vec::new();
    }
    // Lowercase char-wise with a remap back to `chars` indices, so owner
    // lookups survive expansion (e.g. `İ` → `i` + combining dot).
    let mut lower = Vec::with_capacity(lm.chars.len());
    let mut remap = Vec::with_capacity(lm.chars.len());
    for (i, &c) in lm.chars.iter().enumerate() {
        for lc in c.to_lowercase() {
            lower.push(lc);
            remap.push(i);
        }
    }
    let q = query.len();
    if lower.len() < q {
        return Vec::new();
    }
    let mut out = Vec::new();
    for j in 0..=(lower.len() - q) {
        if lower[j..j + q] == query[..] {
            out.push(span_segments(lm, remap[j], remap[j + q - 1] + 1, cols));
        }
    }
    out
}

/// Max cell distance for a multi-click to count as same-cell.
const MULTI_CLICK_RANGE: usize = 1;
/// Min spacing between X11 bell rings — throttles tab-completion storms.
const BELL_AUDIO_MIN: Duration = Duration::from_millis(120);

/// Ring the X11 keyboard bell (what xterm rings on BEL), throttled.
fn ring_bell(last: &mut Option<Instant>) {
    if last.is_some_and(|t| t.elapsed() < BELL_AUDIO_MIN) {
        return;
    }
    *last = Some(Instant::now());
    let _ = std::process::Command::new("xkbbell").spawn();
}

/// In-surface text search state (Ctrl+Shift+F).
struct Search {
    query: String,
    /// One entry per match — `(start col, end col exclusive, grid
    /// line)` segments; a match crossing a soft wrap has one segment
    /// per covered row. Grid lines go negative into scrollback.
    matches: Vec<Vec<(usize, usize, i32)>>,
    active: usize,
    /// `(cols, history, screen lines, content gen)` at the last run —
    /// any change (reflow on resize, new output) re-runs the search so
    /// highlights stay on the moved match cells.
    stamp: (usize, usize, usize, u64),
}

/// URL hint mode state — numbered link chips + the digits typed so far.
struct HintState {
    /// One chip per visible link, in reading order.
    spans: Vec<HintSpan>,
    /// Same order as `spans` — label `n` opens `urls[n - 1]`.
    urls: Vec<String>,
    /// Digits accumulated by `hint_key`.
    digits: String,
}

/// One terminal surface — scene content + input owner for a session.
pub struct TermSurface {
    session: Rc<Session>,
    app: AppState,
    fonts: TermFonts,
    /// Shared palette — swapped on theme reload.
    palette: Rc<RefCell<Palette>>,
    font_size_pt: f32,
    /// The `font-family` pref the shaping stack was built for — compared
    /// against `session.font_family` each frame (hot reload).
    family_pref: String,

    // geometry (grid size in cells, logical units at draw time)
    cols: u16,
    lines: u16,

    // The backend's frame-request callback — kept so input events (which
    // don't schedule a frame on delivery) can request a repaint when they
    // change what the scene draws.
    invalidator: Option<SceneInvalidator>,
    // cross-thread wake pipe: parser thread → channel → local future →
    // SceneInvalidator on the main thread.
    wake_tx: Option<async_channel::Sender<()>>,
    /// Bumped on every drained parser wake — part of the search stamp,
    /// so new output re-runs an open search.
    content_gen: Rc<Cell<u64>>,
    /// Dead-man switch for the spawned future — cleared on teardown/None.
    wake_alive: Rc<Cell<bool>>,
    /// The parked drain future; dropped (detached) on teardown.
    wake_task: Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>>,

    // interaction state
    focused: bool,
    modifiers: Modifiers,
    /// Protocol button currently held (drag reporting).
    held_button: Option<u8>,
    selecting: bool,
    /// (instant, row, col, count) of the last press for multi-click.
    last_click: Option<(Instant, usize, usize, u8)>,
    preedit: Option<(String, usize)>,
    scroll_accum_px: f64,
    bell_at: Option<Instant>,
    /// Last grid-size change for the `resize-overlay` badge decay.
    resize_at: Option<Instant>,
    /// Last time the X11 bell actually rang (throttle).
    bell_ring_at: Option<Instant>,
    blink_epoch: Instant,
    search: Option<Search>,
    /// URL hint mode state — chips over every visible link + digits typed.
    hints: Option<HintState>,
    /// Keyboard selection mode (`start_selection`): `(anchor, moving end)`;
    /// `term.selection` is rebuilt from the pair on each move.
    keysel: Option<(Point, Point)>,
    clipboard: Option<waterkit_clipboard::Clipboard>,
    /// X11 pointer-hide for `mouse-hide-while-typing` (None off-X11).
    cursor_hider: Option<crate::xcursor::CursorHider>,
    /// Set when the hider was attempted — avoid reconnecting per frame.
    cursor_hider_tried: bool,
    /// Linux PRIMARY selection (X11, or Wayland data-control where the
    /// compositor offers it) — waterkit-clipboard's `PrimarySelection`;
    /// claims on copy-on-select, middle-click reads it.
    primary: Option<waterkit_clipboard::PrimarySelection>,
    /// Last pointer position in surface-local coords — re-evaluates the
    /// Ctrl+hover link affordance when the modifier chord changes.
    pointer_at: (f64, f64),
    /// Ctrl-hovered link span in viewport segments `(c0, c1, row)` —
    /// drawn underlined and drives the pointer cursor.
    hover_link: Vec<(usize, usize, usize)>,
    /// Drives `.cursor(...)` on the SceneView: IBeam over the grid,
    /// PointingHand over a Ctrl-hovered link.
    pub hover_cursor: Binding<CursorStyle>,
    /// `background-image` decode cache: (config path, decoded brush+dims)
    /// — `None` brush means the file failed to read/decode; the path is
    /// still cached so a bad path is not re-read every frame.
    bg_img: RefCell<BgImageCache>,
}

/// `background-image` decode cache: (config path, decoded brush + pixel dims).
type BgImageCache = (Option<std::path::PathBuf>, Option<(peniko::ImageBrush, u32, u32)>);

impl TermSurface {
    /// Scene content for one session — the host's shared font collection is
    /// resolved once here, per pane.
    pub fn new(
        session: Rc<Session>,
        app: AppState,
        palette: Rc<RefCell<Palette>>,
        fonts: FontCollection,
    ) -> Self {
        let font_size = session.font_size.snapshot();
        let family_pref = session.font_family.snapshot().to_string();
        app.register_theme_wake(&session.terminal);
        Self {
            session,
            app,
            fonts: TermFonts::load(fonts, font_size, &family_pref),
            palette,
            font_size_pt: font_size,
            family_pref,
            cols: 0,
            lines: 0,
            invalidator: None,
            wake_tx: None,
            content_gen: Rc::new(Cell::new(0)),
            wake_alive: Rc::new(Cell::new(false)),
            wake_task: None,
            focused: false,
            modifiers: Modifiers::empty(),
            held_button: None,
            selecting: false,
            last_click: None,
            preedit: None,
            scroll_accum_px: 0.0,
            bell_at: None,
            resize_at: None,
            bell_ring_at: None,
            blink_epoch: Instant::now(),
            search: None,
            hints: None,
            keysel: None,
            clipboard: waterkit_clipboard::Clipboard::new().ok(),
            cursor_hider: None,
            cursor_hider_tried: false,
            primary: waterkit_clipboard::PrimarySelection::new().ok(),
            pointer_at: (0.0, 0.0),
            hover_link: Vec::new(),
            hover_cursor: binding(CursorStyle::IBeam),
            bg_img: RefCell::new((None, None)),
        }
    }

    /// The `background-image` brush + its brush→rect transform for this
    /// frame, or `None`. File bytes are decoded once per config path and
    /// cached; opacity/fit/repeat are applied per frame (hot reload).
    fn bg_image_draw(&self, w: f64, h: f64) -> Option<(peniko::ImageBrush, kurbo::Affine)> {
        use crate::config::BgFit;
        let (path, opacity, fit, repeat) = self.app.config(|c| {
            (
                c.background_image.clone(),
                c.background_image_opacity,
                c.background_image_fit,
                c.background_image_repeat,
            )
        });
        let path = path?;
        let mut cache = self.bg_img.borrow_mut();
        if cache.0.as_deref() != Some(path.as_path()) {
            let loaded = std::fs::read(&path)
                .ok()
                .and_then(|bytes| crate::kitty::decode_png(&bytes))
                .map(|(px, iw, ih)| {
                    let image = peniko::ImageData {
                        data: peniko::Blob::new(std::sync::Arc::new(px)),
                        format: peniko::ImageFormat::Rgba8,
                        alpha_type: peniko::ImageAlphaType::Alpha,
                        width: iw,
                        height: ih,
                    };
                    (peniko::ImageBrush::new(image), iw, ih)
                });
            *cache = (Some(path), loaded);
        }
        let (brush, iw, ih) = cache.1.clone()?;
        let (iw, ih) = (f64::from(iw), f64::from(ih));
        let tile = matches!(fit, BgFit::Tile) || repeat;
        let ext = if tile {
            peniko::Extend::Repeat
        } else {
            peniko::Extend::Pad
        };
        let brush = brush
            .with_alpha(opacity)
            .with_x_extend(ext)
            .with_y_extend(ext);
        // brush_transform maps image-pixel space into the surface rect.
        let transform = match fit {
            BgFit::Stretch => kurbo::Affine::scale_non_uniform(w / iw, h / ih),
            BgFit::Tile => kurbo::Affine::IDENTITY,
            BgFit::Contain | BgFit::Cover => {
                let s = if matches!(fit, BgFit::Contain) {
                    (w / iw).min(h / ih)
                } else {
                    (w / iw).max(h / ih)
                };
                kurbo::Affine::translate(((w - iw * s) / 2.0, (h - ih * s) / 2.0))
                    * kurbo::Affine::scale(s)
            }
        };
        Some((brush, transform))
    }

    /// Surface-local logical position → (col, row) in viewport coords.
    fn viewport_cell(&self, x: f64, y: f64) -> (usize, usize) {
        let m = self.fonts.metrics;
        let pad = PADDING as f64;
        let (cw, ch) = (m.cell_w as f64, m.cell_h as f64);
        let col = ((x - pad) / cw).clamp(0.0, self.cols.saturating_sub(1) as f64) as usize;
        let row = ((y - pad) / ch).clamp(0.0, self.lines.saturating_sub(1) as f64) as usize;
        (col, row)
    }

    /// Surface-local logical position → grid `Point` (scrollback-aware).
    fn grid_point(&self, x: f64, y: f64) -> Point {
        let (col, row) = self.viewport_cell(x, y);
        let offset = self.session.terminal.term.lock().grid().display_offset();
        viewport_to_point(offset, Point::new(row, Column(col)))
    }

    /// Which side of a cell the pointer is on (for selection anchors).
    fn cell_side(&self, x: f64) -> Side {
        let m = self.fonts.metrics;
        let pad = PADDING as f64;
        let cw = m.cell_w as f64;
        let within = (x - pad).rem_euclid(cw);
        if within < cw * 0.5 { Side::Left } else { Side::Right }
    }

    fn write(&self, bytes: impl Into<std::borrow::Cow<'static, [u8]>>) {
        let bytes = bytes.into();
        if std::env::var_os("HYDROTERM_DEBUG_INPUT").is_some() {
            let escaped = String::from_utf8_lossy(&bytes).escape_debug().to_string();
            eprintln!("[pty-write {:?}] {} bytes: {escaped}", std::time::Instant::now(), bytes.len());
        }
        self.session.terminal.write(bytes);
    }

    /// Clipboard text → PTY, with bracketed-paste markers when armed.
    /// Multi-line pastes route through the paste-protection confirm
    /// overlay (Ghostty `clipboard-paste-protection`) unless the program
    /// armed bracketed paste — wrapped text can't execute mid-paste.
    fn paste_clipboard(&mut self) {
        let Some(clip) = &self.clipboard else { return };
        if let Ok(Some(text)) = pollster::block_on(clip.text()) {
            let bracketed =
                self.session.terminal.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
            let unsafe_text = text.contains('\n') || text.contains('\r');
            if unsafe_text && !bracketed && self.app.config(|c| c.paste_protection) {
                self.session.pending_paste.set_from(Some(text.into()));
                return;
            }
            self.paste_text(&text, bracketed);
        }
    }

    /// Current CLIPBOARD contents (empty when the backend is absent).
    fn clipboard_text(&self) -> String {
        self.clipboard
            .as_ref()
            .and_then(|c| pollster::block_on(c.text()).ok().flatten())
            .unwrap_or_default()
    }

    /// Current PRIMARY contents, `None` when unowned or unsupported.
    fn primary_text(&self) -> Option<String> {
        self.primary
            .as_ref()
            .and_then(|p| pollster::block_on(p.text()).ok().flatten())
    }

    /// `clipboard-read = ask` answer: [Allow]/Enter replies with the
    /// clipboard, Escape replies empty and drops the request.
    fn clipboard_read_confirm(&mut self, accept: bool) {
        let fmt = self.session.pending_clipboard_fmt.borrow_mut().take();
        self.session.pending_clipboard_read.set(false);
        if let Some(manager) = self.session.snackbar.borrow().as_ref() {
            manager.dismiss();
        }
        if let Some(fmt) = fmt {
            let text = if accept { self.clipboard_text() } else { String::new() };
            self.write(fmt(&text).into_bytes());
        }
    }

    /// Write `text` to the PTY as a (possibly bracketed) paste.
    fn paste_text(&mut self, text: &str, bracketed: bool) {
        let mut out = String::with_capacity(text.len() + 12);
        if bracketed {
            out.push_str("\x1b[200~");
        }
        // A literal ESC would let the pasted text escape the bracket.
        out.push_str(&text.replace('\x1b', ""));
        if bracketed {
            out.push_str("\x1b[201~");
        }
        self.write(out.into_bytes());
        self.snap_to_cursor_if_scrolled();
    }

    /// Paste-protection overlay answer: write the stashed text
    /// (Enter/[Paste]) or drop it (Escape/[Cancel]).
    fn paste_confirm(&mut self, accept: bool) {
        let text = self.session.pending_paste.snapshot().map(|t| t.to_string());
        self.session.pending_paste.set(None);
        if let Some(manager) = self.session.snackbar.borrow().as_ref() {
            manager.dismiss();
        }
        if accept && let Some(text) = text {
            let bracketed =
                self.session.terminal.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
            self.paste_text(&text, bracketed);
        }
    }

    /// `clipboard-trim` — strip whitespace at the ends of copied text
    /// (the per-line trailing pads are already gone).
    fn trimmed_copy(&self, text: String) -> String {
        if self.app.config(|c| c.clipboard_trim) {
            text.trim().to_owned()
        } else {
            text
        }
    }

    /// The explicit Copy action writes CLIPBOARD only — PRIMARY
    /// continues to hold the last selection (kitty/xterm semantics).
    fn copy_selection(&mut self) {
        let text = self.session.terminal.term.lock().selection_to_string();
        if let Some(text) = text {
            let text = self.trimmed_copy(text);
            if let Some(clip) = self.clipboard.as_mut() {
                let _ = clip.set_text(&text);
            }
        }
    }

    /// Selection-end copy: `copy-on-select = clipboard|primary|both`
    /// routes the just-made selection (default `both`).
    fn copy_on_select(&mut self, mode: crate::config::CopyOnSelect) {
        use crate::config::CopyOnSelect as CoS;
        let text = self.session.terminal.term.lock().selection_to_string();
        if let Some(text) = text {
            let text = self.trimmed_copy(text);
            if std::env::var_os("HYDROTERM_DEBUG_INPUT").is_some() {
                eprintln!(
                    "[copy_sel] mode={mode:?} text={text:?} clip={} primary={}",
                    self.clipboard.is_some(),
                    self.primary.is_some()
                );
            }
            if matches!(mode, CoS::Clipboard | CoS::Both)
                && let Some(clip) = self.clipboard.as_mut()
            {
                let _ = clip.set_text(&text);
            }
            // PRIMARY tracks the last selection — a middle click
            // anywhere pastes what was last selected.
            if matches!(mode, CoS::Primary | CoS::Both)
                && let Some(primary) = self.primary.as_mut()
            {
                let r = primary.set_text(&text);
                if std::env::var_os("HYDROTERM_DEBUG_INPUT").is_some() {
                    eprintln!("[copy_sel] primary.set_text={r:?}");
                }
            }
        }
    }

    /// Open the link under `point`: an OSC8 hyperlink first, then a
    /// plain-text URL scanned off the row (like xterm/kitty Ctrl+click).
    fn open_link_at(&self, point: Point) -> bool {
        let term = self.session.terminal.term.lock();
        let uri = term.grid()[point].hyperlink().map(|h| h.uri().to_string());
        let uri = uri.or_else(|| {
            // Scan the logical line (soft wraps joined) so a link that
            // wraps across rows still resolves; the click's char index
            // is the mark of the cell under it.
            let lm = logical_line_at(term.grid(), point.line.0);
            let idx = lm
                .marks
                .iter()
                .rfind(|m| m.1 == point.line.0 && m.2 <= point.column.0)
                .map(|m| m.0)?;
            url_at(&lm.chars, idx)
        });
        drop(term);
        if let Some(uri) = uri {
            self.open_uri(&uri);
            return true;
        }
        false
    }

    /// Open `uri` via `open-link-with` (the configured program gets `{}`
    /// substituted, or the URL appended as its last argument); falls
    /// back to `xdg-open` when unset or the spawn fails.
    fn open_uri(&self, uri: &str) {
        if let Some(cmdline) = self.app.config(|c| c.open_link_with.clone()) {
            let mut parts = cmdline.split_whitespace();
            if let Some(prog) = parts.next() {
                let mut args: Vec<String> = parts.map(str::to_string).collect();
                if args.iter().any(|a| a.contains("{}")) {
                    for a in &mut args {
                        *a = a.replace("{}", uri);
                    }
                } else {
                    args.push(uri.to_string());
                }
                if std::process::Command::new(prog)
                    .args(&args)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .is_ok()
                {
                    return;
                }
            }
        }
        open_url(uri);
    }

    /// Apply a chord action (copy/paste/tabs/font/search).
    fn do_action(&mut self, action: TermAction) {
        match action {
            TermAction::Copy => self.copy_selection(),
            TermAction::Paste => self.paste_clipboard(),
            TermAction::PasteConfirm => self.paste_confirm(true),
            TermAction::DropText(text) => self.drop_text(&text),
            TermAction::NewTab => {
                self.app.new_tab();
            }
            TermAction::CloseTab => self.app.try_close_pane(self.session.id),
            TermAction::CloseConfirm => self.app.confirm_close(self.session.id),
            TermAction::NewWindow => self.app.new_window(),
            TermAction::ToggleQuickTerminal => self.app.toggle_quick(),
            TermAction::LastTab => self.app.select_last_tab(),
            TermAction::CloseWindow => self.app.close_window(),
            TermAction::ToggleTabBar => self.app.toggle_tab_bar(),
            TermAction::NextTab => self.app.cycle_tab(1),
            TermAction::PrevTab => self.app.cycle_tab(-1),
            TermAction::SelectTab(n) => self.app.select_tab(n),
            TermAction::FontBigger | TermAction::FontSmaller | TermAction::FontReset => {
                let cur = self.session.font_size.snapshot();
                let next = match action {
                    TermAction::FontBigger => (cur + 1.0).min(96.0),
                    TermAction::FontSmaller => (cur - 1.0).max(6.0),
                    _ => FONT_SIZE,
                };
                self.session.font_size.set(next);
            }
            TermAction::IncreaseFontSize(pts) | TermAction::DecreaseFontSize(pts) => {
                let cur = self.session.font_size.snapshot();
                let delta = if matches!(action, TermAction::DecreaseFontSize(_)) {
                    -pts as f32
                } else {
                    pts as f32
                };
                self.session.font_size.set((cur + delta).clamp(6.0, 96.0));
            }
            TermAction::ClearScrollback => {
                let mut term = self.session.terminal.term.lock();
                term.grid_mut().clear_history();
                term.scroll_display(Scroll::Bottom);
            }
            TermAction::ClearScreen => {
                // Ghostty `clear_screen`: erase display + scrollback,
                // cursor to origin — run through the VT parser so cell
                // flags, damage and wrap state stay consistent.
                let mut term = self.session.terminal.term.lock();
                let mut p: Processor = Processor::new();
                p.advance(&mut *term, b"\x1b[3J\x1b[2J\x1b[H");
                term.scroll_display(Scroll::Bottom);
            }
            TermAction::Reset => {
                // Ghostty `reset` == RIS: reset every mode and erase
                // screen + scrollback through the parser.
                let mut term = self.session.terminal.term.lock();
                let mut p: Processor = Processor::new();
                p.advance(&mut *term, b"\x1bc");
                term.scroll_display(Scroll::Bottom);
            }
            TermAction::Search => self.session.search_open.toggle(),
            TermAction::PromptPrev => self.jump_prompt(-1),
            TermAction::PromptNext => self.jump_prompt(1),
            TermAction::SelectAll => {
                let mut term = self.session.terminal.term.lock();
                let history = term.grid().history_size();
                let lines = term.grid().screen_lines();
                let cols = term.grid().columns();
                let start = Point::new(Line(-(history as i32)), Column(0));
                let end = Point::new(Line(lines as i32 - 1), Column(cols - 1));
                term.selection = Some(Selection::new(SelectionType::Simple, start, Side::Left));
                if let Some(sel) = &mut term.selection {
                    sel.update(end, Side::Right);
                }
            }
            TermAction::StartSelection => {
                // Keyboard selection mode: anchor at the cursor's cell;
                // navigation keys extend, Enter copies, Escape cancels.
                let mut term = self.session.terminal.term.lock();
                let cur = term.grid().cursor.point;
                term.selection = Some(Selection::new(SelectionType::Simple, cur, Side::Left));
                if let Some(sel) = &mut term.selection {
                    sel.update(cur, Side::Right);
                }
                drop(term);
                self.keysel = Some((cur, cur));
            }
            TermAction::ScrollToTop => {
                self.session
                    .terminal
                    .term
                    .lock()
                    .scroll_display(Scroll::Top);
            }
            TermAction::ScrollToBottom => {
                self.session
                    .terminal
                    .term
                    .lock()
                    .scroll_display(Scroll::Bottom);
            }
            TermAction::ScrollPageUp => {
                let mut term = self.session.terminal.term.lock();
                // Positive delta moves the viewport up into scrollback —
                // same sign convention as the wheel path.
                let page = term.grid().screen_lines().saturating_sub(1) as i32;
                term.scroll_display(Scroll::Delta(page));
            }
            TermAction::ScrollPageDown => {
                let mut term = self.session.terminal.term.lock();
                let page = term.grid().screen_lines().saturating_sub(1) as i32;
                term.scroll_display(Scroll::Delta(-page));
            }
            TermAction::ScrollLineUp => {
                self.session
                    .terminal
                    .term
                    .lock()
                    .scroll_display(Scroll::Delta(1));
            }
            TermAction::ScrollLineDown => {
                self.session
                    .terminal
                    .term
                    .lock()
                    .scroll_display(Scroll::Delta(-1));
            }
            TermAction::MoveTabLeft => self.app.move_tab(-1),
            TermAction::MoveTabRight => self.app.move_tab(1),
            TermAction::FocusPaneDir { horizontal, forward } => {
                self.app.focus_pane_dir(horizontal, forward);
            }
            TermAction::ResizePane {
                horizontal,
                forward,
                px,
            } => {
                self.app.resize_pane_dir(horizontal, forward, px);
            }
            TermAction::UrlHints => self.url_hints(),
            TermAction::CopyLastOutput => self.copy_last_output(),
            TermAction::OpenScrollbackEditor => self.open_scrollback_editor(),
            TermAction::WriteScreenFile => self.write_screen_file(),
            TermAction::WriteScrollbackFile => self.write_scrollback_file(),
            TermAction::WriteSelectionFile => self.write_selection_file(),
            TermAction::SearchNext => self.search_step(1),
            TermAction::SearchPrev => self.search_step(-1),
            TermAction::Quit => self.app.quit(),
            TermAction::SplitRight => {
                self.app.split_pane(crate::app::SplitDir::Row, self.session.id, false);
            }
            TermAction::SplitDown => {
                self.app
                    .split_pane(crate::app::SplitDir::Column, self.session.id, false);
            }
            TermAction::SplitLeft => {
                self.app.split_pane(crate::app::SplitDir::Row, self.session.id, true);
            }
            TermAction::SplitUp => {
                self.app
                    .split_pane(crate::app::SplitDir::Column, self.session.id, true);
            }
            TermAction::PaneZoom => self.app.toggle_pane_zoom(),
            TermAction::EqualizeSplits => self.app.equalize_splits(),
            TermAction::GotoSplit(index) => self.app.goto_split(index),
            TermAction::ReloadConfig => self.app.reload_config(),
            TermAction::ClipboardReadConfirm => self.clipboard_read_confirm(true),
            TermAction::ClipboardReadDeny => self.clipboard_read_confirm(false),
            TermAction::FocusNextPane => self.app.cycle_pane(1),
            TermAction::FocusPrevPane => self.app.cycle_pane(-1),
            TermAction::Fullscreen => self.app.toggle_fullscreen(),
            TermAction::Palette => self.app.toggle_palette(),
            TermAction::Settings => self.app.toggle_settings(),
        }
    }

    /// Scroll so the next OSC 133 prompt mark sits at the viewport top.
    /// `dir` -1 = previous prompt, +1 = next.
    fn jump_prompt(&mut self, dir: i32) {
        let marks = self.session.terminal.prompt_marks.lock().unwrap();
        if marks.is_empty() {
            return;
        }
        let mut term = self.session.terminal.term.lock();
        let history = term.grid().history_size() as i64;
        let offset = term.grid().display_offset() as i64;
        // Raw row index shown at screen row 0.
        let top = history - offset;
        let target = if dir < 0 {
            marks
                .iter()
                .copied()
                .filter(|&m| m < top)
                .max()
                // At the bottom, every mark is on screen; jump to the one
                // before the prompt the user is typing at.
                .or_else(|| {
                    (offset == 0 && marks.len() >= 2)
                        .then(|| marks[marks.len() - 2])
                })
        } else {
            marks.iter().copied().filter(|&m| m > top).min()
        };
        let Some(abs) = target else { return };
        let want = (history - abs).clamp(0, history) as i32;
        let delta = want - offset as i32;
        if delta != 0 {
            term.scroll_display(Scroll::Delta(delta));
        }
    }

    /// (col, row) the active search match should scroll to center.
    fn search_scroll_target(&self) -> Option<i32> {
        let search = self.search.as_ref()?;
        let line = search.matches.get(search.active)?.first()?.2;
        // Target display_offset so the match sits mid-viewport.
        Some(-line + self.lines as i32 / 2)
    }

    /// Sync local search state with the session bindings: the WaterUI
    /// search bar owns open/close and the query text; the surface owns the
    /// match list and which match is active. Called every frame while open.
    fn sync_search(&mut self) {
        let open = self.session.search_open.snapshot();
        if open != self.search.is_some() {
            self.search = open.then_some(Search {
                query: String::new(),
                matches: Vec::new(),
                active: 0,
                stamp: (0, 0, 0, 0),
            });
        }
        let Some(s) = &self.search else { return };
        let q = self.session.search_query.snapshot().to_string();
        if s.query != q {
            self.search.as_mut().unwrap().query = q;
            self.run_search();
        }
    }

    /// Search the whole buffer for `query`; fills `matches`, scrolls to #1.
    /// Matches are found on logical lines (soft wraps joined) and mapped
    /// back to grid cells of the grid as it is laid out NOW — the stamp
    /// makes a reflow or new output re-run the search.
    fn run_search(&mut self) {
        let generation = self.content_gen.get();
        let Some(search) = &mut self.search else { return };
        let term = self.session.terminal.term.lock();
        let grid = term.grid();
        let (cols, history, lines) = (grid.columns(), grid.history_size(), grid.screen_lines());
        search.stamp = (cols, history, lines, generation);
        search.matches.clear();
        search.active = 0;
        if !search.query.is_empty() {
            let query: Vec<char> = search
                .query
                .chars()
                .flat_map(char::to_lowercase)
                .collect();
            for lm in logical_lines(grid, -(history as i32), lines as i32 - 1) {
                search.matches.extend(line_map_matches(&lm, &query, cols));
            }
        }
        drop(term);
        let n = search.matches.len();
        self.session.search_status.set_from(if search.query.is_empty() {
            String::new()
        } else if n == 0 {
            "no matches".to_string()
        } else {
            format!("{n} matches")
        });
        if let Some(target) = self.search_scroll_target() {
            let cur = self.session.terminal.term.lock().grid().display_offset() as i32;
            let delta = target - cur;
            if delta != 0 {
                self.session
                    .terminal
                    .term
                    .lock()
                    .scroll_display(Scroll::Delta(delta));
            }
        }
    }

    /// Drain events pushed by the parser thread since last frame.
    fn drain_events(&mut self) {
        // Palette-queued actions share the key-chord dispatch path.
        let queued: Vec<TermAction> = self.session.pending_actions.borrow_mut().drain(..).collect();
        for action in queued {
            self.do_action(action);
        }
        let events: Vec<TermEvent> = {
            let rx = self.session.terminal.events.lock().unwrap();
            rx.try_iter().collect()
        };
        for event in events {
            match event {
                TermEvent::Title(title) => {
                    let t = match title.is_empty() {
                        true => Str::from("hydroterm"),
                        false => Str::from(title),
                    };
                    *self.session.base_title.lock().unwrap() = t.clone();
                    // A 🔔 badge holds the title until user attention clears
                    // it — the real title keeps accumulating in base_title.
                    if !*self.session.notify_badge.lock().unwrap() {
                        self.app.set_session_title(self.session.id, t);
                    }
                }
                TermEvent::ClipboardStore(_ty, text) => {
                    // `osc52-write` config (Ghostty clipboard-write): deny
                    // drops program-initiated clipboard writes.
                    if self.app.config(|c| c.osc52_write)
                        && let Some(clip) = self.clipboard.as_mut()
                    {
                        let _ = clip.set_text(&text);
                    }
                }
                TermEvent::ClipboardLoad(_ty, fmt) => {
                    // `clipboard-read` (Ghostty): ask waits on the snackbar
                    // Allow/Enter; deny answers empty so a program can't
                    // slurp the clipboard silently.
                    match self.app.config(|c| c.clipboard_read) {
                        crate::config::ClipboardRead::Allow => {
                            let text = self.clipboard_text();
                            self.write(fmt(&text).into_bytes());
                        }
                        crate::config::ClipboardRead::Ask => {
                            *self.session.pending_clipboard_fmt.borrow_mut() = Some(fmt);
                            self.session.pending_clipboard_read.set(true);
                        }
                        crate::config::ClipboardRead::Deny => {
                            self.write(fmt("").into_bytes());
                        }
                    }
                }
                TermEvent::ColorRequest(index, fmt) => {
                    let rgb = self.palette.borrow().at(index);
                    self.write(fmt(rgb).into_bytes());
                }
                TermEvent::TextAreaSizeRequest(fmt) => {
                    let ws = WindowSize {
                        num_lines: self.lines,
                        num_cols: self.cols,
                        cell_width: self.fonts.metrics.cell_w as u16,
                        cell_height: self.fonts.metrics.cell_h as u16,
                    };
                    self.write(fmt(ws).into_bytes());
                }
                TermEvent::Bell => {
                    if self.app.config(|c| c.visual_bell) {
                        self.bell_at = Some(Instant::now());
                    }
                    if self.app.config(|c| c.audible_bell) {
                        ring_bell(&mut self.bell_ring_at);
                    }
                }
                TermEvent::ChildExit(_status) => {
                    self.session.exited.set(true);
                }
                TermEvent::Exit => {
                    // `wait-after-command`: a `command`/`-e` child keeps
                    // its last frame mounted after exit; the pane only
                    // goes away via an explicit close (Ghostty). Otherwise
                    // the exit closes just this pane — `close_tab` takes a
                    // tab id, not a session id, so route via close_pane
                    // (which removes one split leaf or the whole tab).
                    let hold = self.session.ran_command
                        && self.app.config(|c| c.wait_after_command);
                    if hold {
                        self.session.exited.set(true);
                    } else {
                        self.app.close_pane(self.session.id);
                    }
                }
                TermEvent::Apc(payload, line, col) => {
                    self.handle_apc(&payload, line, col);
                }
                TermEvent::Tap(tap) => match tap {
                    // Marks + the redraw mode are recorded on the reader
                    // thread where the cursor still sits at the mark.
                    TapEvent::PromptStart | TapEvent::ShellRedraw(_) => {}
                    TapEvent::Cwd(path) => {
                        *self.session.cwd.lock().unwrap() = Some(path);
                    }
                    TapEvent::PromptEnd | TapEvent::CommandStart => {}
                    TapEvent::CommandEnd(_code) => {}
                    TapEvent::Notify(title, body) => {
                        // Bell flash + title badge; `desktop-notifications`
                        // gates only the freedesktop notify-send hop.
                        if self.app.config(|c| c.visual_bell) {
                            self.bell_at = Some(Instant::now());
                        }
                        if self.app.config(|c| c.desktop_notifications) {
                            notify_desktop(&title, &body);
                        }
                        let text = if title.is_empty() { body } else { format!("{title}: {body}") };
                        *self.session.notify_badge.lock().unwrap() = true;
                        self.app
                            .set_session_title(self.session.id, Str::from(format!("\u{1f514} {text}")));
                    }
                    TapEvent::Apc(_payload) => {}
                },
            }
        }
    }

    /// kitty graphics: parse + store + reply `\x1b_Gi=<id>;<status>\x1b\\`.
    fn handle_apc(&mut self, payload: &[u8], line: i64, col: usize) {
        let Some(cmd) = crate::kitty::parse(payload) else { return };
        let quiet = cmd.quiet();
        let (id, status) = self
            .session
            .kitty
            .borrow_mut()
            .handle(cmd, line, col);
        if !quiet {
            self.write(format!("\x1b_Gi={id};{status}\x1b\\").into_bytes());
        }
    }

    /// Re-measure fonts when size or family changes (hot reload).
    fn sync_fonts(&mut self) {
        let pref = self.session.font_family.snapshot().to_string();
        if pref != self.family_pref {
            self.family_pref = pref.clone();
            self.fonts.reload_family(&pref);
        }
        let want = self.session.font_size.snapshot();
        if (want - self.font_size_pt).abs() > f32::EPSILON {
            self.font_size_pt = want;
            self.fonts.resize(want);
        }
        let (aw, ah, ab) = self.app.config(|c| {
            (
                c.cell_width_adjust,
                c.cell_height_adjust,
                c.font_baseline_adjust,
            )
        });
        self.fonts.set_cell_adjust(aw, ah);
        self.fonts.set_baseline_adjust(ab);
        self.fonts
            .set_features(&self.app.config(|c| c.font_features.clone()));
    }

    /// Recompute the grid from the logical frame size and propagate resizes.
    fn sync_size(&mut self, width: f32, height: f32) {
        let m = self.fonts.metrics;
        let pad = PADDING * 2.0;
        let cols = ((width - pad) / m.cell_w).floor().max(2.0) as u16;
        let lines = ((height - pad) / m.cell_h).floor().max(1.0) as u16;
        // Degenerate frames (window unmapped/collapsed) must not shrink the
        // PTY — a 1-line winsize breaks apps that read TIOCGWINSZ at start.
        if cols > 2 && lines > 1 && (cols != self.cols || lines != self.lines) {
            self.cols = cols;
            self.lines = lines;
            self.session
                .terminal
                .resize(cols, lines, (m.cell_w as u16, m.cell_h as u16));
            // `resize-overlay`: show the new grid size for a beat after
            // the last change — the Instant decays inside build_scene,
            // same pattern as the bell flash.
            if self.app.config(|c| c.resize_overlay) {
                self.resize_at = Some(Instant::now());
                self.session
                    .resize_label
                    .set(Some(Str::from(format!("{cols}\u{00d7}{lines}"))));
            }
        }
    }

    /// Cursor blink phase for `Instant::now()`.
    fn blink_on(&self) -> bool {
        (self.blink_epoch.elapsed().as_millis() / BLINK_HALF.as_millis()).is_multiple_of(2)
    }

    // -- input handlers ------------------------------------------------------

    fn on_focus(&mut self, gained: bool) {
        self.focused = gained;
        if gained {
            self.app.focus_pane(self.session.id);
        }
        let mode = *self.session.terminal.term.lock().mode();
        if mode.contains(TermMode::FOCUS_IN_OUT) {
            self.write(if gained { b"\x1b[I".to_vec() } else { b"\x1b[O".to_vec() });
        }
    }

    /// User attention on this pane clears a 🔔 notification badge.
    fn clear_notify_badge(&mut self) {
        let mut badge = self.session.notify_badge.lock().unwrap();
        if *badge {
            *badge = false;
            let t = self.session.base_title.lock().unwrap().clone();
            drop(badge);
            self.app.set_session_title(self.session.id, t);
        }
    }

    /// Returns true when the key changed scene or UI state and needs a
    /// frame now (overlay/edit/action paths); a key that only writes bytes
    /// to the PTY returns false — its visible effect is the echoed output,
    /// which the wake pipe draws on arrival.
    fn on_key(&mut self, pressed: bool, key: &Key, code: Code, mods: Modifiers) -> bool {
        if pressed {
            self.clear_notify_badge();
        }
        // Overlay pages capture keys first: palette query, settings
        // commit/close, then the search bar's query.
        if pressed && self.app.palette_open.snapshot() && self.palette_key(key, mods) {
            return true;
        }
        if pressed && self.app.settings_open.snapshot() && self.settings_key(key, mods) {
            return true;
        }
        if pressed && self.search.is_some() && self.search_key(key, mods) {
            return true;
        }
        // Paste-protection overlay captures Enter/Escape; other keys
        // fall through so the pending paste can't swallow input.
        if pressed && self.session.pending_paste.snapshot().is_some() {
            match key {
                Key::Named(NamedKey::Enter) => {
                    self.paste_confirm(true);
                    return true;
                }
                Key::Named(NamedKey::Escape) => {
                    self.paste_confirm(false);
                    return true;
                }
                _ => {}
            }
        }
        // `clipboard-read = ask` prompt: Enter allows, Escape denies.
        if pressed && self.session.pending_clipboard_read.snapshot() {
            match key {
                Key::Named(NamedKey::Enter) => self.clipboard_read_confirm(true),
                Key::Named(NamedKey::Escape) => self.clipboard_read_confirm(false),
                _ => {}
            }
            return true;
        }
        // `confirm-close` snackbar: Enter closes, Escape cancels —
        // swallow everything while it waits so no stray byte hits the
        // program we're about to kill.
        if pressed && self.session.pending_close.snapshot().is_some() {
            match key {
                Key::Named(NamedKey::Enter) => self.app.confirm_close(self.session.id),
                Key::Named(NamedKey::Escape) => self.app.cancel_close_prompt(self.session.id),
                _ => {}
            }
            return true;
        }
        // URL hint mode captures keys: digits/Backspace feed the buffer,
        // Enter opens, Escape cancels — everything else cancels and falls
        // through to normal handling.
        if pressed && self.hints.is_some() && self.hint_key(key) {
            return true;
        }
        // Keyboard selection mode (`start_selection`): navigation keys
        // extend the mark, Enter copies, Escape cancels; any other key
        // exits and falls through to normal handling.
        if pressed && self.keysel.is_some() && self.keysel_key(key) {
            return true;
        }
        let mode = *self.session.terminal.term.lock().mode();
        if pressed {
            // Config keybinds first — they may re-map or disable defaults.
            // `unbind` skips the default chord table too but still falls
            // through to literal bytes — the reference's semantics: an
            // unbound keypress reaches the shell as its raw escape.
            let mut unbound = false;
            match self.app.config(|c| c.lookup_keybind(key, mods)) {
                Some(Some(action)) => {
                    self.do_action(action);
                    return true;
                }
                Some(None) => unbound = true, // explicitly disabled
                None => {}
            }
            if !unbound
                && let Some(action) = action_chord(key, mods).or_else(|| tab_chord(key, code, mods))
            {
                self.do_action(action);
                return true;
            }
            if let Some(bytes) = key_to_bytes(key, code, mods, mode) {
                self.write(bytes);
                self.hide_cursor_on_typing();
                return self.snap_to_bottom_if_scrolled();
            }
            false
        } else if let Some(bytes) = key_release_bytes(key, mods, mode) {
            self.write(bytes);
            false
        } else {
            false
        }
    }

    /// Feed one key into the open command palette. Enter runs the top
    /// match, Escape closes, plain characters edit the query (the surface
    /// keeps keyboard focus — the TextField can't be focused
    /// programmatically, water-rs/hydrolysis#90 — so the binding is driven
    /// from here; a focused field also writes the same binding directly).
    fn palette_key(&mut self, key: &Key, mods: Modifiers) -> bool {
        match key {
            Key::Named(NamedKey::Enter) => {
                let sel = self.app.palette_sel.snapshot().unwrap_or(0);
                self.app.run_palette_at(sel);
                true
            }
            Key::Named(NamedKey::ArrowDown) => {
                let query = self.app.palette_query.snapshot().to_string();
                let n = crate::app::palette_matches(&query).len();
                if n > 0 {
                    self.app.palette_sel.with_mut(|s| {
                        let cur = s.unwrap_or(0);
                        *s = Some((cur + 1).min(n - 1));
                    });
                    self.app
                        .palette_scroll
                        .scroll_to(self.app.palette_sel.snapshot().unwrap_or(0));
                }
                true
            }
            Key::Named(NamedKey::ArrowUp) => {
                self.app.palette_sel.with_mut(|s| {
                    let cur = s.unwrap_or(0);
                    *s = Some(cur.saturating_sub(1));
                });
                self.app
                    .palette_scroll
                    .scroll_to(self.app.palette_sel.snapshot().unwrap_or(0));
                true
            }
            Key::Named(NamedKey::Escape) => {
                self.app.palette_open.set(false);
                true
            }
            Key::Named(NamedKey::Backspace) if mods.is_empty() => {
                let mut q = self.app.palette_query.snapshot().to_string();
                q.pop();
                self.app.palette_query.set_from(q);
                self.app.palette_sel.set(Some(0));
                self.app.palette_scroll.scroll_to(0);
                true
            }
            // Printable keys: consume here — the text itself arrives via
            // `SurfaceInputEvent::TextInput` (see `on_text`). Mod-chords
            // (Ctrl+Shift+P to close, etc.) fall through to the chord path.
            Key::Character(_)
                if !mods.intersects(Modifiers::CONTROL | Modifiers::ALT | Modifiers::META) =>
            {
                true
            }
            _ => false,
        }
    }

    /// Feed one key into search state. Returns true when consumed.
    fn search_key(&mut self, key: &Key, mods: Modifiers) -> bool {
        match key {
            Key::Named(NamedKey::Enter) => {
                // Enter: next match; Shift+Enter: previous.
                self.search_step(if mods.contains(Modifiers::SHIFT) { -1 } else { 1 });
                true
            }
            Key::Named(NamedKey::Escape) => {
                self.session.search_open.set(false);
                true
            }
            Key::Named(NamedKey::Backspace) => {
                let mut q = self.session.search_query.snapshot().to_string();
                q.pop();
                self.session.search_query.set_from(q);
                true
            }
            Key::Named(NamedKey::ArrowUp)
            | Key::Named(NamedKey::ArrowDown)
            | Key::Named(NamedKey::ArrowLeft)
            | Key::Named(NamedKey::ArrowRight)
            | Key::Named(NamedKey::PageUp)
            | Key::Named(NamedKey::PageDown)
            | Key::Named(NamedKey::Tab)
            | Key::Named(NamedKey::F1)
            | Key::Named(NamedKey::F2)
            | Key::Named(NamedKey::F3)
            | Key::Named(NamedKey::F4)
            | Key::Named(NamedKey::F5)
            | Key::Named(NamedKey::F6)
            | Key::Named(NamedKey::F7)
            | Key::Named(NamedKey::F8)
            | Key::Named(NamedKey::F9)
            | Key::Named(NamedKey::F10)
            | Key::Named(NamedKey::F11)
            | Key::Named(NamedKey::F12) => false,
            // Printable keys append to the query — TextInput also arrives,
            // so here we only consume to suppress the PTY path. Mod-chords
            // (Ctrl+Shift+P, Ctrl+C, …) fall through to the chord path.
            Key::Character(_)
                if !mods.intersects(Modifiers::CONTROL | Modifiers::ALT | Modifiers::META) =>
            {
                true
            }
            _ => false,
        }
    }

    /// Feed one key into the open settings page: Enter applies, Escape
    /// closes, printable keys are swallowed (edits go through the
    /// controls' own focus/pointer path).
    fn settings_key(&mut self, key: &Key, mods: Modifiers) -> bool {
        match key {
            Key::Named(NamedKey::Enter) => {
                self.app.apply_settings();
                true
            }
            Key::Named(NamedKey::Escape) => {
                self.app.settings_open.set(false);
                true
            }
            // Printable keys: swallowed (controls own their own editing);
            // mod-chords like Ctrl+Shift+, fall through to close/toggle.
            Key::Character(_)
                if !mods.intersects(Modifiers::CONTROL | Modifiers::ALT | Modifiers::META) =>
            {
                true
            }
            _ => false,
        }
    }

    /// `mouse-hide-while-typing`: hide the pointer on real typed input.
    /// The hider connects lazily (X11 only) and the toggle is live.
    fn hide_cursor_on_typing(&mut self) {
        if !self.app.config(|c| c.mouse_hide_typing) {
            return;
        }
        if !self.cursor_hider_tried {
            self.cursor_hider_tried = true;
            self.cursor_hider = crate::xcursor::CursorHider::new();
        }
        if let Some(h) = &mut self.cursor_hider {
            h.hide();
        }
    }

    /// Append typed text — palette query while open, search query when
    /// searching, else PTY. Returns true when the text changed scene or
    /// UI state (needs a frame); PTY passthrough returns false.
    fn on_text(&mut self, text: &str) -> bool {
        if self.app.settings_open.snapshot() {
            return true;
        }
        if self.app.palette_open.snapshot() {
            let mut q = self.app.palette_query.snapshot().to_string();
            q.push_str(text);
            self.app.palette_query.set_from(q);
            self.app.palette_sel.set(Some(0));
            self.app.palette_scroll.scroll_to(0);
            return true;
        }
        if self.search.is_some() {
            let mut q = self.session.search_query.snapshot().to_string();
            q.push_str(text);
            self.session.search_query.set_from(q);
            return true;
        }
        // Text arriving while a paste is pending is the user's real
        // input — the overlay is modal on the paste, not on typing.
        if self.session.pending_paste.snapshot().is_some()
            || self.session.pending_close.snapshot().is_some()
        {
            return true;
        }
        // Hint-mode digits are already consumed by `hint_key`'s Character
        // arm — swallow the paired TextInput so nothing reaches the PTY.
        if self.hints.is_some() {
            return true;
        }
        self.write(text.as_bytes().to_vec());
        self.hide_cursor_on_typing();
        self.snap_to_bottom_if_scrolled()
    }

    /// Keyboard input bound for the PTY snaps the viewport back to the
    /// live edge — the alacritty/kitty convention; `scroll-on-input =
    /// false` leaves the viewport where the user scrolled it.
    /// Returns true when the viewport moved (needs a frame).
    fn snap_to_bottom_if_scrolled(&mut self) -> bool {
        if !self.app.config(|c| c.scroll_on_input) {
            return false;
        }
        self.snap_viewport_to_bottom()
    }

    /// Paste-like interactions (`paste_text` — menu paste, Shift+Insert,
    /// middle-click PRIMARY, drop, confirmed paste) snap the viewport to
    /// the cursor under `scroll-to-cursor` — a separate gate from
    /// `scroll-on-input`, which covers key bytes only.
    fn snap_to_cursor_if_scrolled(&mut self) -> bool {
        if !self.app.config(|c| c.scroll_to_cursor) {
            return false;
        }
        self.snap_viewport_to_bottom()
    }

    /// Shared snap: scroll to the live edge if the user has scrolled
    /// back. Returns true when the viewport moved (needs a frame).
    fn snap_viewport_to_bottom(&mut self) -> bool {
        let mut term = self.session.terminal.term.lock();
        if term.grid().display_offset() == 0 {
            return false;
        }
        term.scroll_display(Scroll::Bottom);
        true
    }

    fn on_pointer_move(&mut self, x: f64, y: f64) {
        // `focus-follows-mouse`: entering this pane records it as the
        // tab's focused split (app-level record — GUI key focus still
        // needs a press until hydrolysis#126).
        if self.app.config(|c| c.focus_follows_mouse) {
            self.app.focus_pane(self.session.id);
        }
        let (col, row) = self.viewport_cell(x, y);
        let mode = *self.session.terminal.term.lock().mode();

        // `mouse-shift-override`: Shift+click/drag selects even while the
        // program owns the mouse (Ghostty default true).
        let shift_override =
            self.modifiers.contains(Modifiers::SHIFT)
                && self.app.config(|c| c.mouse_shift_override);
        if mode.intersects(TermMode::MOUSE_MODE) && !shift_override {
            // Drag while held, else any-cell motion tracking (1003) isn't in
            // TermMode — only report while a button is held (button-motion).
            if let Some(button) = self.held_button {
                if let Some(bytes) = mouse::encode(
                    MouseAction::Drag(button),
                    CellPos { col, row },
                    self.modifiers,
                    mode,
                ) {
                    self.write(bytes);
                }
            } else if let Some(bytes) = mouse::encode(
                MouseAction::Motion,
                CellPos { col, row },
                self.modifiers,
                mode,
            ) {
                self.write(bytes);
            }
        }

        self.update_hover(x, y);

        if self.selecting {
            let point = self.grid_point(x, y);
            let side = self.cell_side(x);
            let mut term = self.session.terminal.term.lock();
            if let Some(sel) = &mut term.selection {
                sel.update(point, side);
            }
        }
    }

    /// The `open-link-modifier` config as a `Modifiers` flag.
    fn link_modifier(&self) -> Modifiers {
        match self.app.config(|c| c.open_link_modifier) {
            crate::config::LinkMod::Ctrl => Modifiers::CONTROL,
            crate::config::LinkMod::Shift => Modifiers::SHIFT,
            crate::config::LinkMod::Alt => Modifiers::ALT,
            crate::config::LinkMod::Super => Modifiers::META,
        }
    }

    /// Link-hover affordance: while the `open-link-modifier` is held,
    /// underline the link under the pointer and switch the cursor to a
    /// pointing hand. Re-runs on pointer moves and on modifier-chord
    /// changes.
    fn update_hover(&mut self, x: f64, y: f64) {
        self.pointer_at = (x, y);
        let segs = if self.modifiers.contains(self.link_modifier()) {
            self.link_span_at(self.grid_point(x, y))
        } else {
            Vec::new()
        };
        if segs != self.hover_link {
            self.hover_link = segs;
        }
        let cursor = if self.hover_link.is_empty() {
            CursorStyle::IBeam
        } else {
            CursorStyle::PointingHand
        };
        if self.hover_cursor.snapshot() != cursor {
            self.hover_cursor.set(cursor);
        }
    }

    /// Link span under `point` for the Ctrl+hover affordance: the OSC8
    /// run with the same uri on this row first, else the plain-text URL
    /// on the logical line mapped back to viewport segments.
    fn link_span_at(&self, point: Point) -> Vec<(usize, usize, usize)> {
        let term = self.session.terminal.term.lock();
        let grid = term.grid();
        if let Some(uri) = grid[point].hyperlink().map(|h| h.uri().to_string()) {
            let line = point.line.0;
            let same = |c: usize| {
                grid[Point::new(Line(line), Column(c))]
                    .hyperlink()
                    .is_some_and(|h| h.uri() == uri)
            };
            let mut c0 = point.column.0;
            let mut c1 = c0 + 1;
            while c0 > 0 && same(c0 - 1) {
                c0 -= 1;
            }
            while c1 < grid.columns() && same(c1) {
                c1 += 1;
            }
            let row = line + grid.display_offset() as i32;
            return if row >= 0 {
                vec![(c0, c1, row as usize)]
            } else {
                Vec::new()
            };
        }
        let lm = logical_line_at(grid, point.line.0);
        let cols = grid.columns();
        let offset = grid.display_offset() as i32;
        let screen = grid.screen_lines() as i32;
        let Some(idx) = lm
            .marks
            .iter()
            .rfind(|m| m.1 == point.line.0 && m.2 <= point.column.0)
            .map(|m| m.0)
        else {
            return Vec::new();
        };
        for (s, e) in url_spans(&lm.chars) {
            if idx >= s && idx < e {
                return span_segments(&lm, s, e, cols)
                    .into_iter()
                    .filter_map(|(c0, c1, l)| {
                        let r = l + offset;
                        (r >= 0 && r < screen).then_some((c0, c1, r as usize))
                    })
                    .collect();
            }
        }
        Vec::new()
    }

    /// Drag-and-drop: paste the dropped item's path into this pane
    /// (Ghostty/kitty drop behaviour — `file://` URIs decoded, each path
    /// shell-quoted so it lands as one argument).
    pub fn drop_text(&mut self, text: &str) {
        let out = drop_payload(text);
        if out.is_empty() {
            return;
        }
        let bracketed = self
            .session
            .terminal
            .term
            .lock()
            .mode()
            .contains(TermMode::BRACKETED_PASTE);
        self.paste_text(&out, bracketed);
    }

    fn on_pointer_button(&mut self, pressed: bool, button: SurfacePointerButton, x: f64, y: f64) {
        let (col, row) = self.viewport_cell(x, y);
        let mode = *self.session.terminal.term.lock().mode();

        // `mouse-shift-override`: Shift+click/drag selects even while the
        // program owns the mouse (Ghostty default true).
        let shift_override =
            self.modifiers.contains(Modifiers::SHIFT)
                && self.app.config(|c| c.mouse_shift_override);
        if mode.intersects(TermMode::MOUSE_MODE) && !shift_override {
            let action = if pressed {
                let b = mouse::press_button(button);
                self.held_button = Some(b);
                MouseAction::Press(b)
            } else {
                self.held_button = None;
                MouseAction::Release
            };
            if let Some(bytes) = mouse::encode(action, CellPos { col, row }, self.modifiers, mode) {
                self.write(bytes);
            }
            return;
        }

        if pressed {
            self.clear_notify_badge();
            match button {
                SurfacePointerButton::Primary => {
                    // <open-link-modifier>+click opens a link.
                    if self.modifiers.contains(self.link_modifier())
                        && self.open_link_at(self.grid_point(x, y))
                    {
                        return;
                    }
                    // Alt+drag = block selection; otherwise multi-click by timing.
                    let now = Instant::now();
                    let count = self
                        .last_click
                        .filter(|(t, r, c, _)| {
                            now.duration_since(*t)
                                < Duration::from_millis(
                                    self.app.config(|c| c.click_interval),
                                )
                                && r.abs_diff(row) <= MULTI_CLICK_RANGE
                                && c.abs_diff(col) <= MULTI_CLICK_RANGE
                        })
                        .map(|(_, _, _, n)| n + 1)
                        .unwrap_or(1);
                    self.click_count_reset(count);
                    let ty = if self.modifiers.contains(Modifiers::ALT) {
                        SelectionType::Block
                    } else {
                        match count {
                            2 => SelectionType::Semantic,
                            3.. => SelectionType::Lines,
                            _ => SelectionType::Simple,
                        }
                    };
                    let point = self.grid_point(x, y);
                    let side = self.cell_side(x);
                    let mut term = self.session.terminal.term.lock();
                    term.selection = Some(Selection::new(ty, point, side));
                    drop(term);
                    self.selecting = true;
                    self.last_click = Some((now, row, col, count));
                }
                SurfacePointerButton::Middle => {
                    // Middle click pastes PRIMARY on Linux (xterm
                    // convention); CLIPBOARD when PRIMARY is empty or
                    // the compositor does not offer it.
                    match self.primary_text() {
                        Some(text) => {
                            let bracketed = self
                                .session
                                .terminal
                                .term
                                .lock()
                                .mode()
                                .contains(TermMode::BRACKETED_PASTE);
                            self.paste_text(&text, bracketed);
                        }
                        None => self.paste_clipboard(),
                    }
                }
                SurfacePointerButton::Secondary
                    if self.session.terminal.term.lock().selection.is_some() =>
                {
                    // Right-click: extend an existing selection like xterm does.
                    let point = self.grid_point(x, y);
                    let mut term = self.session.terminal.term.lock();
                    if let Some(sel) = &mut term.selection {
                        sel.update(point, self.cell_side(x));
                    }
                }
                _ => {}
            }
        } else if button == SurfacePointerButton::Primary {
            self.selecting = false;
            let mut term = self.session.terminal.term.lock();
            if term.selection.as_ref().is_some_and(|s| s.is_empty()) {
                // Click without drag (tiny movement) clears the selection.
                term.selection = None;
            } else {
                let mode = self.app.config(|c| c.copy_on_select);
                if mode != crate::config::CopyOnSelect::Disabled {
                    drop(term);
                    self.copy_on_select(mode);
                }
            }
        }
    }

    fn click_count_reset(&mut self, count: u8) {
        // Beyond triple-click the count wraps back to a simple drag.
        if count > 3 {
            self.last_click = None;
        }
    }

    /// URL hint mode — chips over every visible link, digits + Enter open.
    ///
    /// Numbered spans are collected at activation time; any further input
    /// or scroll clears the mode (the grid may have moved).
    fn url_hints(&mut self) {
        let term = self.session.terminal.term.lock();
        let grid = term.grid();
        let (cols, history, lines) = (grid.columns(), grid.history_size(), grid.screen_lines());
        let offset = grid.display_offset() as i32;
        let screen = lines as i32;
        let mut spans: Vec<HintSpan> = Vec::new();
        let mut urls: Vec<String> = Vec::new();
        // URLs are detected on logical lines (soft wraps joined) so a
        // link spanning a wrap is one span, then mapped back to cells —
        // the badge anchors on its first visible row part.
        'outer: for lm in logical_lines(grid, -(history as i32), lines as i32 - 1) {
            for (s, e) in url_spans(&lm.chars) {
                let segments: Vec<(usize, usize, usize)> = span_segments(&lm, s, e, cols)
                    .into_iter()
                    .filter_map(|(c0, c1, l)| {
                        let r = l + offset;
                        (r >= 0 && r < screen).then_some((c0, c1, r as usize))
                    })
                    .collect();
                if segments.is_empty() {
                    continue;
                }
                urls.push(lm.chars[s..e].iter().collect());
                spans.push(HintSpan {
                    segments,
                    label: spans.len() + 1,
                });
                if spans.len() >= 99 {
                    break 'outer;
                }
            }
        }
        drop(term);
        if spans.is_empty() {
            self.hints = None;
        } else {
            self.hints = Some(HintState { spans, urls, digits: String::new() });
        }
    }

    /// Keyboard selection mode (`start_selection`): arrows/Home/End/
    /// PageUp/PageDown move the mark's end cell-wise; Enter copies and
    /// clears; Escape cancels. Any other key exits mode and falls
    /// through to normal key handling (returns false).
    fn keysel_key(&mut self, key: &Key) -> bool {
        let Some((anchor, mut cur)) = self.keysel else {
            return false;
        };
        let mut term = self.session.terminal.term.lock();
        let history = term.grid().history_size() as i32;
        let rows = term.grid().screen_lines() as i32;
        let cols = term.grid().columns();
        match key {
            Key::Named(NamedKey::Escape) => {
                term.selection = None;
                self.keysel = None;
                return true;
            }
            Key::Named(NamedKey::Enter) => {
                drop(term);
                self.copy_selection();
                self.session.terminal.term.lock().selection = None;
                self.keysel = None;
                return true;
            }
            Key::Named(NamedKey::ArrowLeft) => cur.column = Column(cur.column.0.saturating_sub(1)),
            Key::Named(NamedKey::ArrowRight) => {
                cur.column = Column((cur.column.0 + 1).min(cols - 1))
            }
            Key::Named(NamedKey::ArrowUp) => {
                cur.line = Line((cur.line.0 - 1).max(-history))
            }
            Key::Named(NamedKey::ArrowDown) => {
                cur.line = Line((cur.line.0 + 1).min(rows - 1))
            }
            Key::Named(NamedKey::Home) => cur.column = Column(0),
            Key::Named(NamedKey::End) => cur.column = Column(cols - 1),
            Key::Named(NamedKey::PageUp) => {
                cur.line = Line((cur.line.0 - rows).max(-history))
            }
            Key::Named(NamedKey::PageDown) => {
                cur.line = Line((cur.line.0 + rows).min(rows - 1))
            }
            // Any other key exits the mode and falls through.
            _ => {
                self.keysel = None;
                return false;
            }
        }
        // Rebuild each move like a pointer drag past the anchor: the
        // earlier point is the left end, the later one the right end.
        let (start, end) = if (cur.line.0, cur.column.0) < (anchor.line.0, anchor.column.0) {
            (cur, anchor)
        } else {
            (anchor, cur)
        };
        let mut sel = Selection::new(SelectionType::Simple, start, Side::Left);
        sel.update(end, Side::Right);
        term.selection = Some(sel);
        self.keysel = Some((anchor, cur));
        true
    }

    /// Keys while hint mode is active. Returns false when the key should
    /// fall through to normal handling (mode was cleared).
    fn hint_key(&mut self, key: &Key) -> bool {
        match key {
            Key::Named(NamedKey::Escape) => self.hints = None,
            Key::Named(NamedKey::Enter) => {
                if let Some(h) = self.hints.take()
                    && let Ok(n) = h.digits.parse::<usize>()
                    && let Some(url) = h.urls.get(n.wrapping_sub(1))
                    && n >= 1
                {
                    self.open_uri(url);
                }
            }
            Key::Named(NamedKey::Backspace) => {
                if let Some(h) = &mut self.hints {
                    h.digits.pop();
                }
            }
            Key::Character(t) if t.chars().all(|c| c.is_ascii_digit()) => {
                if let Some(h) = &mut self.hints {
                    h.digits.push_str(t.as_str());
                }
            }
            // Anything else cancels the mode and falls through.
            _ => {
                self.hints = None;
                return false;
            }
        }
        true
    }

    /// Copy the output of the last command — the rows between its
    /// OSC 133 `C` and `D` marks (`last_output`, reader-thread recorded).
    fn copy_last_output(&mut self) {
        let span = *self.session.terminal.last_output.lock().unwrap();
        let Some((start_abs, end_abs)) = span else { return };
        let mut term = self.session.terminal.term.lock();
        let history = term.grid().history_size() as i64;
        // A still-running command has no `D` — copy to the live bottom.
        let end_abs = end_abs.unwrap_or(history + term.grid().screen_lines() as i64 - 1);
        if end_abs < start_abs {
            return;
        }
        let saved = term.selection.take();
        let start = Point::new(
            Line((start_abs - history) as i32),
            Column(0),
        );
        let last_col = term.grid().columns() - 1;
        let mut sel = Selection::new(SelectionType::Simple, start, Side::Left);
        sel.update(
            Point::new(Line((end_abs - history) as i32), Column(last_col)),
            Side::Right,
        );
        term.selection = Some(sel);
        let text = term.selection_to_string();
        term.selection = saved;
        drop(term);
        if let (Some(text), Some(clip)) = (text, self.clipboard.as_mut())
            && !text.is_empty()
        {
            let _ = clip.set_text(&text);
        }
    }

    /// Dump scrollback + screen to a temp file and open `$VISUAL`/`$EDITOR`
    /// on it in a fresh tab (kitty `scrollback_pager`/WezTerm parity).
    fn open_scrollback_editor(&mut self) {
        let text = {
            let term = self.session.terminal.term.lock();
            let grid = term.grid();
            let history = grid.history_size() as i32;
            term.bounds_to_string(
                Point::new(Line(-history), Column(0)),
                Point::new(Line(grid.screen_lines() as i32 - 1), grid.last_column()),
            )
        };
        self.write_buffer_to_editor(text, "scrollback");
    }

    /// `write_screen_file` — the visible viewport only (no scrollback).
    fn write_screen_file(&mut self) {
        let text = {
            let term = self.session.terminal.term.lock();
            let grid = term.grid();
            term.bounds_to_string(
                Point::new(Line(0), Column(0)),
                Point::new(Line(grid.screen_lines() as i32 - 1), grid.last_column()),
            )
        };
        self.write_buffer_to_editor(text, "screen");
    }

    /// `write_scrollback_file` — scrollback + screen (same region as the
    /// scrollback editor; Ghostty keeps the editor-less name).
    fn write_scrollback_file(&mut self) {
        self.open_scrollback_editor();
    }

    /// `write_selection_file` — the current selection's text.
    fn write_selection_file(&mut self) {
        let text = self
            .session
            .terminal
            .term
            .lock()
            .selection_to_string()
            .unwrap_or_default();
        self.write_buffer_to_editor(text, "selection");
    }

    /// Dump `text` to a temp file and open it in `$VISUAL`/`$EDITOR`
    /// inside a new tab (kitty/Ghostty write-file shape).
    fn write_buffer_to_editor(&mut self, text: String, kind: &str) {
        if text.trim().is_empty() {
            return;
        }
        let path = std::env::temp_dir().join(format!(
            "hydroterm-{kind}-{}-{}.txt",
            std::process::id(),
            self.session.id
        ));
        if std::fs::write(&path, text).is_err() {
            return;
        }
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".to_string());
        let mut cmd: Vec<String> = editor.split_whitespace().map(str::to_string).collect();
        cmd.push(path.to_string_lossy().into_owned());
        self.app.new_tab_command(cmd);
    }

    /// Step the search-match cursor by `dir` (+1 next, -1 prev), wrapping.
    fn search_step(&mut self, dir: i32) {
        if let Some(s) = &mut self.search
            && !s.matches.is_empty()
        {
            let len = s.matches.len() as i32;
            s.active = ((s.active as i32 + dir).rem_euclid(len)) as usize;
        }
        if let Some(target) = self.search_scroll_target() {
            let cur = self.session.terminal.term.lock().grid().display_offset() as i32;
            self.session
                .terminal
                .term
                .lock()
                .scroll_display(Scroll::Delta(target - cur));
        }
    }

    fn on_scroll(&mut self, x: f64, y: f64, _dx: f64, dy: f64, unit: ScrollUnit) {
        let (col, row) = self.viewport_cell(x, y);
        let mode = *self.session.terminal.term.lock().mode();

        let lines_delta = match unit {
            ScrollUnit::Line => dy,
            ScrollUnit::Pixel => {
                let h = self.fonts.metrics.cell_h as f64;
                self.scroll_accum_px += dy;
                let lines = (self.scroll_accum_px / h).trunc();
                self.scroll_accum_px -= lines * h;
                lines
            }
        } * f64::from(self.app.config(|c| c.mouse_scroll_multiplier));

        if mode.intersects(TermMode::MOUSE_MODE) {
            if let Some((btn, count)) = mouse::wheel_button(lines_delta) {
                for _ in 0..count {
                    if let Some(bytes) =
                        mouse::encode(MouseAction::Wheel(btn), CellPos { col, row }, self.modifiers, mode)
                    {
                        self.write(bytes);
                    }
                }
            }
            return;
        }

        // Alternate scroll: in the alt screen send arrow keys instead.
        if mode.contains(TermMode::ALTERNATE_SCROLL) && mode.contains(TermMode::ALT_SCREEN) {
            let key = if dy > 0.0 { b"\x1bOA" } else { b"\x1bOB" };
            let app_cursor = mode.contains(TermMode::APP_CURSOR);
            let seq: &[u8] = if !app_cursor && dy > 0.0 {
                b"\x1b[A"
            } else if !app_cursor {
                b"\x1b[B"
            } else {
                key
            };
            for _ in 0..lines_delta.abs().max(1.0) as usize {
                self.write(seq.to_vec());
            }
            return;
        }

        self.hints = None; // viewport moved — stale chips would mislead
        let mut term = self.session.terminal.term.lock();
        term.scroll_display(Scroll::Delta(lines_delta as i32));
    }

    // -- scene plumbing -------------------------------------------------------

    /// Draw the terminal into the frame's scene.
    fn build(&mut self, scene: &mut dyn Scene2D, width: f32, height: f32) {
        {
            // Grid fingerprint for the frame — the open search re-runs
            // when reflow (resize/font zoom) or new output moved its
            // matches. The surface's mouse-reporting flag follows the
            // same grid state so the context menu yields to DECSET
            // 1000/1002/1006 programs.
            let term = self.session.terminal.term.lock();
            let grid = term.grid();
            let stamp = (
                grid.columns(),
                grid.history_size(),
                grid.screen_lines(),
                self.content_gen.get(),
            );
            let reporting = term.mode().intersects(TermMode::MOUSE_MODE);
            drop(term);
            if self.session.mouse_reporting.snapshot() != reporting {
                self.session.mouse_reporting.set(reporting);
            }
            if self.search.as_ref().is_some_and(|s| s.stamp != stamp) {
                self.run_search();
            }
        }
        let matches_view: Vec<(usize, usize, usize)> = self
            .search
            .as_ref()
            .map(|s| {
                let offset = self.session.terminal.term.lock().grid().display_offset() as i32;
                s.matches
                    .iter()
                    .flatten()
                    .map(|&(c0, c1, line)| (c0, c1, (line + offset) as usize))
                    .filter(|&(_, _, r)| r < self.lines as usize)
                    .collect()
            })
            .unwrap_or_default();
        let active: Vec<(usize, usize, usize)> = self
            .search
            .as_ref()
            .and_then(|s| s.matches.get(s.active))
            .map(|segs| {
                let offset = self.session.terminal.term.lock().grid().display_offset() as i32;
                segs.iter()
                    .map(|&(c0, c1, line)| (c0, c1, (line + offset) as usize))
                    .filter(|&(_, _, r)| r < self.lines as usize)
                    .collect()
            })
            .unwrap_or_default();
        let preedit = self.preedit.clone();

        let term = self.session.terminal.term.lock();
        let grid = term.grid();
        let scroll = ScrollInfo {
            display_offset: grid.display_offset(),
            history_size: grid.history_size(),
            screen_lines: grid.screen_lines(),
        };
        let palette = self.palette.borrow();
        let bell_alpha = bell_flash_alpha(self.bell_at, Instant::now());

        let hint_spans: Vec<HintSpan> = self
            .hints
            .as_ref()
            .map(|h| h.spans.clone())
            .unwrap_or_default();
        let hint_digits = self
            .hints
            .as_ref()
            .map(|h| h.digits.clone())
            .unwrap_or_default();
        let blink_on = self.blink_on();
        let focused = self.focused;
        let cursor_invert_fg_bg = self.app.config(|c| c.cursor_invert_fg_bg);
        let bg_opacity = self.app.config(|c| c.background_opacity);
        let bg_image = self.bg_image_draw(f64::from(width), f64::from(height));
        let mut ctx = DrawContext {
            palette: &palette,
            fonts: &mut self.fonts,
            width,
            height,
            blink_on,
            focused,
            cursor_invert_fg_bg,
            cursor_text: self.app.config(|c| c.cursor_text),
            cursor_opacity: self.app.config(|c| c.cursor_opacity),
            preedit,
            scroll,
            search_matches: &matches_view,
            search_active: &active,
            bell_flash: bell_alpha,
            bg_opacity,
            hints: &hint_spans,
            hint_digits: &hint_digits,
            bold_bright: self.app.config(|c| c.bold_is_bright),
            min_contrast: self.app.config(|c| c.minimum_contrast),
            selection_invert: self.app.config(|c| c.selection_invert),
            hover_link: &self.hover_link,
            bg_image,
            unfocused_fill: self.app.config(|c| c.unfocused_split_fill),
        };
        let top = scroll.history_size as i64 - scroll.display_offset as i64;
        let m = ctx.fonts.metrics;
        let pad = PADDING;
        let draw_img = |scene: &mut dyn Scene2D, img: &crate::kitty::KittyImage| {
            let row = img.line - top;
            let rows = if img.rows > 0 {
                img.rows as i64
            } else {
                (img.px_h as f32 / m.cell_h).ceil() as i64
            };
            if row + rows < 0 || row >= self.lines as i64 {
                return;
            }
            let x = pad + img.col as f32 * m.cell_w;
            let y = pad + row as f32 * m.cell_h;
            let w = if img.cols > 0 {
                img.cols as f32 * m.cell_w
            } else {
                img.px_w as f32
            };
            let h = if img.rows > 0 {
                img.rows as f32 * m.cell_h
            } else {
                img.px_h as f32
            };
            let transform = kurbo::Affine::translate((x as f64, y as f64))
                * kurbo::Affine::scale_non_uniform(
                    w as f64 / img.px_w as f64,
                    h as f64 / img.px_h as f64,
                );
            scene.draw_image(&img.brush, transform);
        };
        // z<0 images draw over cell backgrounds but below the text layer.
        let images = self.session.kitty.borrow();
        scene::draw_term(scene, &term, &mut ctx, &mut |scene| {
            for img in images.images.iter().filter(|i| i.z < 0) {
                draw_img(scene, img);
            }
        });
        for img in images.images.iter().filter(|i| i.z >= 0) {
            draw_img(scene, img);
        }
    }
}

impl Drop for TermSurface {
    fn drop(&mut self) {
        // Park the wake future: mark dead, then post one last ping so it
        // exits instead of hanging on the channel forever.
        self.wake_alive.set(false);
        if let Some(tx) = &self.wake_tx {
            let _ = tx.try_send(());
        }
        self.session.terminal.proxy.set_wake(|| {});
    }
}

/// Input-throughput instrumentation, enabled by `HYDROTERM_INPUT_STATS=1`.
/// Counts `SurfaceInputEvent` deliveries and `Key` deliveries between
/// consecutive `build_scene` calls, per-key inter-arrival gaps, and draw
/// duration — the three numbers needed to attribute event-thread stalls
/// to the app or to the framework's redraw scheduling.
pub(crate) fn input_stats() -> bool {
    static ONCE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ONCE.get_or_init(|| std::env::var_os("HYDROTERM_INPUT_STATS").is_some())
}

/// Events delivered since the last draw (any `SurfaceInputEvent`).
static STAT_EVENTS: AtomicU64 = AtomicU64::new(0);
/// Key presses delivered since the last draw.
static STAT_KEYS: AtomicU64 = AtomicU64::new(0);
/// Timestamp of the previous key delivery, for inter-key gaps.
static LAST_KEY_AT: Mutex<Option<Instant>> = Mutex::new(None);

impl SceneContent for TermSurface {
    fn build_scene(&mut self, scene: &mut dyn Scene2D, width: f32, height: f32) -> bool {
        let draw_start = Instant::now();
        self.app.poll_config();
        self.drain_events();
        // Rejoin ZWJ-split scalars before the frame is measured or drawn.
        crate::terminal::fixup_graphemes(&mut self.session.terminal.term.lock());
        self.sync_search();
        self.sync_fonts();
        self.sync_size(width, height);
        self.session.pane_px.set((width, height));

        // A blinking cursor or live bell flash needs the next frame anyway;
        // the wake pipe covers PTY output between frames.
        let cursor_blinking = self.session.terminal.term.lock().cursor_style().blinking;
        let bell_live = self
            .bell_at
            .is_some_and(|t| t.elapsed() < Duration::from_secs_f32(BELL_FLASH_SECS));
        // The resize badge stays up for RESIZE_LABEL_MS after the last
        // size change, then clears itself.
        let resize_live = self
            .resize_at
            .is_some_and(|t| t.elapsed() < RESIZE_LABEL_MS);
        if !resize_live && self.resize_at.is_some() {
            self.resize_at = None;
            self.session.resize_label.set(None);
        }

        self.build(scene, width, height);

        if input_stats() {
            let keys = STAT_KEYS.swap(0, Ordering::Relaxed);
            let events = STAT_EVENTS.swap(0, Ordering::Relaxed);
            let ms = draw_start.elapsed().as_secs_f64() * 1000.0;
            eprintln!("istats draw keys={keys} events={events} draw_ms={ms:.1}");
        }

        cursor_blinking || bell_live || resize_live || self.search.is_some()
    }

    fn set_invalidator(&mut self, invalidator: Option<SceneInvalidator>) {
        self.invalidator = invalidator.clone();
        match invalidator {
            Some(invalidator) => {
                // One channel per surface: the parser thread's wake callback
                // does a `try_send` (the closure must be Send+Sync), and this
                // future — running on the winit main thread via the local
                // executor — drains it and calls the real invalidator.
                let (tx, rx) = async_channel::unbounded::<()>();
                self.session
                    .terminal
                    .proxy
                    .set_wake({
                        let tx = tx.clone();
                        move || {
                            let _ = tx.try_send(());
                        }
                    });
                self.wake_alive.set(true);
                // Events queued before this install (a fast child exit,
                // a clipboard read, ...) arrived while `wake` was a
                // no-op — self-poke once so they drain on the next frame
                // instead of sitting in the queue until a repaint.
                let _ = tx.try_send(());
                let alive = Rc::clone(&self.wake_alive);
                let content_gen = Rc::clone(&self.content_gen);
                let app = self.app.clone();
                let session_id = self.session.id;
                self.wake_task = Some(Box::pin(spawn_local(async move {
                    while rx.recv().await.is_ok() {
                        if !alive.get() {
                            break;
                        }
                        // Coalesce bursts: one invalidation per batch.
                        while rx.try_recv().is_ok() {}
                        content_gen.set(content_gen.get() + 1);
                        // `tab-activity`: a parser wake means new output —
                        // dot the owning tab if it is not selected.
                        app.note_activity(session_id);
                        invalidator();
                    }
                })));
                self.wake_tx = Some(tx);
            }
            None => {
                self.wake_alive.set(false);
                if let Some(tx) = &self.wake_tx {
                    let _ = tx.try_send(());
                }
                self.wake_tx = None;
                self.wake_task = None;
                self.session.terminal.proxy.set_wake(|| {});
            }
        }
    }

    fn wants_input_events(&self) -> bool {
        true
    }

    fn input(&mut self, event: &SurfaceInputEvent) {
        if std::env::var_os("HYDROTERM_DEBUG_INPUT").is_some() {
            eprintln!("[input s{} @{self:p}] {event:?}", self.session.id);
        }
        if input_stats() {
            STAT_EVENTS.fetch_add(1, Ordering::Relaxed);
            if let SurfaceInputEvent::Key { pressed: true, .. } = event {
                let n = STAT_KEYS.fetch_add(1, Ordering::Relaxed) + 1;
                let now = Instant::now();
                if let Some(prev) = LAST_KEY_AT.lock().unwrap().replace(now) {
                    let gap = (now - prev).as_secs_f64() * 1000.0;
                    eprintln!("istats key #{n} gap_ms={gap:.1}");
                }
            }
        }
        // A frame is only requested when the handler changed what the
        // scene draws. Keystrokes that just write bytes to the PTY need
        // no invalidation — the echoed output repaints through the wake
        // pipe anyway, and invalidating here used to cost a whole frame
        // per keystroke on the single winit event thread before the echo
        // even arrived.
        let needs_frame = match event {
            SurfaceInputEvent::Focus(gained) => {
                self.on_focus(*gained);
                true
            }
            SurfaceInputEvent::Modifiers(mods) => {
                self.modifiers = *mods;
                // Ctrl toggles the link-hover affordance without a move.
                let (x, y) = self.pointer_at;
                self.update_hover(x, y);
                false
            }
            SurfaceInputEvent::PointerMove { position } => {
                if let Some(h) = &mut self.cursor_hider {
                    h.show();
                }
                self.on_pointer_move(position.x, position.y);
                true
            }
            SurfaceInputEvent::PointerButton { pressed, button, position } => {
                if let Some(h) = &mut self.cursor_hider {
                    h.show();
                }
                self.on_pointer_button(*pressed, *button, position.x, position.y);
                true
            }
            SurfaceInputEvent::Scroll { position, delta_x, delta_y, unit, .. } => {
                self.on_scroll(position.x, position.y, *delta_x, *delta_y, *unit);
                true
            }
            SurfaceInputEvent::Key { pressed, key, code, modifiers, repeat: _ } => {
                self.on_key(*pressed, key, *code, *modifiers)
            }
            SurfaceInputEvent::TextInput(text) => self.on_text(text.as_str()),
            SurfaceInputEvent::CompositionStart => {
                self.preedit = Some((String::new(), 0));
                true
            }
            SurfaceInputEvent::CompositionUpdate { text, caret } => {
                self.preedit = Some((text.to_string(), caret.unwrap_or(text.len())));
                true
            }
            SurfaceInputEvent::CompositionCommit(text) => {
                self.preedit = None;
                let _ = self.on_text(text.as_str());
                true
            }
            SurfaceInputEvent::CompositionCancel => {
                self.preedit = None;
                true
            }
        };
        if needs_frame
            && let Some(invalidator) = &self.invalidator
        {
            invalidator();
        }
    }

    /// IME window position: the cell under the caret, in surface-local
    /// logical coordinates — the same space `build_scene` draws in.
    fn ime_caret(&self) -> Option<kurbo::Rect> {
        let term = self.session.terminal.term.lock();
        let CursorInfo { row, col, .. } = cursor_info(&term);
        if row < 0 {
            return None;
        }
        let m = self.fonts.metrics;
        let x = PADDING as f64 + col as f64 * m.cell_w as f64;
        let y = PADDING as f64 + row as f64 * m.cell_h as f64;
        Some(kurbo::Rect::new(x, y, x + 2.0, y + m.cell_h as f64))
    }

    fn accessibility_label(&self) -> Option<String> {
        Some("Terminal".to_string())
    }

    fn accessibility_value(&self) -> Option<String> {
        let term = self.session.terminal.term.lock();
        let grid = term.grid();
        let bottom = grid.screen_lines() as i32 - 1;
        Some(term.bounds_to_string(
            Point::new(Line(bottom.saturating_sub(5)), Column(0)),
            Point::new(Line(bottom), grid.last_column()),
        ))
    }
}

/// Recognized URL schemes for plain-text detection.
const URL_SCHEMES: [&str; 7] = [
    "https://",
    "http://",
    "file://",
    "ssh://",
    "git://",
    "ftp://",
    "gemini://",
];

/// Scan `chars` (one grid row) for a scheme:// run covering `col`.
/// Returns the URL; wraps at row boundaries are not followed.
fn url_at(chars: &[char], col: usize) -> Option<String> {
    for scheme in URL_SCHEMES {
        let sc: Vec<char> = scheme.chars().collect();
        let mut off = 0;
        while off + sc.len() <= chars.len() {
            if chars[off..off + sc.len()] == sc[..] {
                let mut end = off + sc.len();
                while end < chars.len() && is_url_char(chars[end]) {
                    end += 1;
                }
                // Trailing sentence punctuation is almost never part of the URL.
                while end > off + sc.len() && matches!(chars[end - 1], '.' | ',' | ';' | ':' | '!' | '?') {
                    end -= 1;
                }
                if col >= off && col < end {
                    return Some(chars[off..end].iter().collect());
                }
                off = end;
            } else {
                off += 1;
            }
        }
    }
    None
}

/// Bytes allowed inside a bare URL — printable ASCII minus delimiters
/// and brackets (so `](https://x)` and `<https://x>` trim correctly).
fn is_url_char(c: char) -> bool {
    c.is_ascii_graphic()
        && !matches!(
            c,
            '"' | '\'' | '<' | '>' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '`'
        )
}

/// Every scheme:// run in `chars` as `(start, end-exclusive)` column pairs.
/// Same scan as `url_at` without the column filter — powers URL hint mode.
fn url_spans(chars: &[char]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    for scheme in URL_SCHEMES {
        let sc: Vec<char> = scheme.chars().collect();
        let mut off = 0;
        while off + sc.len() <= chars.len() {
            if chars[off..off + sc.len()] == sc[..] {
                let mut end = off + sc.len();
                while end < chars.len() && is_url_char(chars[end]) {
                    end += 1;
                }
                while end > off + sc.len() && matches!(chars[end - 1], '.' | ',' | ';' | ':' | '!' | '?') {
                    end -= 1;
                }
                spans.push((off, end));
                off = end;
            } else {
                off += 1;
            }
        }
    }
    spans.sort_unstable();
    spans
}

/// Detached `xdg-open` — never wait on the launcher.
/// `file://` URI → filesystem path with percent-decoding; non-URI
/// input returns verbatim.
fn file_uri_to_path(text: &str) -> String {
    let Some(rest) = text.strip_prefix("file://") else {
        return text.to_string();
    };
    // Strip an optional authority (`file://host/path`); localhost and
    // the empty authority both map to this machine.
    let path = match rest.split_once('/') {
        Some(("", p)) | Some(("localhost", p)) => format!("/{p}"),
        Some((host, p)) => format!("//{host}/{p}"),
        None => rest.to_string(),
    };
    let mut out: Vec<u8> = Vec::with_capacity(path.len());
    let bytes = path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(&path[i + 1..i + 3], 16)
        {
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Dropped text → the line written to the PTY: each whitespace-separated
/// item is URI-decoded and shell-quoted, joined with spaces.
fn drop_payload(text: &str) -> String {
    let mut out = String::new();
    for item in text.split_whitespace() {
        let path = file_uri_to_path(item);
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&shell_quote(&path));
    }
    out
}

/// Shell-quote a path so a dropped filename survives as one argument:
/// alnum and `._/~-` stay bare, anything else wraps in single quotes
/// with `'`\'' escaping (the POSIX idiom).
fn shell_quote(path: &str) -> String {
    if path
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '/' | '~' | '-'))
    {
        return path.to_string();
    }
    format!("'{}'", path.replace('\'', "'\\''"))
}

fn open_url(uri: &str) {
    let _ = std::process::Command::new("xdg-open")
        .arg(uri)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// OSC 9/777 → a freedesktop desktop notification via `notify-send` when
/// the desktop provides it; a missing binary or session bus just leaves
/// the in-app bell/badge path to carry the notification.
#[cfg(target_os = "linux")]
fn notify_desktop(title: &str, body: &str) {
    let _ = std::process::Command::new("notify-send")
        .arg("--app-name=hydroterm")
        .arg(if title.is_empty() { "hydroterm" } else { title })
        .arg(body)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(not(target_os = "linux"))]
fn notify_desktop(_title: &str, _body: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn drop_payload_decodes_and_quotes() {
        assert_eq!(
            drop_payload("file:///home/u/plain.txt"),
            "/home/u/plain.txt"
        );
        assert_eq!(
            drop_payload("file:///home/u/a%20b%20c.png"),
            "'/home/u/a b c.png'"
        );
        assert_eq!(
            drop_payload("file:///tmp/it's.txt"),
            "'/tmp/it'\\''s.txt'"
        );
        // Multiple items on one drop join space-separated.
        assert_eq!(
            drop_payload("file:///a%20b\nfile:///c"),
            "'/a b' /c"
        );
        // Non-URI text passes through (still quoted when needed).
        assert_eq!(drop_payload("/x/y-z"), "/x/y-z");
        // UTF-8 percent sequences decode, not byte-as-char corruption
        // (non-ASCII letters are alphanumeric → stay unquoted).
        assert_eq!(drop_payload("file:///tmp/%C3%A4"), "/tmp/ä");
        assert_eq!(drop_payload("   "), "");
    }

    #[test]
    fn url_at_finds_url_under_col() {
        let row = chars("see https://example.com/x for docs");
        assert_eq!(url_at(&row, 10), Some("https://example.com/x".to_string()));
        assert_eq!(url_at(&row, 0), None);
        assert_eq!(url_at(&row, 30), None);
    }

    #[test]
    fn url_at_trims_trailing_punct() {
        let row = chars("open https://a.b/c, then");
        assert_eq!(url_at(&row, 6), Some("https://a.b/c".to_string()));
    }

    #[test]
    fn url_at_trims_brackets() {
        let row = chars("[x](https://a.b/?q=(r)) ");
        // Click inside the link: parens stop the scan.
        assert_eq!(url_at(&row, 8), Some("https://a.b/?q=".to_string()));
    }

    #[test]
    fn url_spans_finds_both_links() {
        let s = chars("open https://a.io/x then https://b.dev/y.");
        let spans = url_spans(&s);
        assert_eq!(spans.len(), 2);
        assert_eq!(s[spans[0].0..spans[0].1].iter().collect::<String>(), "https://a.io/x");
        assert_eq!(s[spans[1].0..spans[1].1].iter().collect::<String>(), "https://b.dev/y");
    }

    #[test]
    fn url_at_second_of_two() {
        let row = chars("https://a.b/ and http://c.d/e");
        assert_eq!(url_at(&row, 22), Some("http://c.d/e".to_string()));
    }

    #[test]
    fn bell_flash_holds_then_clears() {
        let t0 = Instant::now();
        assert!((bell_flash_alpha(Some(t0), t0) - BELL_FLASH_ALPHA).abs() < 1e-6);
        let mid = t0 + Duration::from_secs_f32(BELL_FLASH_SECS / 2.0);
        assert!(
            (bell_flash_alpha(Some(t0), mid) - BELL_FLASH_ALPHA).abs() < 1e-6
        );
        let past = t0 + Duration::from_secs_f32(BELL_FLASH_SECS + 0.05);
        assert_eq!(bell_flash_alpha(Some(t0), past), 0.0);
        assert_eq!(bell_flash_alpha(None, t0), 0.0);
    }

    // -- logical-line model ---------------------------------------------------

    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::term::{Config, Term};
    use alacritty_terminal::vte::ansi::Processor;

    #[derive(Clone, Copy)]
    struct Sz(usize, usize);
    impl Dimensions for Sz {
        fn total_lines(&self) -> usize {
            self.0
        }
        fn screen_lines(&self) -> usize {
            self.0
        }
        fn columns(&self) -> usize {
            self.1
        }
    }

    fn feed(term: &mut Term<VoidListener>, bytes: &str) {
        let mut p: Processor = Processor::new();
        p.advance(term, bytes.as_bytes());
    }

    /// A match that crosses a soft wrap is ONE hit whose highlight covers
    /// the correct cells on both rows — match positions are computed on
    /// the grid as it is laid out, soft wraps included.
    #[test]
    fn search_match_spanning_soft_wrap() {
        let mut term = Term::new(Config::default(), &Sz(24, 20), VoidListener);
        feed(&mut term, "see https://example.com/alpha here");
        let grid = term.grid();
        let top = -(grid.history_size() as i32);
        let bottom = grid.screen_lines() as i32 - 1;
        let query: Vec<char> = "https://example.com"
            .chars()
            .flat_map(char::to_lowercase)
            .collect();
        let hits: Vec<Vec<(usize, usize, i32)>> = logical_lines(grid, top, bottom)
            .iter()
            .flat_map(|lm| line_map_matches(lm, &query, grid.columns()))
            .collect();
        assert_eq!(hits.len(), 1, "wrap-crossing URL is one match: {hits:?}");
        // "see " occupies cols 0..4 of row 0; "https://example." fills
        // cols 4..20, then "com" continues on row 1 cols 0..3.
        assert_eq!(hits[0], vec![(4, 20, 0), (0, 3, 1)]);
    }

    /// URL detection on logical lines: a link split across a soft wrap is
    /// one span that maps back to cells on both rows.
    #[test]
    fn url_span_maps_across_soft_wrap() {
        let mut term = Term::new(Config::default(), &Sz(24, 20), VoidListener);
        feed(&mut term, "see https://example.com/alpha here");
        let grid = term.grid();
        let lm = logical_line_at(grid, 0);
        let spans = url_spans(&lm.chars);
        assert_eq!(spans.len(), 1, "{spans:?}");
        let (s, e) = spans[0];
        let url: String = lm.chars[s..e].iter().collect();
        assert_eq!(url, "https://example.com/alpha");
        // cols 4..20 of row 0 ("https://example."), then "com/alpha" on
        // row 1 cols 0..9.
        assert_eq!(span_segments(&lm, s, e, grid.columns()), vec![(4, 20, 0), (0, 9, 1)]);
    }

    /// Wide chars occupy two cells: match segments span cells, not chars.
    #[test]
    fn match_segments_span_wide_chars() {
        let mut term = Term::new(Config::default(), &Sz(24, 20), VoidListener);
        feed(&mut term, "你好ab");
        let grid = term.grid();
        let lm = logical_line_at(grid, 0);
        let query: Vec<char> = "好a".chars().flat_map(char::to_lowercase).collect();
        assert_eq!(line_map_matches(&lm, &query, grid.columns()), vec![vec![(2, 5, 0)]]);
    }
}
