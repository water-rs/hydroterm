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
use std::time::{Duration, Instant};

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::{TermMode, viewport_to_point};
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
use crate::scene::{self, CursorInfo, DrawContext, PADDING, ScrollInfo, cursor_info};
use crate::terminal::TermEvent;

/// Blink half-period for the cursor.
const BLINK_HALF: Duration = Duration::from_millis(530);
/// Bell flash decay time.
const BELL_FLASH_SECS: f32 = 0.15;
/// Time window for double/triple click detection.
const MULTI_CLICK: Duration = Duration::from_millis(400);
/// Max cell distance for a multi-click to count as same-cell.
const MULTI_CLICK_RANGE: usize = 1;

/// In-surface text search state (Ctrl+Shift+F).
struct Search {
    query: String,
    /// (col, grid line) — grid lines go negative into scrollback.
    matches: Vec<(usize, i32)>,
    active: usize,
}

/// One terminal surface — scene content + input owner for a session.
pub struct TermSurface {
    session: Rc<Session>,
    app: AppState,
    fonts: TermFonts,
    /// Shared palette — swapped on theme reload.
    palette: Rc<RefCell<Palette>>,
    font_size_pt: f32,

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
    blink_epoch: Instant,
    search: Option<Search>,
    clipboard: Option<waterkit_clipboard::Clipboard>,
}

impl TermSurface {
    /// Scene content for one session — the host's shared font collection is
    /// resolved once here, per pane.
    pub fn new(
        session: Rc<Session>,
        app: AppState,
        palette: Rc<RefCell<Palette>>,
        fonts: FontCollection,
    ) -> Self {
        let font_size = session.font_size.get();
        Self {
            session,
            app,
            fonts: TermFonts::load(fonts, font_size),
            palette,
            font_size_pt: font_size,
            cols: 0,
            lines: 0,
            invalidator: None,
            wake_tx: None,
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
            blink_epoch: Instant::now(),
            search: None,
            clipboard: waterkit_clipboard::Clipboard::new().ok(),
        }
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

    /// Open the link under `point`: an OSC8 hyperlink first, then a
    /// plain-text URL scanned off the row (like xterm/kitty Ctrl+click).
    fn open_link_at(&self, point: Point) -> bool {
        let term = self.session.terminal.term.lock();
        let uri = term.grid()[point].hyperlink().map(|h| h.uri().to_string());
        let uri = uri.or_else(|| {
            let chars: Vec<char> = term.grid()[point.line]
                .into_iter()
                .filter(|c| !c.flags.contains(Flags::WIDE_CHAR_SPACER))
                .map(|c| c.c)
                .collect();
            url_at(&chars, point.column.0)
        });
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
            TermAction::Fullscreen => self.app.toggle_fullscreen(),
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

    /// Sync local search state with the session bindings: the WaterUI
    /// search bar owns open/close and the query text; the surface owns the
    /// match list and which match is active. Called every frame while open.
    fn sync_search(&mut self) {
        let open = self.session.search_open.get();
        if open != self.search.is_some() {
            self.search = open.then_some(Search {
                query: String::new(),
                matches: Vec::new(),
                active: 0,
            });
        }
        let Some(s) = &self.search else { return };
        let q = self.session.search_query.get().to_string();
        if s.query != q {
            self.search.as_mut().unwrap().query = q;
            self.run_search();
        }
    }

    /// Search the whole buffer for `query`; fills `matches`, scrolls to #1.
    fn run_search(&mut self) {
        let Some(search) = &mut self.search else { return };
        search.matches.clear();
        if search.query.is_empty() {
            self.session.search_status.set_from("");
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
        let n = search.matches.len();
        self.session
            .search_status
            .set_from(if n == 0 { "no matches".to_string() } else { format!("{n} matches") });
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
                    *self.session.base_title.lock().unwrap() = t.clone();
                    // A 🔔 badge holds the title until user attention clears
                    // it — the real title keeps accumulating in base_title.
                    if !*self.session.notify_badge.lock().unwrap() {
                        self.app.set_session_title(self.session.id, t);
                    }
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
                TermEvent::Apc(payload, line, col) => {
                    self.handle_apc(&payload, line, col);
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
                    TapEvent::Notify(title, body) => {
                        // No desktop-notification channel yet — flash the
                        // bell and badge the title until the next prompt.
                        self.bell_at = Some(Instant::now());
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

    /// Re-measure fonts when the size changes.
    fn sync_fonts(&mut self) {
        let want = self.session.font_size.get();
        if (want - self.font_size_pt).abs() > f32::EPSILON {
            self.font_size_pt = want;
            self.fonts.resize(want);
        }
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

    fn on_key(&mut self, pressed: bool, key: &Key, code: Code, mods: Modifiers) {
        if pressed {
            self.clear_notify_badge();
        }
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
                self.session.search_open.set(false);
                true
            }
            Key::Named(NamedKey::Backspace) => {
                let mut q = self.session.search_query.get().to_string();
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
            let mut q = self.session.search_query.get().to_string();
            q.push_str(text);
            self.session.search_query.set_from(q);
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
            self.clear_notify_badge();
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

    /// Draw the terminal into the frame's scene.
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
        let preedit = self.preedit.clone();

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
        let top = scroll.history_size as i64 - scroll.display_offset as i64;
        let m = self.fonts.metrics;
        let pad = PADDING;
        for img in &self.session.kitty.borrow().images {
            let row = img.line - top;
            let rows = if img.rows > 0 {
                img.rows as i64
            } else {
                (img.px_h as f32 / m.cell_h).ceil() as i64
            };
            if row + rows < 0 || row >= self.lines as i64 {
                continue;
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

impl SceneContent for TermSurface {
    fn build_scene(&mut self, scene: &mut dyn Scene2D, width: f32, height: f32) -> bool {
        self.app.poll_config();
        self.drain_events();
        self.sync_search();
        self.sync_fonts();
        self.sync_size(width, height);

        // A blinking cursor or live bell flash needs the next frame anyway;
        // the wake pipe covers PTY output between frames.
        let cursor_blinking = self.session.terminal.term.lock().cursor_style().blinking;
        let bell_live = self
            .bell_at
            .is_some_and(|t| t.elapsed() < Duration::from_secs_f32(BELL_FLASH_SECS));

        self.build(scene, width, height);

        cursor_blinking || bell_live || self.search.is_some()
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
                let alive = Rc::clone(&self.wake_alive);
                self.wake_task = Some(Box::pin(spawn_local(async move {
                    while rx.recv().await.is_ok() {
                        if !alive.get() {
                            break;
                        }
                        // Coalesce bursts: one invalidation per batch.
                        while rx.try_recv().is_ok() {}
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
        // Delivering an event does not schedule a frame — anything the
        // handler changed (selection, scroll offset, focus, preedit) paints
        // on the next requested one.
        if let Some(invalidator) = &self.invalidator {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
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
    fn url_at_second_of_two() {
        let row = chars("https://a.b/ and http://c.d/e");
        assert_eq!(url_at(&row, 22), Some("http://c.d/e".to_string()));
    }
}
