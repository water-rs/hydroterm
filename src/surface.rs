//! The `GpuView` that hosts a terminal session: input routing (keyboard,
//! IME, pointer, scroll), the PTY event drain, resize bookkeeping, and the
//! Vello/hybrid rasterization plumbing modeled on `SceneSurfaceRenderer`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::{TermMode, viewport_to_point};
use waterui_core::layout::{ProposalSize, Size, StretchAxis, ViewDimensions};
use waterui_core::{Environment, Str};
use waterui_graphics::input::{ScrollUnit, SurfaceInputEvent, SurfacePointerButton};
use waterui_graphics::scene2d::Scene2D;
use waterui_graphics::scene2d_hybrid::{HybridScene2D, HybridUpload};
use waterui_graphics::scene2d_vello::VelloScene2D;
use waterui_graphics::shared_context::{SceneEngine, SharedSceneRenderer};
use waterui_graphics::shaders::BLIT;
use waterui_graphics::{Code, GpuContext, GpuFrame, GpuView, Key, Modifiers, NamedKey};

use crate::app::{AppState, FONT_SIZE, Session};
use crate::fonts::FontStack;
use crate::keys::{TermAction, action_chord, key_release_bytes, key_to_bytes, tab_chord};
use crate::mouse::{self, CellPos, MouseAction};
use crate::palette::Palette;
use std::cell::RefCell;
use std::rc::Rc;
use crate::scene::{self, CursorInfo, DrawContext, PADDING, ScrollInfo, cursor_info};
use crate::osctap::TapEvent;
use crate::terminal::TermEvent;

/// Blink half-period for the cursor.
const BLINK_HALF: Duration = Duration::from_millis(530);
/// Bell flash decay time.
const BELL_FLASH_SECS: f32 = 0.15;
/// Time window for double/triple click detection.
const MULTI_CLICK: Duration = Duration::from_millis(400);
/// Max cell distance for a multi-click to count as same-cell.
const MULTI_CLICK_RANGE: usize = 1;

/// The engine's scene storage — mirrors `SceneSurfaceRenderer`.
enum SceneBuf {
    /// Compute pipeline → intermediate storage texture → blit.
    Classic(Box<vello::Scene>),
    /// CPU/GPU split pipeline, straight into the frame.
    Hybrid(Box<vello_hybrid::Scene>),
}

/// Blit plumbing for the classic path.
struct Blit {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

/// In-surface text search state (Ctrl+Shift+F).
struct Search {
    query: String,
    /// (col, grid line) — grid lines go negative into scrollback.
    matches: Vec<(usize, i32)>,
    active: usize,
}

/// One terminal surface — renderer + input owner for a session.
pub struct TermSurface {
    session: Rc<Session>,
    app: AppState,
    fonts: FontStack,
    /// Shared palette — swapped on theme reload.
    palette: Rc<RefCell<Palette>>,
    font_size_pt: f32,

    // GPU plumbing
    scene: Option<SceneBuf>,
    renderer: Option<Arc<SharedSceneRenderer>>,
    blit: Option<Blit>,
    intermediate: Option<(wgpu::Texture, wgpu::TextureView)>,
    inter_size: (u32, u32),

    // geometry (logical→physical scale from the last frame)
    cols: u16,
    lines: u16,
    scale: f64,

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
    blink_epoch: Instant,
    search: Option<Search>,
    clipboard: Option<waterkit_clipboard::Clipboard>,
}

impl TermSurface {
    /// Wrap a session in a GPU surface renderer.
    pub fn new(session: Rc<Session>, app: AppState, palette: Rc<RefCell<Palette>>) -> Self {
        let font_size = session.font_size.get();
        Self {
            session,
            app,
            fonts: FontStack::load(font_size, 1.0),
            palette,
            font_size_pt: font_size,
            scene: None,
            renderer: None,
            blit: None,
            intermediate: None,
            inter_size: (0, 0),
            cols: 0,
            lines: 0,
            scale: 1.0,
            focused: false,
            modifiers: Modifiers::empty(),
            held_button: None,
            selecting: false,
            last_click: None,
            preedit: None,
            scroll_accum_px: 0.0,
            bell_at: None,
            blink_epoch: Instant::now(),
            search: None,
            clipboard: waterkit_clipboard::Clipboard::new().ok(),
        }
    }

    /// Logical pointer position → (col, row) in viewport coords.
    fn viewport_cell(&self, x: f64, y: f64) -> (usize, usize) {
        let m = self.fonts.metrics;
        let pad = PADDING as f64 * m.scale;
        let (cw, ch) = (m.cell_w as f64, m.cell_h as f64);
        let col = ((x * self.scale - pad) / cw).clamp(0.0, self.cols.saturating_sub(1) as f64) as usize;
        let row = ((y * self.scale - pad) / ch).clamp(0.0, self.lines.saturating_sub(1) as f64) as usize;
        (col, row)
    }

    /// Logical pointer position → grid `Point` (scrollback-aware).
    fn grid_point(&self, x: f64, y: f64) -> Point {
        let (col, row) = self.viewport_cell(x, y);
        let offset = self.session.terminal.term.lock().grid().display_offset();
        viewport_to_point(offset, Point::new(row, Column(col)))
    }

    /// Which side of a cell the pointer is on (for selection anchors).
    fn cell_side(&self, x: f64) -> Side {
        let m = self.fonts.metrics;
        let pad = PADDING as f64 * m.scale;
        let cw = m.cell_w as f64;
        let within = (x * self.scale - pad).rem_euclid(cw);
        if within < cw * 0.5 { Side::Left } else { Side::Right }
    }

    fn write(&self, bytes: impl Into<std::borrow::Cow<'static, [u8]>>) {
        self.session.terminal.write(bytes);
    }

    /// Clipboard text → PTY, with bracketed-paste markers when armed.
    fn paste_clipboard(&mut self) {
        let Some(clip) = &self.clipboard else { return };
        if let Ok(Some(text)) = pollster::block_on(clip.text()) {
            let bracketed =
                self.session.terminal.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
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
        }
    }

    fn copy_selection(&mut self) {
        let text = self.session.terminal.term.lock().selection_to_string();
        if let (Some(text), Some(clip)) = (text, self.clipboard.as_mut()) {
            let _ = clip.set_text(&text);
        }
    }

    /// Open the OSC8 hyperlink under `point` in the system browser.
    fn open_link_at(&self, point: Point) -> bool {
        let term = self.session.terminal.term.lock();
        let uri = term.grid()[point].hyperlink().map(|h| h.uri().to_string());
        drop(term);
        if let Some(uri) = uri {
            // Detached open — we never wait on the launcher.
            let _ = std::process::Command::new("xdg-open")
                .arg(&uri)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            return true;
        }
        false
    }

    /// Apply a chord action (copy/paste/tabs/font/search).
    fn do_action(&mut self, action: TermAction) {
        match action {
            TermAction::Copy => self.copy_selection(),
            TermAction::Paste => self.paste_clipboard(),
            TermAction::NewTab => {
                self.app.new_tab();
            }
            TermAction::CloseTab => self.app.close_pane(self.session.id),
            TermAction::NextTab => self.app.cycle_tab(1),
            TermAction::PrevTab => self.app.cycle_tab(-1),
            TermAction::SelectTab(n) => self.app.select_tab(n),
            TermAction::FontBigger | TermAction::FontSmaller | TermAction::FontReset => {
                let cur = self.session.font_size.get();
                let next = match action {
                    TermAction::FontBigger => (cur + 1.0).min(96.0),
                    TermAction::FontSmaller => (cur - 1.0).max(6.0),
                    _ => FONT_SIZE,
                };
                self.session.font_size.set(next);
            }
            TermAction::ClearScrollback => {
                let mut term = self.session.terminal.term.lock();
                term.grid_mut().clear_history();
                term.scroll_display(Scroll::Bottom);
            }
            TermAction::Search => {
                self.search = Some(Search {
                    query: String::new(),
                    matches: Vec::new(),
                    active: 0,
                });
            }
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
            TermAction::Quit => self.app.quit(),
            TermAction::SplitRight => {
                self.app.split_pane(crate::app::SplitDir::Row, self.session.id);
            }
            TermAction::SplitDown => {
                self.app.split_pane(crate::app::SplitDir::Column, self.session.id);
            }
            TermAction::FocusNextPane => self.app.cycle_pane(1),
            TermAction::FocusPrevPane => self.app.cycle_pane(-1),
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
        let &(_, line) = search.matches.get(search.active)?;
        // Target display_offset so the match sits mid-viewport.
        Some(-line + self.lines as i32 / 2)
    }

    /// Search the whole buffer for `query`; fills `matches`, scrolls to #1.
    fn run_search(&mut self) {
        let Some(search) = &mut self.search else { return };
        search.matches.clear();
        if search.query.is_empty() {
            return;
        }
        let query = search.query.to_lowercase();
        let term = self.session.terminal.term.lock();
        let grid = term.grid();
        let top = -(grid.history_size() as i32);
        let bottom = grid.screen_lines() as i32 - 1;
        for line in top..=bottom {
            let start = Point::new(Line(line), Column(0));
            let end = Point::new(Line(line), grid.last_column());
            let text = term.bounds_to_string(start, end).to_lowercase();
            for (idx, _) in text.match_indices(&query) {
                let col = text[..idx].chars().count();
                search.matches.push((col, line));
            }
        }
        search.active = 0;
        drop(term);
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
                    self.app.set_session_title(self.session.id, t);
                }
                TermEvent::ClipboardStore(_ty, text) => {
                    if let Some(clip) = self.clipboard.as_mut() {
                        let _ = clip.set_text(&text);
                    }
                }
                TermEvent::ClipboardLoad(_ty, fmt) => {
                    let text = self
                        .clipboard
                        .as_ref()
                        .and_then(|c| pollster::block_on(c.text()).ok().flatten())
                        .unwrap_or_default();
                    self.write(fmt(&text).into_bytes());
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
                    self.bell_at = Some(Instant::now());
                }
                TermEvent::ChildExit(_status) => {
                    self.session.exited.set(true);
                }
                TermEvent::Exit => {
                    self.app.close_tab(self.session.id);
                }
                TermEvent::Tap(tap) => match tap {
                    // Marks are recorded on the reader thread where the
                    // cursor still sits at the mark position.
                    TapEvent::PromptStart => {}
                    TapEvent::Cwd(path) => {
                        *self.session.cwd.lock().unwrap() = Some(path);
                    }
                    TapEvent::PromptEnd | TapEvent::CommandStart => {}
                    TapEvent::CommandEnd(_code) => {}
                    TapEvent::Notify(_title, _body) => {
                        // No system-notify path yet — tracked as a feedback item.
                    }
                    TapEvent::Apc(_payload) => {
                        // kitty graphics land here once the decoder lands.
                    }
                },
            }
        }
    }

    /// Rebuild the font stack when size or scale changed.
    fn sync_fonts(&mut self, scale: f64) {
        let want = self.session.font_size.get();
        if (want - self.font_size_pt).abs() > f32::EPSILON
            || (self.fonts.metrics.scale - scale).abs() > f64::EPSILON
        {
            self.font_size_pt = want;
            self.fonts = FontStack::load(want, scale);
        }
    }

    /// Recompute the grid from the frame size and propagate resizes.
    fn sync_size(&mut self, width_px: u32, height_px: u32) {
        let m = self.fonts.metrics;
        let pad = PADDING * m.scale as f32 * 2.0;
        let cols = ((width_px as f32 - pad) / m.cell_w).floor().max(2.0) as u16;
        let lines = ((height_px as f32 - pad) / m.cell_h).floor().max(1.0) as u16;
        // Degenerate frames (window unmapped/collapsed) must not shrink the
        // PTY — a 1-line winsize breaks apps that read TIOCGWINSZ at start.
        if cols > 2 && lines > 1 && (cols != self.cols || lines != self.lines) {
            self.cols = cols;
            self.lines = lines;
            self.session
                .terminal
                .resize(cols, lines, (m.cell_w as u16, m.cell_h as u16));
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

    fn on_key(&mut self, pressed: bool, key: &Key, code: Code, mods: Modifiers) {
        // Search mode captures keys into the query.
        if pressed && self.search.is_some() && self.search_key(key, mods) {
            return;
        }
        let mode = *self.session.terminal.term.lock().mode();
        if pressed {
            // Config keybinds first — they may re-map or disable defaults.
            match self.app.config(|c| c.lookup_keybind(key, mods)) {
                Some(Some(action)) => {
                    self.do_action(action);
                    return;
                }
                Some(None) => return, // explicitly disabled
                None => {}
            }
            if let Some(action) = action_chord(key, mods).or_else(|| tab_chord(key, code, mods)) {
                self.do_action(action);
                return;
            }
            if let Some(bytes) = key_to_bytes(key, code, mods, mode) {
                self.write(bytes);
            }
        } else if let Some(bytes) = key_release_bytes(key, mods, mode) {
            self.write(bytes);
        }
    }

    /// Feed one key into search state. Returns true when consumed.
    fn search_key(&mut self, key: &Key, _mods: Modifiers) -> bool {
        match key {
            Key::Named(NamedKey::Enter) => {
                // Enter: jump to next match.
                if let Some(s) = &mut self.search && !s.matches.is_empty() {
                    s.active = (s.active + 1) % s.matches.len();
                }
                if let Some(target) = self.search_scroll_target() {
                    let cur = self.session.terminal.term.lock().grid().display_offset() as i32;
                    self.session
                        .terminal
                        .term
                        .lock()
                        .scroll_display(Scroll::Delta(target - cur));
                }
                true
            }
            Key::Named(NamedKey::Escape) => {
                self.search = None;
                true
            }
            Key::Named(NamedKey::Backspace) => {
                if let Some(s) = &mut self.search {
                    s.query.pop();
                }
                self.run_search();
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
            _ => {
                // Printable keys append to the query — TextInput also arrives,
                // so here we only consume to suppress the PTY path.
                true
            }
        }
    }

    /// Append typed text — search query when searching, else PTY.
    fn on_text(&mut self, text: &str) {
        if self.search.is_some() {
            if let Some(s) = &mut self.search {
                s.query.push_str(text);
            }
            self.run_search();
            return;
        }
        self.write(text.as_bytes().to_vec());
    }

    fn on_pointer_move(&mut self, x: f64, y: f64) {
        let (col, row) = self.viewport_cell(x, y);
        let mode = *self.session.terminal.term.lock().mode();

        if mode.intersects(TermMode::MOUSE_MODE) {
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
            return;
        }

        if self.selecting {
            let point = self.grid_point(x, y);
            let side = self.cell_side(x);
            let mut term = self.session.terminal.term.lock();
            if let Some(sel) = &mut term.selection {
                sel.update(point, side);
            }
        }
    }

    fn on_pointer_button(&mut self, pressed: bool, button: SurfacePointerButton, x: f64, y: f64) {
        let (col, row) = self.viewport_cell(x, y);
        let mode = *self.session.terminal.term.lock().mode();

        if mode.intersects(TermMode::MOUSE_MODE) {
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
            match button {
                SurfacePointerButton::Primary => {
                    // Ctrl+click opens an OSC8 link.
                    if self.modifiers.contains(Modifiers::CONTROL)
                        && self.open_link_at(self.grid_point(x, y))
                    {
                        return;
                    }
                    // Alt+drag = block selection; otherwise multi-click by timing.
                    let now = Instant::now();
                    let count = self
                        .last_click
                        .filter(|(t, r, c, _)| {
                            now.duration_since(*t) < MULTI_CLICK
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
                    self.paste_clipboard();
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
            } else if self.app.config(|c| c.copy_on_select) {
                drop(term);
                self.copy_selection();
            }
        }
    }

    fn click_count_reset(&mut self, count: u8) {
        // Beyond triple-click the count wraps back to a simple drag.
        if count > 3 {
            self.last_click = None;
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
        };

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

        let mut term = self.session.terminal.term.lock();
        term.scroll_display(Scroll::Delta(lines_delta as i32));
    }

    // -- scene plumbing -------------------------------------------------------

    /// Draw the terminal into the active scene.
    fn build(&mut self, scene: &mut dyn Scene2D, width: f32, height: f32) {
        let matches_view: Vec<(usize, usize)> = self
            .search
            .as_ref()
            .map(|s| {
                let offset = self.session.terminal.term.lock().grid().display_offset() as i32;
                s.matches
                    .iter()
                    .map(|&(c, line)| (c, (line + offset) as usize))
                    .filter(|&(_, r)| r < self.lines as usize)
                    .collect()
            })
            .unwrap_or_default();
        let active = self.search.as_ref().and_then(|s| {
            let offset = self.session.terminal.term.lock().grid().display_offset() as i32;
            s.matches
                .get(s.active)
                .map(|&(c, line)| (c, (line + offset) as usize))
        });
        // Prepend the live query as a synthetic match-free banner: draw it via
        // preedit when searching (the search bar lives in the preedit slot).
        let preedit = self.preedit.clone().or_else(|| {
            self.search
                .as_ref()
                .map(|s| (format!("/{}", s.query), s.query.len()))
        });

        let term = self.session.terminal.term.lock();
        let grid = term.grid();
        let scroll = ScrollInfo {
            display_offset: grid.display_offset(),
            history_size: grid.history_size(),
            screen_lines: grid.screen_lines(),
        };
        let palette = self.palette.borrow();
        let bell_alpha = self
            .bell_at
            .map(|t| (1.0 - t.elapsed().as_secs_f32() / BELL_FLASH_SECS).max(0.0) * 0.18)
            .unwrap_or(0.0);

        let blink_on = self.blink_on();
        let focused = self.focused;
        let mut ctx = DrawContext {
            palette: &palette,
            fonts: &mut self.fonts,
            width,
            height,
            blink_on,
            focused,
            preedit,
            scroll,
            search_matches: &matches_view,
            search_active: active,
            bell_flash: bell_alpha,
        };
        scene::draw_term(scene, &term, &mut ctx);
    }

    /// Classic path: rasterize into an intermediate texture, then blit.
    fn render_classic(
        &mut self,
        renderer: &SharedSceneRenderer,
        frame: &mut GpuFrame,
    ) {
        let mut buf = self.scene.take().expect("scene used before setup");
        let SceneBuf::Classic(scene) = &mut buf else {
            panic!("TermSurface built a hybrid scene for the classic engine");
        };

        if self.inter_size != (frame.width, frame.height) {
            let texture = frame.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("terminal intermediate texture"),
                size: wgpu::Extent3d {
                    width: frame.width,
                    height: frame.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.intermediate = Some((texture, view));
            self.inter_size = (frame.width, frame.height);
        }
        // A fresh view each frame keeps `self` free for `build` to borrow.
        let intermediate_view = self
            .intermediate
            .as_ref()
            .expect("intermediate missing")
            .0
            .create_view(&wgpu::TextureViewDescriptor::default());

        scene.reset();
        {
            let mut scene2d = VelloScene2D::new(scene);
            self.build(&mut scene2d, frame.width as f32, frame.height as f32);
        }

        renderer.with_classic(frame.device, |renderer| {
            renderer
                .render_to_texture(
                    frame.device,
                    frame.queue,
                    scene,
                    &intermediate_view,
                    &vello::RenderParams {
                        base_color: peniko::Color::TRANSPARENT,
                        width: frame.width,
                        height: frame.height,
                        antialiasing_method: vello::AaConfig::Area,
                    },
                )
                .expect("terminal vello render failed");
        });

        let blit = self.blit.as_ref().expect("blit missing");
        let bind_group = frame.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("terminal blit bind group"),
            layout: &blit.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&intermediate_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&blit.sampler),
                },
            ],
        });

        let mut encoder = frame
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("terminal blit encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("terminal blit pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &frame.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&blit.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..6, 0..1);
        }
        frame.queue.submit([encoder.finish()]);
        self.scene = Some(buf);
    }

    /// Hybrid path: CPU preprocess + render pass straight into the frame.
    fn render_hybrid(
        &mut self,
        renderer: &SharedSceneRenderer,
        frame: &mut GpuFrame,
    ) {
        let mut buf = self.scene.take().expect("scene used before setup");
        let SceneBuf::Hybrid(scene) = &mut buf else {
            panic!("TermSurface built a classic scene for the hybrid engine");
        };

        let width = u16::try_from(frame.width).expect("surface wider than a hybrid scene");
        let height = u16::try_from(frame.height).expect("surface taller than a hybrid scene");
        scene.reset_and_resize(width, height);

        let mut encoder = frame
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("terminal hybrid encoder"),
            });
        drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("terminal hybrid clear pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &frame.view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        }));

        renderer.with_hybrid(frame.device, frame.format, |hybrid| {
            {
                let upload = HybridUpload::new(frame.device, frame.queue, &mut encoder);
                let mut scene2d = HybridScene2D::new(scene, hybrid, upload);
                self.build(&mut scene2d, frame.width as f32, frame.height as f32);
            }
            hybrid
                .renderer
                .render(
                    scene,
                    &mut hybrid.resources,
                    frame.device,
                    frame.queue,
                    &mut encoder,
                    &vello_hybrid::RenderSize {
                        width: frame.width,
                        height: frame.height,
                    },
                    &frame.view,
                    &vello_hybrid::TextureBindings::new(),
                )
                .expect("terminal hybrid render failed");
        });

        frame.queue.submit([encoder.finish()]);
        self.scene = Some(buf);
    }
}

impl GpuView for TermSurface {
    async fn setup(&mut self, ctx: &GpuContext<'_>, _env: &mut Environment) {
        // Wake this surface whenever the parser has output or events.
        let handle = ctx.redraw_handle.clone();
        self.session.terminal.proxy.set_wake(move || handle.request_redraw());

        self.renderer = Some(Arc::clone(ctx.scene_renderer()));
        if ctx.scene_renderer().engine() == SceneEngine::Hybrid {
            self.scene = Some(SceneBuf::Hybrid(Box::new(vello_hybrid::Scene::new(1, 1))));
            return;
        }
        self.scene = Some(SceneBuf::Classic(Box::new(vello::Scene::new())));

        let (vs, fs) =
            BLIT.create_render_stages(ctx.device, "vs_main", "fs_main");
        let layout = ctx
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("terminal blit bind group layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });
        let pipeline_layout = ctx
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("terminal blit pipeline layout"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });
        let pipeline = ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("terminal blit pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: vs.module(),
                entry_point: Some(vs.entry_point()),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: fs.module(),
                entry_point: Some(fs.entry_point()),
                targets: &[Some(wgpu::ColorTargetState {
                    format: ctx.surface_format,
                    blend: ctx.alpha_blend_state(),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = ctx.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("terminal blit sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        self.blit = Some(Blit { pipeline, layout, sampler });
    }

    fn render(&mut self, frame: &mut GpuFrame) {
        self.app.poll_config();
        self.drain_events();
        self.sync_fonts(frame.scale());
        self.sync_size(frame.width, frame.height);
        self.scale = frame.scale();

        // A blinking cursor or live bell flash needs the next frame anyway;
        // the wake callback covers PTY output between frames.
        let cursor_blinking = self.session.terminal.term.lock().cursor_style().blinking;
        let bell_live = self
            .bell_at
            .is_some_and(|t| t.elapsed() < Duration::from_secs_f32(BELL_FLASH_SECS));
        if cursor_blinking || bell_live || self.search.is_some() {
            frame.request_redraw();
        }

        let renderer = Arc::clone(self.renderer.as_ref().expect("renderer used before setup"));
        match renderer.engine() {
            SceneEngine::Classic => self.render_classic(&renderer, frame),
            SceneEngine::Hybrid => self.render_hybrid(&renderer, frame),
        }
    }

    fn wants_input_events(&self) -> bool {
        true
    }

    fn input(&mut self, event: &SurfaceInputEvent) {
        if std::env::var_os("HYDROTERM_DEBUG_INPUT").is_some() {
            eprintln!("[input] {event:?}");
        }
        match event {
            SurfaceInputEvent::Focus(gained) => self.on_focus(*gained),
            SurfaceInputEvent::Modifiers(mods) => self.modifiers = *mods,
            SurfaceInputEvent::PointerMove { position } => {
                self.on_pointer_move(position.x, position.y);
            }
            SurfaceInputEvent::PointerButton { pressed, button, position } => {
                self.on_pointer_button(*pressed, *button, position.x, position.y);
            }
            SurfaceInputEvent::Scroll { position, delta_x, delta_y, unit, .. } => {
                self.on_scroll(position.x, position.y, *delta_x, *delta_y, *unit);
            }
            SurfaceInputEvent::Key { pressed, key, code, modifiers, repeat: _ } => {
                self.on_key(*pressed, key, *code, *modifiers);
            }
            SurfaceInputEvent::TextInput(text) => self.on_text(text.as_str()),
            SurfaceInputEvent::CompositionStart => {
                self.preedit = Some((String::new(), 0));
            }
            SurfaceInputEvent::CompositionUpdate { text, caret } => {
                self.preedit = Some((text.to_string(), caret.unwrap_or(text.len())));
            }
            SurfaceInputEvent::CompositionCommit(text) => {
                self.preedit = None;
                self.on_text(text.as_str());
            }
            SurfaceInputEvent::CompositionCancel => {
                self.preedit = None;
            }
        }
    }

    /// IME window position: the cell under the caret, in surface coordinates.
    fn ime_caret(&self) -> Option<kurbo::Rect> {
        let term = self.session.terminal.term.lock();
        let CursorInfo { row, col, .. } = cursor_info(&term);
        if row < 0 {
            return None;
        }
        let m = self.fonts.metrics;
        // Convert physical-px metrics back to logical coordinates.
        let cw = m.cell_w as f64 / m.scale;
        let ch = m.cell_h as f64 / m.scale;
        let x = PADDING as f64 + col as f64 * cw;
        let y = PADDING as f64 + row as f64 * ch;
        Some(kurbo::Rect::new(x, y, x + 2.0, y + ch))
    }

    fn is_opaque(&self) -> bool {
        true
    }

    fn measure(&self, proposal: ProposalSize) -> ViewDimensions {
        // Fill whatever the tab gives us; report the proposal back.
        ViewDimensions::new(Size::new(
            proposal.width.unwrap_or(640.0),
            proposal.height.unwrap_or(400.0),
        ))
    }

    fn stretch_axis(&self) -> StretchAxis {
        StretchAxis::Both
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
