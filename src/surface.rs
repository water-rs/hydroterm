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
use alacritty_terminal::vte::ansi::{Color as AnsiColor, Processor};
use nami::{Binding, Signal, binding};
use waterui::cursor::CursorStyle;
use waterui::snackbar::Snackbar;
use waterui::task::spawn_local;
use waterui::window::WindowState;
use waterui_core::Str;
use waterui_graphics::input::{ScrollUnit, SurfaceInputEvent, SurfacePointerButton};
use waterui_graphics::scene2d::Scene2D;
use waterui_graphics::scene_view::{SceneContent, SceneInvalidator};
use waterui_graphics::{Code, Key, Modifiers, NamedKey};
use waterui_text::FontCollection;

use crate::app::{AppState, Session};
use crate::config::MouseShiftCapture;
use crate::fonts::TermFonts;
use crate::keys::{FileSink, TermAction, action_chord, key_release_bytes, key_to_bytes, tab_chord};
use crate::mouse::{self, CellPos, MouseAction};
use crate::osctap::TapEvent;
use crate::palette::Palette;
use crate::scene::{self, CursorInfo, DrawContext, HintSpan, PADDING, ScrollInfo, cursor_info};
use crate::terminal::TermEvent;

/// Blink half-period for the cursor.
const BLINK_HALF: Duration = Duration::from_millis(530);
/// Bell flash decay time.
const BELL_FLASH_SECS: f32 = 0.15;

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

/// `undo` serializer — the session's whole grid (scrollback + screen)
/// as a byte stream cell-faithful enough to replay: characters plus
/// SGR color/attribute runs, WRAPLINE rows joined (no newline), and
/// wide-char spacer cells skipped. Read from `History(-…)` through the
/// last screen line; trailing unstyled spaces are trimmed per row.
/// Replay via `Terminal::inject_output` lands the text above the new
/// shell's first prompt.
pub(crate) fn dump_grid_ansi(session: &Session) -> Vec<u8> {
    use alacritty_terminal::vte::ansi::NamedColor;
    let term = session.terminal.term.lock();
    let grid = term.grid();
    let history = grid.history_size() as i32;
    let lines = grid.screen_lines() as i32;
    let cols = grid.columns();
    let style = Flags::BOLD
        | Flags::DIM
        | Flags::ITALIC
        | Flags::UNDERLINE
        | Flags::INVERSE
        | Flags::STRIKEOUT
        | Flags::HIDDEN;
    let mut out: Vec<u8> = Vec::new();
    let mut cur_flags = Flags::empty();
    let mut cur_fg = AnsiColor::Named(NamedColor::Foreground);
    let mut cur_bg = AnsiColor::Named(NamedColor::Background);
    for l in -history..lines {
        let row = &grid[Line(l)];
        // Last significant column: a cell matters if it isn't a blank
        // with default bg and no style.
        let mut last = cols;
        for c in (0..cols).rev() {
            let cell = &row[Column(c)];
            let styled = cell.bg != AnsiColor::Named(NamedColor::Background)
                || !cell.flags.is_empty();
            if cell.c != ' ' || styled {
                break;
            }
            last = c;
        }
        for c in 0..last {
            let cell = &row[Column(c)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let flags = cell.flags & style;
            if flags != cur_flags || cell.fg != cur_fg || cell.bg != cur_bg {
                out.extend_from_slice(b"\x1b[0m");
                let mut codes = String::new();
                for (flag, code) in [
                    (Flags::BOLD, "1"),
                    (Flags::DIM, "2"),
                    (Flags::ITALIC, "3"),
                    (Flags::UNDERLINE, "4"),
                    (Flags::INVERSE, "7"),
                    (Flags::HIDDEN, "8"),
                    (Flags::STRIKEOUT, "9"),
                ] {
                    if flags.contains(flag) {
                        codes.push_str(code);
                        codes.push(';');
                    }
                }
                push_color(&mut codes, cell.fg, false);
                push_color(&mut codes, cell.bg, true);
                while codes.ends_with(';') {
                    codes.pop();
                }
                if !codes.is_empty() {
                    out.extend_from_slice(format!("\x1b[{codes}m").as_bytes());
                }
                cur_flags = flags;
                cur_fg = cell.fg;
                cur_bg = cell.bg;
            }
            let mut b = [0u8; 4];
            out.extend_from_slice(cell.c.encode_utf8(&mut b).as_bytes());
            if let Some(zs) = cell.zerowidth() {
                for &z in zs {
                    out.extend_from_slice(z.encode_utf8(&mut b).as_bytes());
                }
            }
        }
        // A row whose last cell carries WRAPLINE continues on the next
        // grid row — join them instead of emitting a newline.
        let wrapped = cols > 0 && row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
        if !wrapped {
            out.extend_from_slice(b"\r\n");
        }
    }
    out.extend_from_slice(b"\x1b[0m");
    out
}

/// SGR operand for one cell color: `38`/`48` + `;5;n` for indexed /
/// named palette colors, `;2;r;g;b` for RGB. Named "default" colors
/// (Foreground/Background) emit 39/49.
fn push_color(codes: &mut String, color: AnsiColor, bg: bool) {
    use alacritty_terminal::vte::ansi::NamedColor;
    let base = if bg { "48" } else { "38" };
    let mut index = |n: u8| {
        codes.push_str(base);
        codes.push_str(";5;");
        codes.push_str(&n.to_string());
        codes.push(';');
    };
    match color {
        AnsiColor::Named(n) => match n {
            NamedColor::Foreground
            | NamedColor::BrightForeground
            | NamedColor::DimForeground => {
                codes.push_str(if bg { "49" } else { "39" });
                codes.push(';');
            }
            NamedColor::Background => {
                codes.push_str(if bg { "49" } else { "39" });
                codes.push(';');
            }
            NamedColor::Cursor => {
                codes.push_str(if bg { "49" } else { "39" });
                codes.push(';');
            }
            NamedColor::Black => index(0),
            NamedColor::Red => index(1),
            NamedColor::Green => index(2),
            NamedColor::Yellow => index(3),
            NamedColor::Blue => index(4),
            NamedColor::Magenta => index(5),
            NamedColor::Cyan => index(6),
            NamedColor::White => index(7),
            NamedColor::DimBlack => index(0),
            NamedColor::DimRed => index(1),
            NamedColor::DimGreen => index(2),
            NamedColor::DimYellow => index(3),
            NamedColor::DimBlue => index(4),
            NamedColor::DimMagenta => index(5),
            NamedColor::DimCyan => index(6),
            NamedColor::DimWhite => index(7),
            NamedColor::BrightBlack => index(8),
            NamedColor::BrightRed => index(9),
            NamedColor::BrightGreen => index(10),
            NamedColor::BrightYellow => index(11),
            NamedColor::BrightBlue => index(12),
            NamedColor::BrightMagenta => index(13),
            NamedColor::BrightCyan => index(14),
            NamedColor::BrightWhite => index(15),
        },
        AnsiColor::Indexed(n) => index(n),
        AnsiColor::Spec(rgb) => {
            codes.push_str(base);
            codes.push_str(";2;");
            codes.push_str(&rgb.r.to_string());
            codes.push(';');
            codes.push_str(&rgb.g.to_string());
            codes.push(';');
            codes.push_str(&rgb.b.to_string());
            codes.push(';');
        }
    }
}

/// `osc-color-report-format = 8-bit`: rewrite every `rgb:XXXX/YYYY/ZZZZ`
/// in a color-report reply to `rgb:XX/YY/ZZ` (unscaled — each component
/// keeps its most significant byte, Ghostty `osc_color_report_format`).
fn color_reply_8bit(reply: &str) -> String {
    let mut out = String::with_capacity(reply.len());
    let mut rest = reply;
    while let Some(pos) = rest.find("rgb:") {
        out.push_str(&rest[..pos + 4]);
        rest = &rest[pos + 4..];
        for i in 0..3 {
            let end = rest
                .find(|c: char| !(c.is_ascii_hexdigit()))
                .unwrap_or(rest.len());
            let comp = &rest[..end];
            out.push_str(&comp[..comp.len().min(2)]);
            rest = &rest[end..];
            if i < 2 {
                if let Some(sep) = rest.strip_prefix('/') {
                    out.push('/');
                    rest = sep;
                } else {
                    break;
                }
            }
        }
    }
    out.push_str(rest);
    out
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
    /// `font-family-bold` / `-italic` / `-bold-italic` last applied —
    /// re-resolved inside `sync_fonts` only when one changes.
    style_prefs: (Option<String>, Option<String>, Option<String>),
    font_style_pref: Option<String>,
    variant_style_prefs: (Option<String>, Option<String>, Option<String>),
    codepoint_map_pref: Vec<(u32, u32, String)>,

    // geometry (grid size in cells, logical units at draw time)
    cols: u16,
    lines: u16,
    /// Draw/input origin of the cell grid — `PADDING`, or `PADDING` plus
    /// half the leftover when `window-padding-balance` centers the grid.
    pad_x: f32,
    pad_y: f32,

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
    /// Epoch returned by `set_wake`; passed to `clear_wake` so dropping a
    /// stale surface after its replacement installed a wake can't erase
    /// the live callback.
    wake_epoch: Cell<u64>,
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
    /// `bell-features` `border` — ring the pane until re-focused or
    /// interacted with (cleared in `clear_notify_badge`).
    bell_border: bool,
    /// Last grid-size change for the `resize-overlay` badge decay.
    resize_at: Option<Instant>,
    /// Resizes seen on this surface — `resize-overlay = after-first`
    /// suppresses the very first one (the initial layout).
    resize_count: u32,
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
    /// `toggle_mouse_visibility` manual hide state — survives pointer
    /// motion until toggled off (Ghostty).
    pointer_hidden: bool,
    /// `content_gen` seen by the last `build()` — `scroll-to-bottom
    /// output` snaps when the term publishes new cells while scrolled.
    last_output_gen: Cell<u64>,
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
        let style_prefs = app.config(|c| {
            (
                c.font_family_bold.clone(),
                c.font_family_italic.clone(),
                c.font_family_bold_italic.clone(),
            )
        });
        let font_style_pref = app.config(|c| c.font_style.clone());
        let variant_style_prefs = app.config(|c| {
            (
                c.font_style_bold.clone(),
                c.font_style_italic.clone(),
                c.font_style_bold_italic.clone(),
            )
        });
        let codepoint_map_pref = app.config(|c| c.font_codepoint_map.clone());
        let mut fonts = TermFonts::load(fonts, font_size, &family_pref);
        fonts.set_style_families(&style_prefs.0, &style_prefs.1, &style_prefs.2);
        fonts.set_font_style(&font_style_pref);
        fonts.set_variant_styles(
            &variant_style_prefs.0,
            &variant_style_prefs.1,
            &variant_style_prefs.2,
        );
        fonts.set_codepoint_map(&codepoint_map_pref);
        Self {
            session,
            app,
            fonts,
            style_prefs,
            font_style_pref,
            variant_style_prefs,
            codepoint_map_pref,
            palette,
            font_size_pt: font_size,
            family_pref,
            cols: 0,
            lines: 0,
            pad_x: PADDING,
            pad_y: PADDING,
            invalidator: None,
            wake_tx: None,
            content_gen: Rc::new(Cell::new(0)),
            wake_alive: Rc::new(Cell::new(false)),
            wake_epoch: Cell::new(0),
            wake_task: None,
            focused: false,
            modifiers: Modifiers::empty(),
            held_button: None,
            selecting: false,
            last_click: None,
            preedit: None,
            scroll_accum_px: 0.0,
            bell_at: None,
            bell_border: false,
            resize_at: None,
            resize_count: 0,
            bell_ring_at: None,
            blink_epoch: Instant::now(),
            search: None,
            hints: None,
            keysel: None,
            clipboard: waterkit_clipboard::Clipboard::new().ok(),
            cursor_hider: None,
            cursor_hider_tried: false,
            pointer_hidden: false,
            last_output_gen: Cell::new(0),
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
        let (pad_x, pad_y) = (self.pad_x as f64, self.pad_y as f64);
        let (cw, ch) = (m.cell_w as f64, m.cell_h as f64);
        let col = ((x - pad_x) / cw)
            .clamp(0.0, self.cols.saturating_sub(1) as f64) as usize;
        let row = ((y - pad_y) / ch)
            .clamp(0.0, self.lines.saturating_sub(1) as f64) as usize;
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
        let pad = self.pad_x as f64;
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
        self.app.refocus(self.session.id);
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
        // `clipboard-paste-bracketed-safe` — true (default) strips
        // literal ESC so the pasted text cannot escape the bracket;
        // false passes bytes verbatim (Ghostty's paranoia-off mode).
        if self.app.config(|c| c.paste_bracketed_safe) {
            out.push_str(&text.replace('\x1b', ""));
        } else {
            out.push_str(text);
        }
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
        self.app.refocus(self.session.id);
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
            // `app-notifications = clipboard-copy` — the copy toast.
            if self.app.config(|c| c.app_notify_clipboard_copy)
                && let Some(manager) = self.session.snackbar.borrow().as_ref()
            {
                manager.show(Snackbar::new("Copied to clipboard"));
            }
            // `selection-clear-on-copy` — explicit copies clear the
            // selection; `copy-on-select` goes through `copy_on_select`
            // and never lands here.
            if self.app.config(|c| c.selection_clear_on_copy) {
                self.session.terminal.term.lock().selection = None;
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

    /// The URI under `point`: an OSC8 hyperlink first, then a
    /// plain-text URL scanned off the row. `link-url = false` gates
    /// only the detected-URL scan — `link = <regex>` patterns still
    /// apply (they are a separate feature in the reference).
    fn link_at(&self, point: Point) -> Option<String> {
        let term = self.session.terminal.term.lock();
        let uri = term.grid()[point].hyperlink().map(|h| h.uri().to_string());
        uri.or_else(|| {
            let lm = logical_line_at(term.grid(), point.line.0);
            let idx = lm
                .marks
                .iter()
                .rfind(|m| m.1 == point.line.0 && m.2 <= point.column.0)
                .map(|m| m.0)?;
            let (url_on, patterns) = self
                .app
                .config(|c| (c.link_url, c.link_patterns.clone()));
            url_at(&lm.chars, idx, &patterns, url_on)
        })
    }

    /// Open the link under `point` (like xterm/kitty Ctrl+click).
    fn open_link_at(&self, point: Point) -> bool {
        if let Some(uri) = self.link_at(point) {
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
    /// `keybind = all:` — apply the per-surface part of the action to
    /// every session (Ghostty: an `all:` bind fires on all surfaces).
    /// Session-model actions apply to each session; anything else runs
    /// once through `do_action`.
    fn do_action_all(&mut self, action: TermAction) {
        match action {
            TermAction::FontReset => {
                let configured = self.app.config(|c| c.font_size);
                for s in self.app.all_sessions().iter() {
                    s.font_size_override.set(false);
                    s.font_size.set(configured);
                }
            }
            TermAction::IncreaseFontSize(pts) | TermAction::DecreaseFontSize(pts) => {
                let delta = if matches!(action, TermAction::DecreaseFontSize(_)) {
                    -pts as f32
                } else {
                    pts as f32
                };
                for s in self.app.all_sessions().iter() {
                    let cur = s.font_size.snapshot();
                    s.font_size_override.set(true);
                    s.font_size.set((cur + delta).clamp(6.0, 96.0));
                }
            }
            TermAction::SetFontSize(pt) => {
                for s in self.app.all_sessions().iter() {
                    s.font_size_override.set(true);
                    s.font_size.set(pt.clamp(6.0, 96.0));
                }
            }
            TermAction::ClearScrollback => {
                for s in self.app.all_sessions().iter() {
                    let mut term = s.terminal.term.lock();
                    term.grid_mut().clear_history();
                    term.scroll_display(Scroll::Bottom);
                }
            }
            TermAction::ClearScreen => {
                for s in self.app.all_sessions().iter() {
                    let mut term = s.terminal.term.lock();
                    let mut p: Processor = Processor::new();
                    p.advance(&mut *term, b"\x1b[3J\x1b[2J\x1b[H");
                    term.scroll_display(Scroll::Bottom);
                }
            }
            TermAction::Reset => {
                for s in self.app.all_sessions().iter() {
                    let mut term = s.terminal.term.lock();
                    let mut p: Processor = Processor::new();
                    p.advance(&mut *term, b"\x1bc");
                    term.scroll_display(Scroll::Bottom);
                }
            }
            // `all:` on a sequence applies each member's all-surface
            // semantics (font/scrollback actions hit every pane).
            TermAction::Sequence(actions) => {
                for a in actions {
                    self.do_action_all(a);
                }
            }
            _ => self.do_action(action),
        }
    }

    fn do_action(&mut self, action: TermAction) {
        match action {
            TermAction::Copy => self.copy_selection(),
            TermAction::Paste => self.paste_clipboard(),
            TermAction::PasteConfirm => self.paste_confirm(true),
            TermAction::DropText(text) => self.drop_text(&text),
            TermAction::NewTab => {
                self.app.new_tab();
            }
            TermAction::CloseTab => {
                if let Some(tab_id) = self.app.tab_id_of(self.session.id) {
                    self.app.try_close_tab(tab_id);
                }
            }
            TermAction::CloseSurface => self.app.try_close_pane(self.session.id),
            TermAction::CloseConfirm => {
                self.app.confirm_close(self.session.id);
                self.app.refocus(self.session.id);
            }
            TermAction::NewWindow => self.app.new_window(),
            TermAction::ToggleQuickTerminal => self.app.toggle_quick(),
            TermAction::LastTab => self.app.select_last_tab(),
            TermAction::CloseWindow => self.app.close_window(),
            TermAction::CloseAllTabs => self.app.close_all_tabs(),
            TermAction::CloseOtherTabs => self.app.close_other_tabs(),
            TermAction::ToggleTabBar => self.app.toggle_tab_bar(),
            TermAction::NextTab => self.app.cycle_tab(1),
            TermAction::PrevTab => self.app.cycle_tab(-1),
            TermAction::SelectTab(n) => self.app.select_tab(n),
            TermAction::FontReset => {
                self.session.font_size_override.set(false);
                self.session
                    .font_size
                    .set(self.app.config(|c| c.font_size));
            }
            TermAction::IncreaseFontSize(pts) | TermAction::DecreaseFontSize(pts) => {
                let cur = self.session.font_size.snapshot();
                let delta = if matches!(action, TermAction::DecreaseFontSize(_)) {
                    -pts as f32
                } else {
                    pts as f32
                };
                self.session.font_size_override.set(true);
                self.session.font_size.set((cur + delta).clamp(6.0, 96.0));
            }
            TermAction::SetFontSize(pt) => {
                self.session.font_size_override.set(true);
                self.session.font_size.set(pt.clamp(6.0, 96.0));
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
            TermAction::Search => {
                self.session.search_open.toggle();
                if !self.session.search_open.snapshot() {
                    self.app.refocus(self.session.id);
                }
            }
            TermAction::StartSearch => self.session.search_open.set(true),
            TermAction::EndSearch => {
                self.session.search_open.set(false);
                self.session.search_query.set_from("");
                self.app.refocus(self.session.id);
            }
            TermAction::SearchSelection => {
                let text = self
                    .session
                    .terminal
                    .term
                    .lock()
                    .selection_to_string();
                if let Some(text) = text.filter(|t| !t.is_empty()) {
                    self.session.search_query.set_from(text);
                    self.session.search_open.set(true);
                }
            }
            // `search:text` — set the query (the bar only needs to open
            // when a non-empty query is set); empty cancels the search
            // without hiding the bar (the reference's semantics).
            TermAction::SearchFor(text) => {
                if text.is_empty() {
                    self.session.search_query.set_from("");
                } else {
                    self.session.search_query.set_from(text);
                    self.session.search_open.set(true);
                }
            }
            TermAction::JumpToPrompt(n) => {
                let dir = n.signum();
                for _ in 0..n.abs() {
                    let before = self
                        .session
                        .terminal
                        .term
                        .lock()
                        .grid()
                        .display_offset();
                    self.jump_prompt(dir);
                    // A markless direction ends the loop early.
                    if self
                        .session
                        .terminal
                        .term
                        .lock()
                        .grid()
                        .display_offset()
                        == before
                    {
                        break;
                    }
                }
            }
            // `sequence:` — run each member through this dispatch.
            TermAction::Sequence(actions) => {
                for a in actions {
                    self.do_action(a);
                }
            }
            TermAction::Undo => self.app.undo_close(),
            TermAction::ToggleMark => self.toggle_mark(),
            TermAction::JumpToMark(dir) => self.jump_mark(dir),
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
            // `scroll_page_lines:N` — positive N scrolls down
            // (toward the live edge), matching the negative delta.
            TermAction::ScrollPageLines(n) => {
                if n != 0 {
                    self.session
                        .terminal
                        .term
                        .lock()
                        .scroll_display(Scroll::Delta(-n));
                }
            }
            // `scroll_page_fractional:f` — positive f scrolls down.
            TermAction::ScrollPageFractional(f) => {
                let mut term = self.session.terminal.term.lock();
                let page = term.grid().screen_lines().saturating_sub(1) as f64;
                let delta = (-f * page).round() as i32;
                if delta != 0 {
                    term.scroll_display(Scroll::Delta(delta));
                }
            }
            TermAction::MoveTab(n) => self.app.move_tab(n as isize),
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
            TermAction::WriteScreenFile(sink) => {
                if let Some(text) = self.screen_text() {
                    self.write_buffer_sink(text, "screen", sink);
                }
            }
            TermAction::WriteScrollbackFile(sink) => {
                if let Some(text) = self.scrollback_text() {
                    self.write_buffer_sink(text, "scrollback", sink);
                }
            }
            TermAction::WriteSelectionFile(sink) => {
                if let Some(text) = self.selection_text_dump() {
                    self.write_buffer_sink(text, "selection", sink);
                }
            }
            TermAction::WriteLastOutputFile(sink) => {
                if let Some(text) = self.last_output_text() {
                    self.write_buffer_sink(text, "last-output", sink);
                }
            }
            TermAction::OpenConfig => self.open_config(),
            TermAction::ScrollToSelection => {
                let mut term = self.session.terminal.term.lock();
                let (history, offset) =
                    (term.grid().history_size(), term.grid().display_offset());
                if let Some(range) = term.selection.as_ref().and_then(|s| s.to_range(&term)) {
                    let delta =
                        scroll_to_selection_delta(range.start.line.0, history, offset);
                    term.scroll_display(Scroll::Delta(delta));
                }
            }
            TermAction::ClearSelection => {
                self.session.terminal.term.lock().selection = None;
                self.keysel = None;
            }
            TermAction::NavigateSearch(dir) => self.search_step(dir),
            TermAction::Quit => self.app.quit(),
            TermAction::SplitAuto => {
                let (w, h) = self.session.pane_px.snapshot();
                self.app
                    .split_pane(crate::app::auto_split_dir(w, h), self.session.id, false);
            }
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
            // `text:`/`esc:`/`csi:` payloads — literal bytes on the pty,
            // same write path as typed input.
            TermAction::TypeText(s) => self.write(s.into_bytes()),
            TermAction::EscSeq(s) => {
                let mut b = vec![b'\x1b'];
                b.extend_from_slice(s.as_bytes());
                self.write(b);
            }
            TermAction::CsiSeq(s) => {
                let mut b = vec![b'\x1b', b'['];
                b.extend_from_slice(s.as_bytes());
                self.write(b);
            }
            TermAction::PasteFromSelection => {
                let bracketed = self
                    .session
                    .terminal
                    .term
                    .lock()
                    .mode()
                    .contains(TermMode::BRACKETED_PASTE);
                if let Some(t) = self.primary_text() {
                    self.paste_text(&t, bracketed);
                }
            }
            // No `Scroll::Row` variant in the pinned alacritty — the
            // targets convert to a delta off `display_offset`.
            TermAction::ScrollToFraction(f) => {
                let mut term = self.session.terminal.term.lock();
                let history = term.grid().history_size();
                let target = (history as f64 * f).round() as usize;
                let delta = target as i32 - term.grid().display_offset() as i32;
                if delta != 0 {
                    term.scroll_display(Scroll::Delta(delta));
                }
            }
            TermAction::ScrollToRow(n) => {
                let mut term = self.session.terminal.term.lock();
                let history = term.grid().history_size();
                let target = n.min(history);
                let delta = target as i32 - term.grid().display_offset() as i32;
                if delta != 0 {
                    term.scroll_display(Scroll::Delta(delta));
                }
            }
            TermAction::PromptTitle => {
                self.session.title_prompt_writes_tab.set(false);
                self.session
                    .title_query
                    .set(self.session.title.snapshot());
                self.session.title_prompt_open.set(true);
            }
            TermAction::PromptTabTitle => {
                self.session.title_prompt_writes_tab.set(true);
                let seed = self
                    .app
                    .tab_title_of(self.session.id)
                    .unwrap_or_else(|| self.session.title.snapshot());
                self.session.title_query.set(seed);
                self.session.title_prompt_open.set(true);
            }
            TermAction::SetSurfaceTitle(title) => {
                self.app.set_session_title(self.session.id, Str::from(title));
            }
            TermAction::SetTabTitle(title) => {
                self.app
                    .set_tab_title(self.session.id, (!title.is_empty()).then_some(title));
            }
            TermAction::Inspector => {
                let s = &self.session;
                s.inspector_open.set(!s.inspector_open.snapshot());
            }
            TermAction::InspectorSet(open) => {
                self.session.inspector_open.set(open);
            }
            TermAction::Ignore => {}
            TermAction::CopyUrlToClipboard => {
                let (x, y) = self.pointer_at;
                if let Some(uri) = self.link_at(self.grid_point(x, y))
                    && let Some(clip) = self.clipboard.as_mut()
                {
                    let _ = clip.set_text(&uri);
                }
            }
            TermAction::OpenUrl(uri) => {
                self.open_uri(&uri);
            }
            TermAction::CopyTitleToClipboard => {
                if let Some(clip) = self.clipboard.as_mut() {
                    let _ = clip.set_text(self.session.title.snapshot().as_ref());
                }
            }
            TermAction::ToggleMouseVisibility => {
                // Ghostty `toggle_mouse_visibility` — manual hide/show
                // that survives pointer motion until toggled back.
                self.pointer_hidden = !self.pointer_hidden;
                if !self.cursor_hider_tried {
                    self.cursor_hider_tried = true;
                    self.cursor_hider = crate::xcursor::CursorHider::new();
                }
                if let Some(h) = &mut self.cursor_hider {
                    if self.pointer_hidden {
                        h.hide();
                    } else {
                        h.show();
                    }
                }
            }
            // Ghostty `adjust_selection:dir` — drive the keyboard
            // selection from a keybind: no active selection starts one
            // anchored at the cursor; `escape` clears and exits.
            TermAction::AdjustSelection(dir) => {
                use crate::keys::AdjustSel as D;
                if dir == D::Escape {
                    self.session.terminal.term.lock().selection = None;
                    self.keysel = None;
                    return;
                }
                if self.keysel.is_none() {
                    let cur = self.session.terminal.term.lock().grid().cursor.point;
                    self.keysel = Some((cur, cur));
                }
                let key = match dir {
                    D::Left => Key::Named(NamedKey::ArrowLeft),
                    D::Right => Key::Named(NamedKey::ArrowRight),
                    D::Up => Key::Named(NamedKey::ArrowUp),
                    D::Down => Key::Named(NamedKey::ArrowDown),
                    D::Home => Key::Named(NamedKey::Home),
                    D::End => Key::Named(NamedKey::End),
                    D::PageUp => Key::Named(NamedKey::PageUp),
                    D::PageDown => Key::Named(NamedKey::PageDown),
                    D::Escape => unreachable!(),
                };
                self.keysel_key(&key);
            }
        }
    }

    /// Scroll so the OSC 133 prompt mark sits at the viewport top.
    /// `dir` -1 = previous prompt, +1 = next (one mark per call).
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

    /// `toggle_mark` — toggle a mark on the cursor's absolute row
    /// (`history_size + screen line`, the `prompt_marks` convention;
    /// Ghostty marks are invisible — no glyph is drawn).
    fn toggle_mark(&mut self) {
        let term = self.session.terminal.term.lock();
        let abs = term.grid().history_size() as i64 + term.grid().cursor.point.line.0 as i64;
        drop(term);
        let mut marks = self.session.marks.lock().unwrap();
        if let Some(pos) = marks.iter().position(|&m| m == abs) {
            marks.remove(pos);
        } else {
            marks.push(abs);
            marks.sort_unstable();
        }
    }

    /// `jump_to_mark:previous|next` — scroll so the nearest toggled mark
    /// sits at the viewport top (same math as `jump_prompt`).
    fn jump_mark(&mut self, dir: i32) {
        let marks = self.session.marks.lock().unwrap();
        let mut term = self.session.terminal.term.lock();
        let history = term.grid().history_size() as i64;
        let offset = term.grid().display_offset() as i64;
        if marks.is_empty() {
            return;
        }
        let top = history - offset;
        let target = if dir < 0 {
            marks.iter().copied().filter(|&m| m < top).max()
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
                    // Armed 🔔 badge: `set_session_title` re-applies the
                    // window-title prefix itself; session/tab titles stay
                    // clean (the chip shows the badge, not the prefix).
                    self.app.set_session_title(self.session.id, t);
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
                    let reply = fmt(rgb);
                    match self.app.config(|c| c.osc_color_report_format) {
                        crate::config::OscColorReportFormat::None => {}
                        crate::config::OscColorReportFormat::Bits8 => {
                            self.write(color_reply_8bit(&reply).into_bytes());
                        }
                        crate::config::OscColorReportFormat::Bits16 => {
                            self.write(reply.into_bytes());
                        }
                    }
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
                    // `bell-features` `attention`/`title` — each is an
                    // attention alert cleared by `clear_notify_badge`.
                    // Ghostty's `attention` also raises the WM urgency
                    // hint; no waterui/hydrolysis surface reaches
                    // `request_user_attention` — WATERUI_FEEDBACK #51,
                    // unimplemented rather than faked.
                    let attention = self.app.config(|c| c.bell_attention);
                    let title = self.app.config(|c| c.bell_title);
                    if attention || title {
                        *self.session.notify_badge.lock().unwrap() = true;
                    }
                    if attention {
                        self.app.tab_badge(self.session.id, true);
                    }
                    // `bell-features` `border` — a ring around the pane
                    // until it's re-focused or receives input (Ghostty).
                    if self.app.config(|c| c.bell_border) {
                        self.bell_border = true;
                    }
                    // `bell-features` `title` — prepend 🔔 to the
                    // WINDOW title only; the tab chip's mark is the
                    // badge (one attention indicator per surface).
                    if title {
                        self.app.bell_title(self.session.id);
                    }
                }
                TermEvent::ChildExit(code) => {
                    *self.session.child_exit.lock().unwrap() =
                        Some((code, self.session.spawned_at.elapsed()));
                    self.session.exited.set(true);
                }
                TermEvent::Exit => {
                    // `wait-after-command`: a `command`/`-e` child keeps
                    // its last frame mounted after exit; the pane only
                    // goes away via an explicit close (Ghostty). Otherwise
                    // the exit closes just this pane — `close_tab` takes a
                    // tab id, not a session id, so route via close_pane
                    // (which removes one split leaf or the whole tab).
                    // `abnormal-command-exit-runtime`: a child that dies
                    // with a non-zero code within N ms of spawn is
                    // abnormal — hold the surface and show the notice
                    // card instead of closing silently (Ghostty).
                    let hold = self.session.ran_command
                        && self.app.config(|c| c.wait_after_command);
                    let abnormal_ms = self.app.config(|c| c.abnormal_command_exit_runtime);
                    let abnormal = abnormal_ms > 0
                        && self
                            .session
                            .child_exit
                            .lock()
                            .unwrap()
                            .is_some_and(|(code, elapsed)| {
                                code.is_some_and(|c| c != 0)
                                    && elapsed.as_millis() <= abnormal_ms as u128
                            });
                    if hold || abnormal {
                        self.session.exited.set(true);
                        if abnormal {
                            let info = *self.session.child_exit.lock().unwrap();
                            let msg = match info {
                                Some((Some(code), elapsed)) => format!(
                                    "process exited abnormally (code {code}) after {:.1}s",
                                    elapsed.as_secs_f64()
                                ),
                                _ => "process exited abnormally".to_string(),
                            };
                            self.session.abnormal_notice.set(Some(Str::from(msg)));
                        }
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
                    TapEvent::CommandEnd(_code) => {
                        // `notify-on-command-finish`: the OSC 133 C→D
                        // elapsed time decides — shorter commands stay
                        // quiet (`-after`, default 5s).
                        let (when, after) = self.app.config(|c| {
                            (c.notify_on_command_finish, c.notify_on_command_finish_after)
                        });
                        let elapsed = self
                            .session
                            .terminal
                            .command_started_at
                            .lock()
                            .unwrap()
                            .map(|t| t.elapsed().as_secs_f64());
                        let long_enough = elapsed.is_some_and(|e| e >= after);
                        let unfocused =
                            self.app.focused_session().is_none_or(|s| s.id != self.session.id);
                        let fire = long_enough
                            && match when {
                                crate::config::NotifyWhen::Always => true,
                                crate::config::NotifyWhen::Unfocused => unfocused,
                                crate::config::NotifyWhen::No => false,
                            };
                        if fire {
                            if self.app.config(|c| c.visual_bell) {
                                self.bell_at = Some(Instant::now());
                            }
                            notify_desktop("hydroterm", "Command finished");
                            if self.app.config(|c| c.bell_attention) {
                                *self.session.notify_badge.lock().unwrap() = true;
                            self.app.tab_badge(self.session.id, true);
                            }
                            if self.app.config(|c| c.bell_border) {
                                self.bell_border = true;
                            }
                            if self.app.config(|c| c.bell_title) {
                                self.app.set_session_title(
                                    self.session.id,
                                    Str::from("\u{1f514} Command finished"),
                                );
                            }
                        }
                    }
                    TapEvent::Notify(title, body) => {
                        // Bell flash + title badge; `desktop-notifications`
                        // gates only the freedesktop notify-send hop.
                        if self.app.config(|c| c.visual_bell) {
                            self.bell_at = Some(Instant::now());
                        }
                        // `desktop-notifications` gates only the
                        // freedesktop notify-send hop (Ghostty); the
                        // in-app badges below still apply.
                        if self.app.config(|c| c.desktop_notifications) {
                            notify_desktop(&title, &body);
                        }
                        if self.app.config(|c| c.bell_attention) {
                            *self.session.notify_badge.lock().unwrap() = true;
                            self.app.tab_badge(self.session.id, true);
                        }
                        if self.app.config(|c| c.bell_border) {
                            self.bell_border = true;
                        }
                        if self.app.config(|c| c.bell_title) {
                            let text = if title.is_empty() { body } else { format!("{title}: {body}") };
                            self.app
                                .set_session_title(self.session.id, Str::from(format!("\u{1f514} {text}")));
                        }
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
        let limit = self.app.config(|c| c.image_storage_limit);
        let (id, status) = self
            .session
            .kitty
            .borrow_mut()
            .handle(cmd, line, col, limit);
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
        let style_prefs = self.app.config(|c| {
            (
                c.font_family_bold.clone(),
                c.font_family_italic.clone(),
                c.font_family_bold_italic.clone(),
            )
        });
        if style_prefs != self.style_prefs {
            self.fonts
                .set_style_families(&style_prefs.0, &style_prefs.1, &style_prefs.2);
            self.style_prefs = style_prefs;
        }
        let font_style_pref = self.app.config(|c| c.font_style.clone());
        if font_style_pref != self.font_style_pref {
            self.fonts.set_font_style(&font_style_pref);
            self.font_style_pref = font_style_pref;
        }
        let variant_style_prefs = self.app.config(|c| {
            (
                c.font_style_bold.clone(),
                c.font_style_italic.clone(),
                c.font_style_bold_italic.clone(),
            )
        });
        if variant_style_prefs != self.variant_style_prefs {
            self.fonts.set_variant_styles(
                &variant_style_prefs.0,
                &variant_style_prefs.1,
                &variant_style_prefs.2,
            );
            self.variant_style_prefs = variant_style_prefs;
        }
        let codepoint_map_pref = self.app.config(|c| c.font_codepoint_map.clone());
        if codepoint_map_pref != self.codepoint_map_pref {
            self.fonts.set_codepoint_map(&codepoint_map_pref);
            self.codepoint_map_pref = codepoint_map_pref;
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
        self.session.cell_px.set((m.cell_w, m.cell_h));
        // `window-padding-x`/`window-padding-y` live inside the surface's
        // own frame (with the internal `PADDING` margin) so the scene's
        // `window-padding-color` extension can paint them contiguously.
        let (wpx, wpy) = self
            .app
            .config(|c| (c.window_padding_x, c.window_padding_y));
        let pad_x = PADDING + wpx;
        let pad_y = PADDING + wpy;
        let cols = ((width - pad_x * 2.0) / m.cell_w).floor().max(2.0) as u16;
        let lines = ((height - pad_y * 2.0) / m.cell_h).floor().max(1.0) as u16;
        // Degenerate frames (window unmapped/collapsed) must not shrink the
        // PTY — a 1-line winsize breaks apps that read TIOCGWINSZ at start.
        // `window-padding-balance`: split the leftover frame space evenly
        // between the two edges; off, it sits on the right/bottom.
        if self.app.config(|c| c.window_padding_balance) {
            let rem_x = (width - pad_x * 2.0 - f32::from(cols) * m.cell_w).max(0.0);
            let rem_y = (height - pad_y * 2.0 - f32::from(lines) * m.cell_h).max(0.0);
            // Integer pads keep every cell edge on a device-pixel boundary;
            // a fractional offset AA-blends each row/column shared edge.
            self.pad_x = pad_x + (rem_x / 2.0).round();
            self.pad_y = pad_y + (rem_y / 2.0).round();
        } else {
            self.pad_x = pad_x;
            self.pad_y = pad_y;
        }
        if cols > 2 && lines > 1 && (cols != self.cols || lines != self.lines) {
            self.cols = cols;
            self.lines = lines;
            self.session
                .terminal
                .resize(cols, lines, (m.cell_w as u16, m.cell_h as u16));
            // `resize-overlay`: show the new grid size for a beat after
            // the last change — the Instant decays inside build_scene,
            // same pattern as the bell flash. `after-first` skips this
            // surface's very first resize (its initial layout).
            self.resize_count += 1;
            let show = match self.app.config(|c| c.resize_overlay) {
                crate::config::ResizeOverlay::Always => true,
                crate::config::ResizeOverlay::AfterFirst => self.resize_count > 1,
                crate::config::ResizeOverlay::Never => false,
            };
            if show {
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
        } else if self.app.is_quick_app()
            && self.app.config(|c| c.quick_terminal_autohide)
        {
            // `quick-terminal-autohide`: the drop-down's surface losing
            // window focus closes the window (Ghostty). This runs at
            // Focus(false), so the closed state takes effect only when
            // focus actually leaves — not while the window is fading out.
            self.app.quick_state.set(WindowState::Closed);
        }
        let mode = *self.session.terminal.term.lock().mode();
        if mode.contains(TermMode::FOCUS_IN_OUT) {
            self.write(if gained { b"\x1b[I".to_vec() } else { b"\x1b[O".to_vec() });
        }
    }

    /// User attention on this pane clears a 🔔 notification badge.
    fn clear_notify_badge(&mut self) {
        // Bell `border`/`title`/`attention` alerts all persist until the
        // pane is re-focused or receives input (Ghostty).
        self.bell_border = false;
        let mut badge = self.session.notify_badge.lock().unwrap();
        if *badge {
            *badge = false;
            self.app.tab_badge(self.session.id, false);
            drop(badge);
            self.app.bell_title(self.session.id);
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
        if pressed && self.session.title_prompt_open.snapshot() && self.title_prompt_key(key, mods) {
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
                Key::Named(NamedKey::Enter) => {
                    self.app.confirm_close(self.session.id);
                    self.app.refocus(self.session.id);
                }
                Key::Named(NamedKey::Escape) => {
                    self.app.cancel_close_prompt(self.session.id);
                    self.app.refocus(self.session.id);
                }
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
            // `unconsumed:` — the bind fires and the press still encodes
            // to the program, skipping the default chord table (the
            // user's bind occupies this slot). `global:`/`all:` always
            // consume (Ghostty).
            let mut fired_unconsumed = false;
            match self.app.config(|c| c.lookup_keybind(key, mods)) {
                Some((trig, Some(action))) => {
                    // `performable:` — an unperformable bind does not
                    // consume the press; it falls through to the default
                    // chord table and literal bytes (Ghostty).
                    if trig.performable && !self.action_performable(&action) {
                        // fall through
                    } else {
                        if trig.all {
                            self.do_action_all(action);
                        } else {
                            self.do_action(action);
                        }
                        if trig.unconsumed && !trig.all && !trig.global {
                            fired_unconsumed = true;
                        } else {
                            return true;
                        }
                    }
                }
                Some((_, None)) => unbound = true, // explicitly disabled
                None => {}
            }
            // `keybind = clear` suppresses the built-in chord table —
            // only configured binds fire (Ghostty).
            if !unbound
                && !fired_unconsumed
                && !self.app.config(|c| c.keybinds_cleared)
                && let Some(action) = action_chord(key, mods).or_else(|| tab_chord(key, code, mods))
            {
                self.do_action(action);
                return true;
            }
            if let Some(bytes) = key_to_bytes(key, code, mods, mode) {
                self.write(bytes);
                self.clear_selection_on_input();
                self.hide_cursor_on_typing();
                return self.snap_to_bottom_if_scrolled();
            }
            false
        } else if let Some(bytes) = key_release_bytes(key, mods, mode) {
            self.write(bytes);
            self.clear_selection_on_input();
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
                self.app.refocus(self.session.id);
                true
            }
            Key::Named(NamedKey::ArrowDown) => {
                let query = self.app.palette_query.snapshot().to_string();
                let n = crate::app::palette_matches(&self.app, &query).len();
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
                self.app.refocus(self.session.id);
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

    /// `prompt_title` keys: Enter renames, Escape cancels, Backspace
    /// edits; text arrives via `on_text` (same unfocusable-TextField
    /// caveat as the palette — hydrolysis#90 — so the binding is
    /// driven here).
    fn title_prompt_key(&mut self, key: &Key, mods: Modifiers) -> bool {
        match key {
            Key::Named(NamedKey::Enter) => {
                let q = self.session.title_query.snapshot();
                if self.session.title_prompt_writes_tab.get() {
                    self.app
                        .set_tab_title(self.session.id, Some(q.to_string()));
                } else {
                    self.app.set_session_title(self.session.id, q);
                }
                self.session.title_prompt_open.set(false);
                self.app.refocus(self.session.id);
                true
            }
            Key::Named(NamedKey::Escape) => {
                self.session.title_prompt_open.set(false);
                self.app.refocus(self.session.id);
                true
            }
            Key::Named(NamedKey::Backspace) if mods.is_empty() => {
                let mut q = self.session.title_query.snapshot().to_string();
                q.pop();
                self.session.title_query.set_from(q);
                true
            }
            // Printable chars are consumed here so no PTY bytes escape;
            // their text arrives via `on_text`.
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
                self.app.refocus(self.session.id);
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
                self.app.refocus(self.session.id);
                true
            }
            Key::Named(NamedKey::Escape) => {
                self.app.settings_open.set(false);
                self.app.refocus(self.session.id);
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

    /// `performable:` keybind gate — whether the action can be
    /// performed right now (Ghostty): an unperformable bind does not
    /// consume the keypress and the event falls through to normal
    /// input as if the bind were absent.
    fn action_performable(&mut self, action: &TermAction) -> bool {
        match action {
            // Selection-scoped actions need a live selection.
            TermAction::Copy
            | TermAction::ClearSelection
            | TermAction::SearchSelection
            | TermAction::ScrollToSelection
            | TermAction::WriteSelectionFile(_) => {
                self.session.terminal.term.lock().selection.is_some()
            }
            // Last-output copy needs the OSC 133 mark trail.
            TermAction::CopyLastOutput | TermAction::WriteLastOutputFile(_) => {
                !self
                    .session
                    .terminal
                    .prompt_marks
                    .lock()
                    .unwrap()
                    .is_empty()
            }
            // URL copy needs a link under the pointer.
            TermAction::CopyUrlToClipboard => {
                let (x, y) = self.pointer_at;
                self.link_at(self.grid_point(x, y)).is_some()
            }
            // Paste reads each clipboard synchronously.
            TermAction::Paste => !self.clipboard_text().is_empty(),
            TermAction::PasteFromSelection => self.primary_text().is_some(),
            // Scroll actions need scrollback to move through.
            TermAction::ScrollPageUp
            | TermAction::ScrollPageDown
            | TermAction::ScrollPageLines(_)
            | TermAction::ScrollPageFractional(_)
            | TermAction::ScrollToTop
            | TermAction::ScrollToBottom
            | TermAction::ScrollToRow(_)
            | TermAction::ScrollToFraction(_)
            | TermAction::JumpToPrompt(_)
            | TermAction::ClearScrollback => {
                self.session
                    .terminal
                    .term
                    .lock()
                    .grid()
                    .history_size()
                    > 0
            }
            TermAction::NavigateSearch(_) | TermAction::EndSearch => {
                self.search.is_some()
            }
            // Everything else is performable whenever dispatched.
            _ => true,
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
        if self.session.title_prompt_open.snapshot() {
            let mut q = self.session.title_query.snapshot().to_string();
            q.push_str(text);
            self.session.title_query.set_from(q);
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
        self.clear_selection_on_input();
        self.hide_cursor_on_typing();
        self.snap_to_bottom_if_scrolled()
    }

    /// `selection-clear-on-typing` (Ghostty default true): a keypress
    /// that produced PTY bytes — or the start of an IME composition —
    /// drops the selection highlight. `= false` keeps it (click and
    /// Escape still clear manually).
    fn clear_selection_on_input(&mut self) {
        if !self.app.config(|c| c.selection_clear_on_typing) {
            return;
        }
        let mut term = self.session.terminal.term.lock();
        term.selection = None;
    }

    /// Keyboard input bound for the PTY snaps the viewport back to the
    /// live edge — the alacritty/kitty convention; `scroll-to-bottom`
    /// without `keystroke` leaves the viewport where the user scrolled it.
    /// Returns true when the viewport moved (needs a frame).
    fn snap_to_bottom_if_scrolled(&mut self) -> bool {
        if !self.app.config(|c| c.scroll_bottom_keystroke) {
            return false;
        }
        self.snap_viewport_to_bottom()
    }

    /// Paste-like interactions (`paste_text` — menu paste, Shift+Insert,
    /// middle-click PRIMARY, drop, confirmed paste) snap the viewport to
    /// the cursor under `scroll-to-cursor` — a separate gate from
    /// `scroll-to-bottom`'s `keystroke` item, which covers key bytes only.
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

        // `mouse-shift-capture`: under mouse reporting a shifted press/
        // drag bypasses the program and selects locally — unless
        // `always`, which reports the event (shift bit included).
        let shift_override = self.modifiers.contains(Modifiers::SHIFT)
            && self.app.config(|c| c.mouse_shift_capture != MouseShiftCapture::Always);
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

    /// Refresh the context menu's pointer-time snapshot — the link
    /// under the pointer and selection liveness. The framework claims
    /// the secondary button for the menu itself (`hit_test.rs` delivers
    /// `pointer_move` but not `pointer_button` when a `.context_menu`
    /// encloses the surface), so this runs on every pointer move — the
    /// press's move arrives before the claim — and on grid scrolls,
    /// which re-map the cell under a still pointer.
    fn refresh_menu_ctx(&mut self) {
        let (x, y) = self.pointer_at;
        let ctx = crate::app::MenuCtx {
            url: self.link_at(self.grid_point(x, y)).map(Str::from),
            sel: self.session.terminal.term.lock().selection.is_some(),
        };
        self.session.menu_ctx.set(ctx);
    }

    /// Link-hover affordance: while the `open-link-modifier` is held,
    /// underline the link under the pointer and switch the cursor to a
    /// pointing hand. Re-runs on pointer moves and on modifier-chord
    /// changes.
    fn update_hover(&mut self, x: f64, y: f64) {
        self.pointer_at = (x, y);
        if self.app.config(|c| {
            c.right_click_action == crate::config::RightClickAction::ContextMenu
        }) {
            self.refresh_menu_ctx();
        }
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
        // `link-url = false` disables detected URLs; OSC8 hyperlinks
        // (explicit markup) still resolve above, like Ghostty — and
        // `link = <regex>` patterns still apply.
        let (url_on, patterns) = self
            .app
            .config(|c| (c.link_url, c.link_patterns.clone()));
        if !url_on && patterns.is_empty() {
            return Vec::new();
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
        for (s, e) in url_spans(&lm.chars, &patterns, url_on) {
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

        // `mouse-shift-capture`: same gate as motion — shift bypasses
        // reporting unless `always`.
        let shift_override = self.modifiers.contains(Modifiers::SHIFT)
            && self.app.config(|c| c.mouse_shift_capture != MouseShiftCapture::Always);
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
                    // `middle-click-action`: `primary-paste` pastes the
                    // selection clipboard only (the xterm convention),
                    // `clipboard-paste` the standard clipboard.
                    match self.app.config(|c| c.middle_click_action) {
                        crate::config::MiddleClickAction::PrimaryPaste => {
                            if let Some(text) = self.primary_text() {
                                let bracketed = self
                                    .session
                                    .terminal
                                    .term
                                    .lock()
                                    .mode()
                                    .contains(TermMode::BRACKETED_PASTE);
                                self.paste_text(&text, bracketed);
                            }
                        }
                        // `clipboard-paste` — the standard clipboard.
                        crate::config::MiddleClickAction::ClipboardPaste => {
                            self.paste_clipboard()
                        }
                        crate::config::MiddleClickAction::Ignore => {}
                    }
                }
                SurfacePointerButton::Secondary => {
                    match self.app.config(|c| c.right_click_action) {
                        crate::config::RightClickAction::Ignore => {}
                        crate::config::RightClickAction::Copy => {
                            self.copy_selection();
                        }
                        crate::config::RightClickAction::Paste => {
                            self.paste_clipboard();
                        }
                        crate::config::RightClickAction::ContextMenu => {
                            // The framework's `.context_menu` claims the
                            // secondary button before it reaches the
                            // surface (the menu acts on the focused
                            // surface), so no press-side work is possible
                            // here — the menu's `menu_ctx` snapshot is
                            // refreshed on pointer moves instead
                            // (`refresh_menu_ctx`).
                        }
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
                // `cursor-click-to-move`: a click on the cursor's row
                // emits left/right arrows so the line editor moves its
                // input cursor (Ghostty `cursor-click-to-move`).
                if self.app.config(|c| c.cursor_click_to_move)
                    && term.grid().display_offset() == 0
                {
                    let cur = term.grid().cursor.point;
                    if row as i64 == i64::from(cur.line.0) && col != cur.column.0 {
                        let delta = col as i64 - cur.column.0 as i64;
                        let bytes = cursor_click_seq(delta);
                        drop(term);
                        self.write(bytes);
                    }
                }
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
        if !self.app.config(|c| c.link_url) {
            return;
        }
        let term = self.session.terminal.term.lock();
        let grid = term.grid();
        let (cols, history, lines) = (grid.columns(), grid.history_size(), grid.screen_lines());
        let offset = grid.display_offset() as i32;
        let screen = lines as i32;
        let mut spans: Vec<HintSpan> = Vec::new();
        let mut urls: Vec<String> = Vec::new();
        let patterns = self.app.config(|c| c.link_patterns.clone());
        // URLs are detected on logical lines (soft wraps joined) so a
        // link spanning a wrap is one span, then mapped back to cells —
        // the badge anchors on its first visible row part.
        'outer: for lm in logical_lines(grid, -(history as i32), lines as i32 - 1) {
            for (s, e) in url_spans(&lm.chars, &patterns, true) {
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
            // Modifier presses are inert: the chord's own Shift/Ctrl/Alt
            // key-downs arrive as separate events before the real key,
            // and must not exit the mode they are part of.
            Key::Named(
                NamedKey::Shift
                | NamedKey::Control
                | NamedKey::Alt
                | NamedKey::AltGraph
                | NamedKey::Meta
                | NamedKey::CapsLock
                | NamedKey::NumLock
                | NamedKey::ScrollLock,
            ) => return true,
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

    /// The text between the last command's OSC 133 `C` and `D` marks
    /// (`last_output`, reader-thread recorded). A still-running command
    /// has no `D` — the span runs to the live bottom.
    fn last_output_text(&mut self) -> Option<String> {
        let span = *self.session.terminal.last_output.lock().unwrap();
        let (start_abs, end_abs) = span?;
        let mut term = self.session.terminal.term.lock();
        let history = term.grid().history_size() as i64;
        let end_abs = end_abs.unwrap_or(history + term.grid().screen_lines() as i64 - 1);
        if end_abs < start_abs {
            return None;
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
        text
    }

    /// Copy the last command's output to the clipboard.
    fn copy_last_output(&mut self) {
        if let (Some(text), Some(clip)) = (self.last_output_text(), self.clipboard.as_mut())
            && !text.is_empty()
        {
            let _ = clip.set_text(&text);
        }
    }

    /// `open_config` — open the live config file in `$VISUAL`/`$EDITOR`
    /// in a new tab (Ghostty action).
    fn open_config(&mut self) {
        let path = self.app.cfg.borrow().path.clone();
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".to_string());
        let mut cmd: Vec<String> = editor.split_whitespace().map(str::to_string).collect();
        cmd.push(path.to_string_lossy().into_owned());
        self.app.new_tab_command(cmd);
    }

    /// Dump scrollback + screen to a temp file and open `$VISUAL`/`$EDITOR`
    /// on it in a fresh tab (kitty `scrollback_pager`/WezTerm parity).
    fn open_scrollback_editor(&mut self) {
        if let Some(text) = self.scrollback_text() {
            self.write_buffer_sink(text, "scrollback", FileSink::Open);
        }
    }

    /// `write_scrollback_file`/`open_scrollback_editor` region:
    /// scrollback + screen.
    fn scrollback_text(&mut self) -> Option<String> {
        let term = self.session.terminal.term.lock();
        let grid = term.grid();
        let history = grid.history_size() as i32;
        Some(term.bounds_to_string(
            Point::new(Line(-history), Column(0)),
            Point::new(Line(grid.screen_lines() as i32 - 1), grid.last_column()),
        ))
    }

    /// `write_screen_file` region — the visible viewport only.
    fn screen_text(&mut self) -> Option<String> {
        let term = self.session.terminal.term.lock();
        let grid = term.grid();
        Some(term.bounds_to_string(
            Point::new(Line(0), Column(0)),
            Point::new(Line(grid.screen_lines() as i32 - 1), grid.last_column()),
        ))
    }

    /// `write_selection_file` region — the current selection's text.
    fn selection_text_dump(&mut self) -> Option<String> {
        self.session.terminal.term.lock().selection_to_string()
    }

    /// `write_*_file[:action]` — dump `text` to a temp file, then
    /// `open` it in the editor tab, `copy` the path to the clipboard,
    /// or `paste` the path at the cursor (Ghostty's action suffix).
    fn write_buffer_sink(&mut self, text: String, kind: &str, sink: FileSink) {
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
        let path = path.to_string_lossy().into_owned();
        match sink {
            FileSink::Open => {
                let editor = std::env::var("VISUAL")
                    .or_else(|_| std::env::var("EDITOR"))
                    .unwrap_or_else(|_| "vi".to_string());
                let mut cmd: Vec<String> =
                    editor.split_whitespace().map(str::to_string).collect();
                cmd.push(path);
                self.app.new_tab_command(cmd);
            }
            FileSink::Copy => {
                if let Some(clip) = self.clipboard.as_mut() {
                    let _ = clip.set_text(&path);
                }
            }
            FileSink::Paste => {
                let bracketed = self
                    .session
                    .terminal
                    .term
                    .lock()
                    .mode()
                    .contains(TermMode::BRACKETED_PASTE);
                self.paste_text(&path, bracketed);
            }
        }
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

        let shift_override = self.modifiers.contains(Modifiers::SHIFT)
            && self.app.config(|c| c.mouse_shift_capture != MouseShiftCapture::Always);
        if mode.intersects(TermMode::MOUSE_MODE) && !shift_override {
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
        drop(term);
        // The cell under a still pointer changed with the viewport.
        if self.app.config(|c| {
            c.right_click_action == crate::config::RightClickAction::ContextMenu
        }) {
            self.refresh_menu_ctx();
        }
    }

    // -- scene plumbing -------------------------------------------------------

    /// Draw the terminal into the frame's scene.
    fn build(&mut self, scene: &mut dyn Scene2D, width: f32, height: f32) {
        // `scroll-to-bottom = output` — new program output while
        // scrolled snaps the viewport to the live edge before the
        // frame's grid read (Ghostty; the reference documents the
        // `output` item but has not wired it).
        let output_gen = self.content_gen.get();
        if output_gen != self.last_output_gen.get() {
            self.last_output_gen.set(output_gen);
            if self.app.config(|c| c.scroll_bottom_output) {
                self.snap_viewport_to_bottom();
            }
        }
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
            // `inspector`: re-report the cursor cell's attributes on
            // every rendered frame — the chip follows edits and cursor
            // moves with no separate refresh path.
            if self.session.inspector_open.snapshot() {
                let cell = &grid[grid.cursor.point];
                let color = |c: &AnsiColor| match c {
                    AnsiColor::Spec(rgb) => {
                        format!("#{:02x}{:02x}{:02x}", rgb.r, rgb.g, rgb.b)
                    }
                    AnsiColor::Indexed(i) => format!("idx {i}"),
                    AnsiColor::Named(n) => format!("{n:?}"),
                };
                // Internal bits (PROMPT_MARK) are hidden — the chip
                // reports only the upstream flag vocabulary.
                let visible = cell.flags.intersection(Flags::all());
                let flags = if visible.is_empty() {
                    "none".to_string()
                } else {
                    let flags = format!("{visible:?}");
                    flags
                        .strip_prefix("Flags(")
                        .and_then(|f| f.strip_suffix(')'))
                        .unwrap_or(&flags)
                        .to_string()
                };
                let link = cell
                    .hyperlink()
                    .map(|h| h.uri().to_string())
                    .unwrap_or_else(|| "none".into());
                self.session.inspector_label.set_from(format!(
                    "U+{:04X} '{}'  fg {}  bg {}  flags {flags}  link {link}",
                    cell.c as u32,
                    cell.c,
                    color(&cell.fg),
                    color(&cell.bg),
                ));
            }
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
            search_colors: self.app.config(|c| crate::scene::SearchColors {
                foreground: c.search_foreground,
                background: c.search_background,
                selected_foreground: c.search_selected_foreground,
                selected_background: c.search_selected_background,
            }),
            bell_flash: bell_alpha,
            bell_border: self.bell_border,
            bg_opacity,
            cell_bg_opacity: self
                .app
                .config(|c| {
                    crate::config::cell_bg_alpha(c.background_opacity_cells, c.background_opacity)
                }),
            pad_mode: self.app.config(|c| c.window_padding_color),
            hints: &hint_spans,
            hint_digits: &hint_digits,
            pad_x: self.pad_x,
            pad_y: self.pad_y,
            font_synthetic_bold: self
                .app
                .config(|c| c.font_synthetic.unwrap_or((true, true)).0),
            font_synthetic_italic: self
                .app
                .config(|c| c.font_synthetic.unwrap_or((true, true)).1),
            bell_color: self.app.config(|c| c.visual_bell_color),
            // `font-thicken` × `font-thicken-strength` (0-255, 0 =
            // lightest): overdraw offset in px, 0.0 disables.
            font_thicken: self.app.config(|c| {
                if c.font_thicken {
                    0.15 + 0.45 * f32::from(c.font_thicken_strength) / 255.0
                } else {
                    0.0
                }
            }),
            bold_color: self.app.config(|c| c.bold_color),
            faint_opacity: self.app.config(|c| c.faint_opacity),
            min_contrast: self.app.config(|c| c.minimum_contrast),
            selection_invert: self.app.config(|c| c.selection_invert),
            hover_link: &self.hover_link,
            bg_image,
            unfocused_fill: self.app.config(|c| c.unfocused_split_fill),
            cursor_thickness: self
                .app
                .config(|c| if c.adjust_cursor_thickness == 0 { 1.0 } else { c.adjust_cursor_thickness as f32 / 100.0 }),
            cursor_height: self
                .app
                .config(|c| if c.adjust_cursor_height == 0 { 1.0 } else { c.adjust_cursor_height as f32 / 100.0 }),
            underline_adjust: self.app.config(|c| {
                (
                    c.adjust_underline_position as f32,
                    if c.adjust_underline_thickness == 0 { 1.0 } else { c.adjust_underline_thickness as f32 / 100.0 },
                )
            }),
            strikethrough_adjust: self.app.config(|c| {
                (
                    c.adjust_strikethrough_position as f32,
                    if c.adjust_strikethrough_thickness == 0 { 1.0 } else { c.adjust_strikethrough_thickness as f32 / 100.0 },
                )
            }),
        };
        let top = scroll.history_size as i64 - scroll.display_offset as i64;
        let m = ctx.fonts.metrics;
        let (pad, pad_y) = (self.pad_x, self.pad_y);
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
            let y = pad_y + row as f32 * m.cell_h;
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
        self.session
            .terminal
            .proxy
            .clear_wake(self.wake_epoch.get());
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
        if std::env::var_os("HYDRO_SNIFF").is_some() {
            eprintln!("[scene] s{} @{self:p} {width:.0}x{height:.0}", self.session.id);
        }
        self.app.poll_config();
        self.drain_events();
        // Rejoin ZWJ-split scalars before the frame is measured or drawn.
        // `grapheme-width-method = legacy` leaves each scalar in its own
        // cells (Ghostty's legacy grid semantics).
        if self.app.config(|c| {
            matches!(
                c.grapheme_width_method,
                crate::config::GraphemeWidthMethod::Unicode
            )
        }) {
            crate::terminal::fixup_graphemes(&mut self.session.terminal.term.lock());
        }
        self.sync_search();
        self.sync_fonts();
        self.sync_size(width, height);
        if std::env::var_os("HYDRO_SNIFF").is_some() {
            let term = self.session.terminal.term.lock();
            let g = term.grid();
            let mut rows = String::new();
            for i in 0..g.screen_lines() {
                let mut s = String::new();
                for c in 0..g.columns() {
                    s.push(g[alacritty_terminal::index::Line(i as i32)]
                        [alacritty_terminal::index::Column(c)]
                        .c);
                }
                rows.push_str(&format!("|{}|", s.trim_end()));
            }
            eprintln!(
                "[grid] s{} off={} hist={} cur={:?} rows={rows}",
                self.session.id,
                g.display_offset(),
                g.history_size(),
                g.cursor.point
            );
        }
        self.session.pane_px.set((width, height));

        // A blinking cursor or live bell flash needs the next frame anyway;
        // the wake pipe covers PTY output between frames.
        let cursor_blinking = self.session.terminal.term.lock().cursor_style().blinking;
        let bell_live = self
            .bell_at
            .is_some_and(|t| t.elapsed() < Duration::from_secs_f32(BELL_FLASH_SECS));
        // The resize badge stays up for `resize-overlay-duration`
        // after the last size change, then clears itself.
        let resize_ms = self.app.config(|c| c.resize_overlay_ms);
        let resize_live = self
            .resize_at
            .is_some_and(|t| t.elapsed() < Duration::from_millis(resize_ms));
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
                let epoch = self
                    .session
                    .terminal
                    .proxy
                    .set_wake({
                        let tx = tx.clone();
                        move || {
                            let _ = tx.try_send(());
                        }
                    });
                self.wake_epoch.set(epoch);
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
                let sniff = std::env::var_os("HYDRO_SNIFF").is_some();
                self.wake_task = Some(Box::pin(spawn_local(async move {
                    while rx.recv().await.is_ok() {
                        if !alive.get() {
                            break;
                        }
                        // Coalesce bursts: one invalidation per batch.
                        while rx.try_recv().is_ok() {}
                        content_gen.set(content_gen.get() + 1);
                        if sniff {
                            eprintln!("[wake] drain s{session_id} gen={}", content_gen.get());
                        }
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
                self.session
                    .terminal
                    .proxy
                    .clear_wake(self.wake_epoch.get());
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
                // `toggle_mouse_visibility` keeps the pointer hidden
                // across motion until the action toggles it back.
                if !self.pointer_hidden && let Some(h) = &mut self.cursor_hider {
                    h.show();
                }
                self.on_pointer_move(position.x, position.y);
                true
            }
            SurfaceInputEvent::PointerButton { pressed, button, position } => {
                if !self.pointer_hidden && let Some(h) = &mut self.cursor_hider {
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
                if std::env::var_os("HYDROTERM_DEBUG_INPUT").is_some() {
                    eprintln!(
                        "[key] pressed={pressed} key={key:?} code={code:?} mods={modifiers:?}"
                    );
                }
                self.on_key(*pressed, key, *code, *modifiers)
            }
            SurfaceInputEvent::TextInput(text) => {
                if std::env::var_os("HYDROTERM_DEBUG_INPUT").is_some() {
                    eprintln!("[text] {text:?}");
                }
                self.on_text(text.as_str())
            }
            SurfaceInputEvent::CompositionStart => {
                if std::env::var_os("HYDROTERM_DEBUG_INPUT").is_some() {
                    eprintln!("[ime] start");
                }
                self.preedit = Some((String::new(), 0));
                self.clear_selection_on_input();
                true
            }
            SurfaceInputEvent::CompositionUpdate { text, caret } => {
                if std::env::var_os("HYDROTERM_DEBUG_INPUT").is_some() {
                    eprintln!("[ime] update text={text:?} caret={caret:?}");
                }
                self.preedit = Some((text.to_string(), caret.unwrap_or(text.len())));
                true
            }
            SurfaceInputEvent::CompositionCommit(text) => {
                if std::env::var_os("HYDROTERM_DEBUG_INPUT").is_some() {
                    eprintln!("[ime] commit {text:?}");
                }
                self.preedit = None;
                let _ = self.on_text(text.as_str());
                true
            }
            SurfaceInputEvent::CompositionCancel => {
                if std::env::var_os("HYDROTERM_DEBUG_INPUT").is_some() {
                    eprintln!("[ime] cancel");
                }
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
        let x = self.pad_x as f64 + col as f64 * m.cell_w as f64;
        let y = self.pad_y as f64 + row as f64 * m.cell_h as f64;
        Some(kurbo::Rect::new(x, y, x + 2.0, y + m.cell_h as f64))
    }

    fn accessibility_label(&self) -> Option<String> {
        Some("Terminal".to_string())
    }

    fn accessibility_value(&self) -> Option<String> {
        let term = self.session.terminal.term.lock();
        let grid = term.grid();
        let bottom = grid.screen_lines() as i32 - 1;
        // A minimal pane (1 row) must not walk scrollback rows that do not
        // exist — `bounds_to_string` indexes storage directly.
        let start = bottom.saturating_sub(5).max(-(grid.history_size() as i32));
        Some(term.bounds_to_string(
            Point::new(Line(start), Column(0)),
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

/// Scan `chars` (one grid row) for a scheme:// run or `link` regex
/// match covering `col`; `url_on=false` skips the scheme scan (the
/// `link-url` gate — custom patterns are independent). Returns the
/// matched text; wraps at row boundaries are not followed.
fn url_at(chars: &[char], col: usize, patterns: &[regex::Regex], url_on: bool) -> Option<String> {
    if url_on {
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
    }
    for (s, e) in regex_spans(chars, patterns) {
        if col >= s && col < e {
            return Some(chars[s..e].iter().collect());
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

/// Every scheme:// run and `link` regex match in `chars` as
/// `(start, end-exclusive)` column pairs; `url_on` gates the scheme
/// scan like `url_at`. Powers hover underline and URL hint mode.
fn url_spans(chars: &[char], patterns: &[regex::Regex], url_on: bool) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    if url_on {
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
    }
    spans.extend(regex_spans(chars, patterns));
    spans.sort_unstable();
    spans.dedup();
    spans
}

/// `link = <regex>` matches on the row as `(start, end)` char-column
/// pairs — regex byte ranges map back through the char offsets so
/// multi-byte cells land correctly.
fn regex_spans(chars: &[char], patterns: &[regex::Regex]) -> Vec<(usize, usize)> {
    if patterns.is_empty() {
        return Vec::new();
    }
    let text: String = chars.iter().collect();
    // Byte offset of each char; append one past the end for match ends.
    let mut offs: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
    offs.push(text.len());
    let to_char = |b: usize| offs.partition_point(|&o| o < b);
    let mut out = Vec::new();
    for re in patterns {
        for m in re.find_iter(&text) {
            let (s, e) = (to_char(m.start()), to_char(m.end()));
            if s < e {
                out.push((s, e));
            }
        }
    }
    out
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

/// `scroll_to_selection` — the `Scroll::Delta` that puts the
/// selection's start line at the top of the viewport: `start.line` is
/// grid-relative (negative in scrollback) and `display_offset` counts
/// lines scrolled above the live view, so the target offset is
/// `-start.line` clamped to the scrollback bounds.
fn scroll_to_selection_delta(start_line: i32, history: usize, display_offset: usize) -> i32 {
    let target = i64::from(-start_line).clamp(0, history as i64);
    (target - display_offset as i64) as i32
}

/// `cursor-click-to-move` — `delta` = click_col − cursor_col: positive
/// emits `delta` right-arrow sequences (`CSI C`), negative left (`CSI D`).
/// Individual arrows so line editors move their input cursor naturally.
fn cursor_click_seq(delta: i64) -> Vec<u8> {
    let mut out = Vec::new();
    let seq: &[u8] = if delta > 0 { b"\x1b[C" } else { b"\x1b[D" };
    for _ in 0..delta.abs() {
        out.extend_from_slice(seq);
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

/// OSC 9/777 → a freedesktop desktop notification through
/// `waterkit-notification` (D-Bus `org.freedesktop.Notifications`). A
/// session bus or daemon being absent just leaves the in-app bell/badge
/// path to carry the notification.
#[cfg(target_os = "linux")]
fn notify_desktop(title: &str, body: &str) {
    let title = if title.is_empty() { "hydroterm" } else { title }.to_string();
    let body = body.to_string();
    std::thread::spawn(move || {
        let _ = waterkit_notification::Notification::new()
            .title(title)
            .body(body)
            .app_name("hydroterm")
            .show();
    });
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
        assert_eq!(url_at(&row, 10, &[], true), Some("https://example.com/x".to_string()));
        assert_eq!(url_at(&row, 0, &[], true), None);
        assert_eq!(url_at(&row, 30, &[], true), None);
    }

    #[test]
    fn url_at_trims_trailing_punct() {
        let row = chars("open https://a.b/c, then");
        assert_eq!(url_at(&row, 6, &[], true), Some("https://a.b/c".to_string()));
    }

    #[test]
    fn url_at_trims_brackets() {
        let row = chars("[x](https://a.b/?q=(r)) ");
        // Click inside the link: parens stop the scan.
        assert_eq!(url_at(&row, 8, &[], true), Some("https://a.b/?q=".to_string()));
    }

    #[test]
    fn url_spans_finds_both_links() {
        let s = chars("open https://a.io/x then https://b.dev/y.");
        let spans = url_spans(&s, &[], true);
        assert_eq!(spans.len(), 2);
        assert_eq!(s[spans[0].0..spans[0].1].iter().collect::<String>(), "https://a.io/x");
        assert_eq!(s[spans[1].0..spans[1].1].iter().collect::<String>(), "https://b.dev/y");
    }

    #[test]
    fn url_at_second_of_two() {
        let row = chars("https://a.b/ and http://c.d/e");
        assert_eq!(url_at(&row, 22, &[], true), Some("http://c.d/e".to_string()));
    }

    #[test]
    fn link_regex_span() {
        // `link = GH-[0-9]+` matches a plain-text pattern with no
        // scheme; multi-byte chars before it keep byte→char mapping.
        let re = regex::Regex::new(r"GH-\d+").unwrap();
        let row = chars("fix GH-42 中GH-7 done");
        assert_eq!(url_at(&row, 6, std::slice::from_ref(&re), true), Some("GH-42".to_string()));
        // Inside the post-CJK match.
        assert_eq!(url_at(&row, 12, std::slice::from_ref(&re), true), Some("GH-7".to_string()));
        // `link-url = false` drops scheme matches but keeps patterns.
        let row = chars("see https://a.b/ GH-9");
        assert_eq!(url_at(&row, 5, std::slice::from_ref(&re), false), None);
        assert_eq!(url_at(&row, 18, std::slice::from_ref(&re), false), Some("GH-9".to_string()));
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
        let spans = url_spans(&lm.chars, &[], true);
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

    /// `scroll_to_selection`: the delta puts the selection start at the
    /// viewport top — positive scrolls up into scrollback, negative back
    /// toward live, clamped to the scrollback bounds.
    #[test]
    fn scroll_to_selection_delta_math() {
        // history 100, offset 40: start at grid line -80 → target 80.
        assert_eq!(scroll_to_selection_delta(-80, 100, 40), 40);
        // Start inside the screen (line 5) while scrolled → scroll to live.
        assert_eq!(scroll_to_selection_delta(5, 100, 40), -40);
        // Start above scrollback top clamps to the top.
        assert_eq!(scroll_to_selection_delta(-200, 100, 40), 60);
        // Already at the target → no movement.
        assert_eq!(scroll_to_selection_delta(-60, 100, 60), 0);
    }

    /// `cursor-click-to-move`: right clicks emit `\e[C`, left `\e[D`,
    /// one sequence per column of delta.
    #[test]
    fn cursor_click_seq_emits_arrows() {
        assert_eq!(cursor_click_seq(3), b"\x1b[C\x1b[C\x1b[C".to_vec());
        assert_eq!(cursor_click_seq(-2), b"\x1b[D\x1b[D".to_vec());
        assert!(cursor_click_seq(0).is_empty());
    }
}
