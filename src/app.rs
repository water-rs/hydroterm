//! Application state: sessions, tabs, split-pane trees, focus tracking,
//! and the actions surfaces trigger (new/close/cycle, splits, clipboard).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use alacritty_terminal::term::Config;
use alacritty_terminal::tty::Shell;
use alacritty_terminal::vte::ansi::CursorStyle;
use nami::collection::List as NamiList;
use nami::{Binding, binding};
use waterui::impl_extractor;
use waterui::Identifiable;
use waterui_core::id::SelfId;
use waterui::layout::frame::Frame;
use waterui::prelude::*;
use waterui::widget::condition::when;
use waterui::window::{Window, WindowState, WindowStyle, conditional_window};
use waterui::window::WindowPresentation;
use waterui::task::spawn_local;
use waterui_core::layout::{Point, Rect, Size};
use waterui_graphics::SceneView;
use waterui::theme::color::{Accent, Background, Foreground, MutedForeground, Surface};
use waterui_graphics::color::{Color, Srgb, signal_color};
use waterui_text::FontCollection;

use crate::config::{AppConfig, ConfigWatcher};
use crate::keys::TermAction;
use crate::palette::Palette;
use crate::surface::TermSurface;
use crate::terminal::Terminal;
use waterui::form::picker::picker;

/// Default font size in points (the config file may override).
pub const FONT_SIZE: f32 = 13.0;

/// Fixed height of the tab strip at every window size.
const TAB_STRIP_HEIGHT: f32 = 30.0;

/// Shared per-session UI state: the bindings a pane surface reads, plus
/// the owning `Terminal` (PTY + grid).
pub struct Session {
    pub id: u64,
    /// The terminal (Term + EventLoop + PTY channel).
    pub terminal: Arc<Terminal>,
    /// OSC-set title (propagates to the tab when the pane is focused).
    pub title: Binding<Str>,
    /// Last OSC 0/2 title — restored when a 🔔 notification badge clears.
    pub base_title: std::sync::Mutex<Str>,
    /// A 🔔 badge is currently overriding the title.
    pub notify_badge: std::sync::Mutex<bool>,
    /// Current font size in points.
    pub font_size: Binding<f32>,
    /// Child process exited.
    pub exited: Binding<bool>,
    /// Latest working directory reported via OSC 7.
    pub cwd: std::sync::Mutex<Option<std::path::PathBuf>>,
    /// Search bar visible above the pane (Ctrl+Shift+F toggles).
    pub search_open: Binding<bool>,
    /// Live search query — bound to the WaterUI `TextField`.
    pub search_query: Binding<Str>,
    /// Match summary shown next to the field ("3 matches" / "").
    pub search_status: Binding<Str>,
    /// kitty graphics placements transmitted on this session.
    pub kitty: Rc<RefCell<crate::kitty::KittyStore>>,
    /// Actions queued by the command palette — drained by the surface on
    /// the next frame (keeps one dispatch path for every action).
    pub pending_actions: Rc<RefCell<Vec<TermAction>>>,
}

impl Session {
    fn spawn(id: u64, cwd: Option<std::path::PathBuf>, cfg: &AppConfig) -> Self {
        let config = Config {
            scrolling_history: cfg.scrollback,
            kitty_keyboard: true,
            default_cursor_style: CursorStyle {
                shape: cfg.cursor_shape,
                blinking: cfg.cursor_blink,
            },
            ..Default::default()
        };
        // `-e` > `shell =` > auto-injected shell integration.
        let shell = if let Some(cmd) = &cfg.command {
            Some(Shell::new(cmd[0].clone(), cmd[1..].to_vec()))
        } else {
            cfg.shell
                .as_ref()
                .map(|s| Shell::new(s.clone(), Vec::<String>::new()))
        };
        // A reasonable initial grid; the surface resizes on its first frame.
        let terminal = Terminal::spawn(config, 120, 32, (9, 18), cwd, shell)
            .expect("failed to spawn PTY — is a shell available?");
        Self {
            id,
            terminal: Arc::new(terminal),
            title: binding(Str::from("Shell")),
            base_title: std::sync::Mutex::new(Str::from("Shell")),
            notify_badge: std::sync::Mutex::new(false),
            font_size: Binding::f32(cfg.font_size),
            exited: Binding::bool(false),
            cwd: std::sync::Mutex::new(None),
            search_open: Binding::bool(false),
            search_query: binding(Str::from("")),
            search_status: binding(Str::from("")),
            kitty: Rc::new(RefCell::new(crate::kitty::KittyStore::default())),
            pending_actions: Rc::new(RefCell::new(Vec::new())),
        }
    }
}

/// Split axis: `Row` stacks panes side by side (split-right),
/// `Column` stacks them top-to-bottom (split-down).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDir {
    Row,
    Column,
}

/// Pane layout tree inside one tab. Leaves hold session ids.
#[derive(Clone)]
pub enum SplitNode {
    Leaf(u64),
    Split {
        dir: SplitDir,
        children: Vec<SplitNode>,
    },
}

impl SplitNode {
    /// Replace leaf `target` with a `dir` split containing
    /// `[target, new_leaf]` — the new pane takes half the target's slot,
    /// like tmux/iTerm pane splits.
    fn split(&mut self, dir: SplitDir, target: u64, new_id: u64) -> bool {
        match self {
            Self::Leaf(id) if *id == target => {
                *self = Self::Split {
                    dir,
                    children: vec![Self::Leaf(target), Self::Leaf(new_id)],
                };
                true
            }
            Self::Leaf(_) => false,
            Self::Split { children, .. } => {
                children.iter_mut().any(|c| c.split(dir, target, new_id))
            }
        }
    }

    /// Remove leaf `sid`; collapse single-child splits. Returns the
    /// surviving tree, or `None` when `sid` was the only leaf.
    fn remove(&self, sid: u64) -> Option<Self> {
        match self {
            Self::Leaf(id) if *id == sid => None,
            Self::Leaf(id) => Some(Self::Leaf(*id)),
            Self::Split { dir, children } => {
                let kept: Vec<SplitNode> = children
                    .iter()
                    .filter_map(|c| c.remove(sid))
                    .collect();
                match kept.as_slice() {
                    [] => None,
                    [only] => Some(only.clone()),
                    _ => Some(Self::Split {
                        dir: *dir,
                        children: kept,
                    }),
                }
            }
        }
    }

    /// Leaf session ids in document order.
    fn leaves(&self) -> Vec<u64> {
        match self {
            Self::Leaf(id) => vec![*id],
            Self::Split { children, .. } => {
                children.iter().flat_map(Self::leaves).collect()
            }
        }
    }
}

/// A tab: one layout tree of panes plus a focused pane.
/// `Clone` shares the bindings (Rc-backed state), so a cloned item from
/// `nami::collection::List` reads and writes the same tab state.
#[derive(Clone, Identifiable)]
pub struct PaneTab {
    #[id]
    pub id: u64,
    pub title: Binding<Str>,
    pub tree: Binding<SplitNode>,
    /// Focused session id inside `tree`.
    pub focused: Binding<u64>,
    /// Zoomed pane: `Some(id)` renders only that leaf (it fills the tab);
    /// `None` = normal split layout. tmux zoom / kitty overlay semantics.
    pub zoomed: Binding<Option<u64>>,
}

/// Everything tabs and surfaces share.
#[derive(Clone)]
pub struct AppState {
    /// Session list — panes index into it.
    sessions: Rc<RefCell<Vec<Rc<Session>>>>,
    /// Tabs in display order; `selected` holds the active tab's id.
    /// A reactive collection so `ForEach` keeps each tab's view alive
    /// across membership changes — no subtree rebuild, no focus loss.
    tabs: NamiList<PaneTab>,
    /// session id → owning tab id.
    session_tab: Arc<Mutex<HashMap<u64, u64>>>,
    /// Selected tab id.
    pub selected: Binding<u64>,
    /// Window title binding.
    pub window_title: Binding<Str>,
    /// Window state binding — normal/minimized/fullscreen/closed.
    /// Owned by us so keybinds can toggle fullscreen.
    pub window_state: Binding<WindowState>,
    /// Config file state (parsed values + mtime watch).
    pub cfg: Rc<RefCell<ConfigWatcher>>,
    /// The active palette — swapped wholesale on theme reload.
    pub palette: Rc<RefCell<Palette>>,
    /// The backend environment, captured by `AppRoot::body` — needed at
    /// runtime to spawn new windows via `Window::show(env)`.
    env: Rc<std::cell::OnceCell<Environment>>,
    /// Command palette open (Ctrl+Shift+P).
    pub palette_open: Binding<bool>,
    /// Live palette query — bound to the WaterUI `TextField`.
    pub palette_query: Binding<Str>,
    /// Index of the highlighted palette row (Up/Down navigation).
    pub palette_sel: Binding<usize>,
    /// List scroll controller — `scroll_to(sel)` keeps the highlighted
    /// row visible while navigating.
    pub palette_scroll: ScrollController<usize>,

    /// Settings page open (Ctrl+Shift+,).
    pub settings_open: Binding<bool>,
    /// Settings edits — snapshotted from the config each time the page
    /// opens; `Apply` writes them back to the file (hot reload applies).
    pub set_font: Binding<i32>,
    pub set_theme: Binding<usize>,
    pub set_blink: Binding<bool>,
    /// The desktop color-scheme may have changed (gsettings monitor).
    pub theme_dirty: Arc<AtomicBool>,
    /// Quick-terminal window state — the X11 global hotkey flips it
    /// Closed ↔ Normal; `conditional_window` mounts/unmounts the window.
    pub quick_state: Binding<WindowState>,
    /// Presentation helper for the quick window (retained `presented` flag).
    quick_presentation: WindowPresentation,
    /// Lazily-created session set for the quick window — kept alive across
    /// show/hide cycles so the drop-down keeps its shell + scrollback.
    quick_app: RefCell<Option<Rc<AppState>>>,
    /// The X11 grab listener spawn only happens once per process.
    quick_listener_started: Rc<AtomicBool>,
    /// Retained drain-future handle — dropping a spawned task cancels it.
    quick_task: Rc<RefCell<Option<Box<dyn std::any::Any>>>>,
    /// Frame binding of the live quick window (kept to re-dock after mount).
    quick_frame: Rc<RefCell<Option<Binding<Rect>>>>,
    /// Hotkey fires but grab failed (no X11) — surface it once.
    pub quick_unavailable: RefCell<bool>,
    /// True for the drop-down's own AppState: it neither hosts a quick
    /// window itself nor spawns a second key grab.
    is_quick: bool,
    /// Weak handles to live terminals so the theme monitor thread can
    /// request frames (dirty is only read inside `poll_config`).
    theme_wakes: Arc<Mutex<Vec<std::sync::Weak<Terminal>>>>,
    next_id: Arc<AtomicU64>,
}

// `.state(&app)` injection rows read the state back through a plain
// `AppState` extractor parameter.
impl_extractor!(AppState);

impl AppState {
    /// Create with one running session.
    // Sessions never leave the UI thread (PTY events arrive through a channel
    // and are consumed in `render`), so the `Arc`s only need UI confinement,
    // not Send+Sync — `Binding` is not Send+Sync by design.
    #[allow(clippy::arc_with_non_send_sync)]
    pub fn new(config_path: Option<std::path::PathBuf>, command: Option<Vec<String>>) -> Self {
        let mut watcher = ConfigWatcher::new(config_path);
        watcher.config.command = command;
        for e in &watcher.errors {
            eprintln!("hydroterm config: {e}");
        }
        let palette = Palette::from_theme(&watcher.config.resolve_theme());
        #[cfg(target_os = "linux")]
        let theme_is_auto = matches!(watcher.config.theme, crate::config::ThemeRef::Auto);
        let quick_binding = Binding::container(WindowState::Closed);
        let state = Self {
            sessions: Rc::new(RefCell::new(Vec::new())),
            tabs: NamiList::new(),
            session_tab: Arc::new(Mutex::new(HashMap::new())),
            selected: Binding::u64(0),
            window_title: binding(Str::from("hydroterm")),
            window_state: binding(WindowState::Normal),
            cfg: Rc::new(RefCell::new(watcher)),
            palette: Rc::new(RefCell::new(palette)),
            env: Rc::new(std::cell::OnceCell::new()),
            palette_open: Binding::bool(false),
            palette_query: binding(Str::from("")),
            palette_sel: Binding::usize(0),
            palette_scroll: ScrollController::new(0),
            settings_open: Binding::bool(false),
            set_font: Binding::i32(13),
            set_theme: Binding::usize(0),
            set_blink: Binding::bool(true),
            theme_dirty: Arc::new(AtomicBool::new(false)),
            quick_state: quick_binding.clone(),
            quick_presentation: WindowPresentation::new(&quick_binding),
            quick_app: RefCell::new(None),
            quick_listener_started: Rc::new(AtomicBool::new(false)),
            quick_task: Rc::new(RefCell::new(None)),
            quick_frame: Rc::new(RefCell::new(None)),
            quick_unavailable: RefCell::new(false),
            is_quick: false,
            theme_wakes: Arc::new(Mutex::new(Vec::new())),
            next_id: Arc::new(AtomicU64::new(0)),
        };
        state.new_tab();
        // `-e` applies to the first session only (like xterm/kitty).
        state.cfg.borrow_mut().config.command = None;
        // `theme = auto`: watch the desktop color-scheme. gsettings
        // `monitor` prints a line per change; flag dirty + poke every
        // live surface so `poll_config` re-resolves on its next frame.
        #[cfg(target_os = "linux")]
        if theme_is_auto {
            let dirty = state.theme_dirty.clone();
            let wakes = state.theme_wakes.clone();
            std::thread::spawn(move || {
                use std::io::BufRead;
                let Ok(mut child) = std::process::Command::new("gsettings")
                    .args(["monitor", "org.gnome.desktop.interface", "color-scheme"])
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                else {
                    return;
                };
                let Some(out) = child.stdout.take() else { return };
                for _ in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                    dirty.store(true, Ordering::Relaxed);
                    wakes.lock().unwrap().retain(|w| match w.upgrade() {
                        Some(t) => {
                            t.proxy.request_frame();
                            true
                        }
                        None => false,
                    });
                }
            });
        }
        state
    }

    /// Register a live surface's frame-wake for the theme monitor.
    pub fn register_theme_wake(&self, terminal: &Arc<Terminal>) {
        self.theme_wakes
            .lock()
            .unwrap()
            .push(Arc::downgrade(terminal));
    }

    /// Re-read the config file when it changed; live-applies font size,
    /// theme and keybinds. Called from each surface's render loop.
    pub fn poll_config(&self) {
        // Desktop color-scheme flip under `theme = auto`.
        if self.theme_dirty.swap(false, Ordering::Relaxed) {
            *self.palette.borrow_mut() =
                Palette::from_theme(&self.cfg.borrow().config.resolve_theme());
        }
        let (config, errors) = {
            let mut w = self.cfg.borrow_mut();
            if !w.poll() {
                return;
            }
            (w.config.clone(), w.errors.clone())
        };
        for e in &errors {
            eprintln!("hydroterm config: {e}");
        }
        *self.palette.borrow_mut() = Palette::from_theme(&config.resolve_theme());
        for s in self.sessions.borrow().iter() {
            s.font_size.set(config.font_size);
        }
    }

    /// Read-only access to the current config.
    pub fn config<R>(&self, f: impl FnOnce(&AppConfig) -> R) -> R {
        f(&self.cfg.borrow().config)
    }

    /// Toggle borderless fullscreen on the main window.
    pub fn toggle_fullscreen(&self) {
        let next = match self.window_state.get() {
            WindowState::Fullscreen => WindowState::Normal,
            _ => WindowState::Fullscreen,
        };
        self.window_state.set(next);
    }

    /// The drop-down's own session set — created once, kept across
    /// show/hide cycles so the shell + scrollback persist.
    fn quick_app(&self) -> Rc<AppState> {
        if let Some(app) = self.quick_app.borrow().as_ref() {
            return app.clone();
        }
        let mut app = AppState::new(Some(self.cfg.borrow().path.clone()), None);
        app.is_quick = true;
        let app = Rc::new(app);
        *self.quick_app.borrow_mut() = Some(app.clone());
        app
    }

    /// Flip the drop-down window open/closed (the X11 hotkey calls this).
    pub fn toggle_quick(&self) {
        if self.is_quick {
            // The drop-down's own view should not host another quick window.
            return;
        }
        let next = match self.quick_state.get() {
            WindowState::Closed => WindowState::Normal,
            _ => WindowState::Closed,
        };
        self.quick_state.set(next);
    }

    /// Start the X11 global-hotkey listener (idempotent, main window only).
    /// The grab thread forwards F12 presses over a channel; this drains it
    /// via `spawn_local` so the `WindowState` flip happens on the UI thread.
    pub fn start_quick_listener(&self) {
        if self.is_quick || self.quick_listener_started.swap(true, Ordering::SeqCst) {
            return;
        }
        // One channel carries both the X11 hotkey press and the delayed
        // frame re-dock (the position apply on a just-mapped X11 window can
        // be lost; re-emitting the frame binding wakes the loop and re-runs
        // `apply_properties` on the now-visible window).
        #[derive(Clone)]
        enum QuickEvent {
            Hotkey,
            Dock,
        }
        let (tx, rx) = async_channel::unbounded::<QuickEvent>();
        match crate::quickterm::spawn_hotkey(tx.clone(), crate::quickterm::XK_F12, QuickEvent::Hotkey) {
            Some(_) => {
                let app = self.clone();
                let task = spawn_local(async move {
                    while let Ok(event) = rx.recv().await {
                        while rx.try_recv().is_ok() {}
                        match event {
                            QuickEvent::Hotkey => {
                                app.toggle_quick();
                                if app.quick_state.get() != WindowState::Closed {
                                    let dock_tx = tx.clone();
                                    std::thread::spawn(move || {
                                        // The first mount is slow (renderer
                                        // init + PTY spawn): poke at a few
                                        // delays so one lands post-map.
                                        for delay in [200u64, 700, 1500] {
                                            std::thread::sleep(std::time::Duration::from_millis(delay));
                                            let _ = dock_tx.try_send(QuickEvent::Dock);
                                        }
                                    });
                                }
                            }
                            QuickEvent::Dock => app.dock_quick_frame(),
                        }
                    }
                });
                *self.quick_task.borrow_mut() = Some(Box::new(task));
            }
            None => {
                *self.quick_unavailable.borrow_mut() = true;
            }
        }
    }

    /// Re-apply the dock frame after the quick window has mapped — the
    /// position applied while the X11 window was still invisible is lost
    /// (the window manager picks its own placement), so poke the binding
    /// once it exists on screen.
    fn dock_quick_frame(&self) {
        let Some(frame) = self.quick_frame.borrow().clone() else { return };
        let Some((sw, sh)) = crate::quickterm::screen_size() else { return };
        frame.set(Rect::new(
            Point::new(0.0, 0.0),
            Size::new(sw as f32, (sh * 0.45) as f32),
        ));
    }

    /// Build the quick terminal's borderless top-docked window (mounted by
    /// `conditional_window` when `quick_state` leaves `Closed`).
    fn quick_window(&self, state: Binding<WindowState>) -> Window {
        let app = self.quick_app();
        let title = app.window_title.clone();
        let w = Window::new(title, state, move || {
            app_root((*app).clone())
        })
        .style(WindowStyle::Borderless)
        .resizable(false);
        // Dock: top of the primary screen, full width, 45% height.
        if let Some((sw, sh)) = crate::quickterm::screen_size() {
            w.frame.set(Rect::new(
                Point::new(0.0, 0.0),
                Size::new(sw as f32, (sh * 0.45) as f32),
            ));
        }
        *self.quick_frame.borrow_mut() = Some(w.frame.clone());
        w
    }

    /// Spawn a whole new OS window with a fresh session set (same config
    /// file, independent tabs and sessions). Uses the runner's
    /// `WindowManager` — `Window::show` mounts a real winit window.
    pub fn new_window(&self) {
        let Some(env) = self.env.get() else { return };
        let state = AppState::new(Some(self.cfg.borrow().path.clone()), None);
        // Same launch-time transparency as the main window.
        let opacity = state.config(|c| c.background_opacity);
        let bg = state.config(|c| c.resolve_theme().background);
        let window = Window::new(
            state.window_title.clone(),
            state.window_state.clone(),
            {
                let state = state.clone();
                move || app_root(state.clone())
            },
        )
        .background(Color::srgb(bg.r, bg.g, bg.b).with_opacity(opacity));
        window.show(env);
    }

    /// Shut every session down and exit the process.
    pub fn quit(&self) {
        for s in self.sessions.borrow().iter() {
            s.terminal.shutdown();
        }
        std::process::exit(0);
    }

    /// Look up one session.
    pub fn session(&self, id: u64) -> Option<Rc<Session>> {
        self.sessions
            .borrow()
            .iter()
            .find(|s| s.id == id)
            .cloned()
    }

    fn alloc_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// The focused session of the selected tab, if any.
    pub fn focused_session(&self) -> Option<Rc<Session>> {
        let tab_id = self.selected.get();
        let focused = self
            .tabs.iter().find(|t| t.id == tab_id)
            .map(|t| t.focused.get())?;
        self.session(focused)
    }

    fn spawn_session(&self, cwd: Option<std::path::PathBuf>) -> Rc<Session> {
        let id = self.alloc_id();
        let cfg = self.cfg.borrow().config.clone();
        let session = Rc::new(Session::spawn(id, cwd, &cfg));
        self.sessions.borrow_mut().push(session.clone());
        session
    }

    /// Spawn a session, wrap it in a new tab, select it. Inherits the OSC 7
    /// cwd of the currently focused session when the shell reported one.
    pub fn new_tab(&self) -> u64 {
        let cwd = self
            .focused_session()
            .and_then(|s| s.cwd.lock().unwrap().clone());
        let session = self.spawn_session(cwd);
        let tab = PaneTab {
            id: self.alloc_id(),
            title: session.title.clone(),
            tree: binding(SplitNode::Leaf(session.id)),
            focused: Binding::u64(session.id),
            zoomed: Binding::default(),
        };
        self.session_tab
            .lock()
            .unwrap()
            .insert(session.id, tab.id);
        let tab_id = tab.id;
        self.tabs.push(tab);
        self.selected.set(tab_id);
        tab_id
    }

    /// Split the pane `target` of the selected tab in `dir`; the new pane
    /// inherits the target's cwd.
    pub fn split_pane(&self, dir: SplitDir, target: u64) -> Option<u64> {
        let tab_id = self.selected.get();
        let tab = self
            .tabs.iter().find(|t| t.id == tab_id)?;
        let cwd = self
            .session(target)
            .and_then(|s| s.cwd.lock().unwrap().clone());
        let session = self.spawn_session(cwd);
        let ok = tab
            .tree
            .with_mut(|tree| tree.split(dir, target, session.id));
        if !ok {
            session.terminal.shutdown();
            self.sessions
                .borrow_mut()
                .retain(|s| s.id != session.id);
            return None;
        }
        self.session_tab
            .lock()
            .unwrap()
            .insert(session.id, tab_id);
        tab.focused.set(session.id);
        Some(session.id)
    }

    /// A pane took focus — record it and sync the tab title.
    pub fn focus_pane(&self, session_id: u64) {
        let Some(tab_id) = self.session_tab.lock().unwrap().get(&session_id).copied()
        else {
            return;
        };
        let Some(tab) = self
            .tabs.iter().find(|t| t.id == tab_id)
        else {
            return;
        };
        tab.focused.set(session_id);
        if let Some(s) = self.session(session_id) {
            tab.title.set(s.title.get());
        }
    }

    /// Update a session's title; mirrors onto its tab and the window
    /// title when the session is focused in the selected tab.
    pub fn set_session_title(&self, session_id: u64, title: Str) {
        if let Some(s) = self.session(session_id) {
            s.title.set(title.clone());
        }
        if let Some(tab_id) = self.session_tab.lock().unwrap().get(&session_id).copied()
            && let Some(tab) = self
                .tabs.iter().find(|t| t.id == tab_id)
                && tab.focused.get() == session_id
            {
                tab.title.set(title.clone());
                if self.selected.get() == tab_id {
                    self.window_title.set(title);
                }
            }
    }

    /// Cycle pane focus within the selected tab.
    pub fn cycle_pane(&self, dir: isize) {
        let tab_id = self.selected.get();
        let Some(tab) = self
            .tabs.iter().find(|t| t.id == tab_id)
        else {
            return;
        };
        let leaves = tab.tree.get().leaves();
        if leaves.len() < 2 {
            return;
        }
        let pos = leaves
            .iter()
            .position(|&s| s == tab.focused.get())
            .unwrap_or(0) as isize;
        let next = (pos + dir).rem_euclid(leaves.len() as isize) as usize;
        self.focus_pane(leaves[next]);
    }

    /// Toggle pane zoom on the selected tab: the focused pane fills the
    /// whole tab; toggling again (or re-focusing then toggling) restores
    /// the split layout.
    pub fn toggle_pane_zoom(&self) {
        let tab_id = self.selected.get();
        let Some(tab) = self
            .tabs.iter().find(|t| t.id == tab_id)
        else {
            return;
        };
        let focused = tab.focused.get();
        tab.zoomed.with_mut(|z| *z = z.take().is_none().then_some(focused));
    }

    /// Close a pane; when it's the tab's last pane, close the tab.
    pub fn close_pane(&self, session_id: u64) {
        let Some(tab_id) = self.session_tab.lock().unwrap().get(&session_id).copied()
        else {
            return;
        };
        let Some(tab) = self
            .tabs.iter().find(|t| t.id == tab_id)
        else {
            return;
        };
        match tab.tree.get().remove(session_id) {
            Some(new_tree) => {
                // Focus a remaining leaf when the closed pane had focus.
                if tab.focused.get() == session_id
                    && let Some(next) = new_tree.leaves().first()
                {
                    tab.focused.set(*next);
                }
                tab.tree.set(new_tree);
            }
            None => return self.close_tab(tab_id),
        }
        self.kill_session(session_id);
        self.session_tab.lock().unwrap().remove(&session_id);
    }

    /// Kill a session's PTY and drop it from the registry.
    fn kill_session(&self, session_id: u64) {
        let mut sessions = self.sessions.borrow_mut();
        if let Some(pos) = sessions.iter().position(|s| s.id == session_id) {
            sessions[pos].terminal.shutdown();
            sessions.remove(pos);
        }
    }

    /// Kill every session in a tab and drop the tab; selects a neighbor.
    pub fn close_tab(&self, tab_id: u64) {
        let Some(tab) = self
            .tabs.iter().find(|t| t.id == tab_id)
        else {
            return;
        };
        let leaves = tab.tree.get().leaves();
        for sid in &leaves {
            self.kill_session(*sid);
            self.session_tab.lock().unwrap().remove(sid);
        }
        let tabs = self.tabs.snapshot();
        if let Some(pos) = tabs.iter().position(|t| t.id == tab_id) {
            let _ = self.tabs.remove(pos);
            if self.selected.get() == tab_id {
                let remaining = self.tabs.snapshot();
                let idx = pos.min(remaining.len().saturating_sub(1));
                if let Some(next) = remaining.as_slice().get(idx) {
                    self.selected.set(next.id);
                }
            }
        }
    }

    /// Select the tab at 1-based index `n`.
    pub fn select_tab(&self, n: usize) {
        let id = self
            .tabs
            .iter()
            .nth(n.saturating_sub(1))
            .map(|t| t.id);
        if let Some(id) = id {
            self.selected.set(id);
        }
    }

    /// Cycle tabs by `dir` (+1/-1).
    pub fn cycle_tab(&self, dir: isize) {
        let tabs = self.tabs.snapshot();
        if tabs.is_empty() {
            return;
        }
        let cur = self.selected.get();
        let pos = tabs.iter().position(|t| t.id == cur).unwrap_or(0) as isize;
        let next = (pos + dir).rem_euclid(tabs.len() as isize) as usize;
        self.selected.set(tabs[next].id);
    }
}

/// One terminal pane: the `SceneView` running `TermSurface` plus the
/// search bar sibling above it. A `View` impl (not free-standing view code)
/// because the shared `FontCollection` lives in the environment — grabbing it
/// in `body` is what math/chart do, and it lets a pane that outlives a
/// subtree rebuild keep its single font context.
struct PaneLeaf {
    session: Rc<Session>,
    state: AppState,
}

impl View for PaneLeaf {
    fn body(self, env: &Environment) -> impl View {
        // Search bar: a real WaterUI row that appears above the surface —
        // the field is a sibling, so toggling it never remounts the
        // SceneView or drops its keyboard focus.
        let query = self.session.search_query.clone();
        let status = self.session.search_status.clone();
        let open = self.session.search_open.clone();
        let surface = Frame::new(SceneView::new(TermSurface::new(
            self.session,
            self.state.clone(),
            self.state.palette,
            FontCollection::from_env(env),
        )));
        let bar = when(open, move || {
            hstack((
                text("/ "),
                field("find in buffer", &query),
                text(status.clone()),
            ))
        })
        .anyview();
        vstack((bar, surface)).spacing(0.0)
    }
}

/// Render one pane node as WaterUI views.
fn pane_view(node: &SplitNode, state: &AppState) -> AnyView {
    match node {
        SplitNode::Leaf(sid) => {
            let session = state.session(*sid);
            match session {
                Some(session) => PaneLeaf {
                    session,
                    state: state.clone(),
                }
                .anyview(),
                None => text("pane closed").anyview(),
            }
        }
        SplitNode::Split { dir, children } => {
            let views: Vec<AnyView> = children
                .iter()
                .map(|child| pane_view(child, state))
                .collect();
            // Vec<AnyView> collects straight into a stack — no ForEach ids.
            match dir {
                SplitDir::Row => views.into_iter().collect::<HStack<_>>().spacing(0.0).anyview(),
                SplitDir::Column => views.into_iter().collect::<VStack<_>>().spacing(0.0).anyview(),
            }
        }
    }
}

/// Root of every hydroterm window: `body` runs inside the environment, so
/// it captures `env` for `AppState::new_window` before rendering the tabs.
struct AppRoot {
    state: AppState,
}

impl View for AppRoot {
    fn body(self, env: &Environment) -> impl View {
        let _ = self.state.env.set(env.clone());
        tabs_view(self.state)
    }
}

/// Window content — used for both the main window and spawned ones.
pub fn app_root(state: AppState) -> impl View {
    AppRoot { state }
}

/// One tab's pane tree — zoomed single leaf or the full split layout.
fn tab_content(tab: PaneTab, app: AppState) -> impl View {
    watch(tab.zoomed.clone(), {
        let app = app.clone();
        let tree = tab.tree.clone();
        move |z: Option<u64>| {
            if let Some(z) = z.filter(|z| app.session(*z).is_some()) {
                pane_view(&SplitNode::Leaf(z), &app)
            } else {
                watch(tree.clone(), {
                    let app = app.clone();
                    move |node: SplitNode| pane_view(&node, &app)
                })
                .anyview()
            }
        }
    })
}

/// Build the window content: a tab strip over a `ZStack` holding every
/// tab's pane tree. Both are `ForEach` collections over the reactive tab
/// `List`, keyed by stable tab id — hydrolysis retains each item's
/// subtree, so adding or switching tabs never rebuilds a sibling's
/// `SceneView` or drops its keyboard focus (Principle 8: precise
/// signals over `watch`). The active tab shows via `.visible`, which
/// keeps the view mounted but undrawn and non-hittable.
// `handler_captures_binding` would push the tab-tap binding through a
// `State<Binding<u64>>` extractor — but extractor parameters in `.on_tap`
// panic at click time (WATERUI_FEEDBACK #20), so the closure captures it.
#[allow(handler_captures_binding)]
pub fn tabs_view(state: AppState) -> impl View {
    let palette_overlay = when(state.palette_open.clone(), {
        let state = state.clone();
        move || palette_view(state.clone())
    })
    .anyview();
    let settings_overlay = when(state.settings_open.clone(), {
        let state = state.clone();
        move || settings_view(state.clone())
    })
    .anyview();

    let strip = {
        let app = state.clone();
        HStack::for_each(state.tabs.clone(), move |tab: PaneTab| {
            let app = app.clone();
            let tab_id = tab.id;
            let active = app.selected.equal_to(tab_id);
            // M3 primary-tab look: accent label + indicator bar when active.
            let label_color = signal_color(
                active.select(Color::new(Accent), Color::new(MutedForeground)),
            );
            let indicator_color = signal_color(
                active.select(Color::new(Accent), Color::new(Background)),
            );
            let sel = app.selected.clone();
            vstack((
                hstack((
                    text(tab.title.clone()).foreground(label_color),
                    text("×")
                        .muted()
                        .padding_with([3.0, 0.0, 4.0, 4.0])
                        .on_tap({
                            let app = app.clone();
                            move || app.close_tab(tab_id)
                        }),
                ))
                .padding_with([4.0, 0.0, 8.0, 4.0]),
                Frame::new(indicator_color).height(3.0),
            ))
            .spacing(0.0)
            .height(TAB_STRIP_HEIGHT)
            // Zero-arg closure — `.on_tap` with an extractor parameter
            // panics on click (feedback #20).
            .on_tap(move || sel.set(tab_id))
        })
    };
    let strip_bar = hstack((
        strip,
        text("+")
            .muted()
            .padding()
            .on_tap({
                let state = state.clone();
                move || _ = state.new_tab()
            }),
    ))
    .spacing(4.0)
    .padding();

    let content = {
        let app = state.clone();
        Frame::new(ZStack::for_each(state.tabs.clone(), move |tab: PaneTab| {
            let app = app.clone();
            tab_content(tab.clone(), app.clone()).visible(app.selected.equal_to(tab.id))
        }))
        .max_width(f32::INFINITY)
        .max_height(f32::INFINITY)
    };

    // X11 global hotkey → quick terminal (F12). The listener spawns once
    // per process; `conditional_window` mounts the drop-down whenever
    // `quick_state` leaves Closed.
    state.start_quick_listener();
    let quick = {
        let app = state.clone();
        conditional_window(&state.quick_presentation, move |win_state| {
            app.quick_window(win_state)
        })
        .anyview()
    };

    zstack((
        vstack((strip_bar, content)).spacing(0.0),
        palette_overlay,
        settings_overlay,
        quick,
    ))
}

/// A command-palette row: display name, chord hint, action.
pub struct PaletteItem {
    pub name: &'static str,
    pub chord: &'static str,
    pub action: TermAction,
}

/// Everything reachable from the palette — same actions as keybinds.
pub const PALETTE_ITEMS: &[PaletteItem] = &[
    PaletteItem { name: "New Tab", chord: "ctrl+shift+t", action: TermAction::NewTab },
    PaletteItem { name: "New Window", chord: "ctrl+shift+n", action: TermAction::NewWindow },
    PaletteItem { name: "Close Pane / Tab", chord: "ctrl+shift+w", action: TermAction::CloseTab },
    PaletteItem { name: "Split Right", chord: "ctrl+shift+e", action: TermAction::SplitRight },
    PaletteItem { name: "Split Down", chord: "ctrl+shift+d", action: TermAction::SplitDown },
    PaletteItem { name: "Toggle Pane Zoom", chord: "ctrl+shift+z", action: TermAction::PaneZoom },
    PaletteItem { name: "Focus Next Pane", chord: "ctrl+shift+]", action: TermAction::FocusNextPane },
    PaletteItem { name: "Focus Previous Pane", chord: "ctrl+shift+[", action: TermAction::FocusPrevPane },
    PaletteItem { name: "Copy", chord: "ctrl+shift+c", action: TermAction::Copy },
    PaletteItem { name: "Paste", chord: "ctrl+shift+v", action: TermAction::Paste },
    PaletteItem { name: "Select All", chord: "ctrl+shift+a", action: TermAction::SelectAll },
    PaletteItem { name: "Find in Buffer", chord: "ctrl+shift+f", action: TermAction::Search },
    PaletteItem { name: "Settings", chord: "ctrl+shift+,", action: TermAction::Settings },
    PaletteItem { name: "Clear Scrollback", chord: "ctrl+shift+k", action: TermAction::ClearScrollback },
    PaletteItem { name: "Increase Font Size", chord: "ctrl+shift+=", action: TermAction::FontBigger },
    PaletteItem { name: "Decrease Font Size", chord: "ctrl+shift+-", action: TermAction::FontSmaller },
    PaletteItem { name: "Reset Font Size", chord: "ctrl+shift+0", action: TermAction::FontReset },
    PaletteItem { name: "Jump to Previous Prompt", chord: "ctrl+shift+up", action: TermAction::PromptPrev },
    PaletteItem { name: "Jump to Next Prompt", chord: "ctrl+shift+down", action: TermAction::PromptNext },
    PaletteItem { name: "Scroll to Top", chord: "", action: TermAction::ScrollToTop },
    PaletteItem { name: "Scroll to Bottom", chord: "", action: TermAction::ScrollToBottom },
    PaletteItem { name: "Next Tab", chord: "ctrl+tab", action: TermAction::NextTab },
    PaletteItem { name: "Previous Tab", chord: "ctrl+shift+tab", action: TermAction::PrevTab },
    PaletteItem { name: "Toggle Fullscreen", chord: "ctrl+shift+f11", action: TermAction::Fullscreen },
    PaletteItem { name: "Quit", chord: "", action: TermAction::Quit },
];

/// Theme names offered by the settings page — index order is the
/// picker's selection value.
pub const THEME_CHOICES: &[&str] = &[
    "auto",
    "hydroterm-dark",
    "hydroterm-light",
    "solarized-dark",
    "solarized-light",
];

/// `ThemeRef` → settings picker index.
pub fn theme_index(theme: &crate::config::ThemeRef) -> usize {
    match theme {
        crate::config::ThemeRef::Auto => 0,
        crate::config::ThemeRef::Named(name) => THEME_CHOICES
            .iter()
            .position(|t| t == name)
            .unwrap_or(0),
    }
}

/// Substring-filter the palette items (empty query → all).
pub fn palette_matches(query: &str) -> Vec<&'static PaletteItem> {
    let q = query.trim().to_lowercase();
    PALETTE_ITEMS
        .iter()
        .filter(|item| q.is_empty() || item.name.to_lowercase().contains(&q))
        .collect()
}

impl AppState {
    /// Open/close the palette (Ctrl+Shift+P). Opening clears the query
    /// and resets row selection to the first match.
    pub fn toggle_palette(&self) {
        let next = !self.palette_open.get();
        if next {
            self.palette_query.set_from("");
            self.palette_sel.set(0);
            self.palette_scroll.scroll_to(0);
        }
        self.palette_open.set(next);
    }

    /// Run the `i`-th match of the current query (Up/Down selection or
    /// a row tap).
    pub fn run_palette_at(&self, i: usize) {
        let q = self.palette_query.get().to_string();
        let matches = palette_matches(&q);
        let Some(&item) = matches.as_slice().get(i) else {
            self.palette_open.set(false);
            return;
        };
        self.run_palette_action(item.action);
    }

    /// Open/close the settings page (Ctrl+Shift+,). Opening snapshots
    /// the live config into the edit bindings.
    pub fn toggle_settings(&self) {
        let next = !self.settings_open.get();
        if next {
            let (font, theme, blink) =
                self.config(|c| (c.font_size as i32, theme_index(&c.theme), c.cursor_blink));
            self.set_font.set(font);
            self.set_theme.set(theme);
            self.set_blink.set(blink);
        }
        self.settings_open.set(next);
    }

    /// Persist the settings edits back into the config file — the
    /// hot-reload watcher applies them on the next poll, same as a
    /// manual edit.
    pub fn apply_settings(&self) {
        self.settings_open.set(false);
        let path = self.cfg.borrow().path.clone();
        let theme = THEME_CHOICES[self.set_theme.get().min(THEME_CHOICES.len() - 1)];
        let blink = self.set_blink.get();
        crate::config::upsert_config_key(&path, "font-size", &self.set_font.get().to_string());
        crate::config::upsert_config_key(&path, "theme", theme);
        crate::config::upsert_config_key(
            &path,
            "cursor-blink",
            if blink { "true" } else { "false" },
        );
    }

    /// Run a palette action: close the overlay, then queue it on the
    /// focused session's surface so every action shares the key-chord
    /// dispatch path. Falls back to the app-level subset when no session
    /// is focused (e.g. the last one exited).
    pub fn run_palette_action(&self, action: TermAction) {
        self.palette_open.set(false);
        if let Some(session) = self.focused_session() {
            session.pending_actions.borrow_mut().push(action);
            session.terminal.proxy.request_frame();
            return;
        }
        match action {
            TermAction::NewTab => {
                self.new_tab();
            }
            TermAction::NewWindow => self.new_window(),
            TermAction::NextTab => self.cycle_tab(1),
            TermAction::PrevTab => self.cycle_tab(-1),
            TermAction::Fullscreen => self.toggle_fullscreen(),
            TermAction::Quit => self.quit(),
            _ => {}
        }
    }
}

/// The palette overlay: a field + filtered action list, stacked over the
/// tabs and a dimming mask. Up/Down moves the selection (the surface's
/// key path), Enter runs the selected match; a row tap runs it directly.
fn palette_view(state: AppState) -> impl View {
    let query = state.palette_query.clone();
    let list = watch(query, {
        let state = state.clone();
        move |q: Str| {
            let items: Vec<&'static PaletteItem> = palette_matches(q.as_str());
            let indices: Vec<SelfId<usize>> = (0..items.len()).map(SelfId::new).collect();
            List::for_each(indices, {
                let state = state.clone();
                let items = items.clone();
                move |i: SelfId<usize>| {
                    let i = *i;
                    let item = items[i];
                    let row = hstack((
                        text(item.name).foreground(Foreground),
                        Spacer::flexible(),
                        text(item.chord).muted(),
                    ))
                    .padding()
                    .on_tap(move |app: AppState| app.run_palette_at(i))
                    .state(&state);
                    ListItem::new(row).selected(state.palette_sel.equal_to(i))
                }
            })
            .scroll_controller(&state.palette_scroll)
            .anyview()
        }
    });
    let panel = vstack((field("type a command", &state.palette_query), list))
        .spacing(4.0)
        .padding()
        .max_height(430.0)
        .background(Surface);
    vstack((panel, Spacer::flexible())).background(Srgb::BLACK.with_opacity(0.45))
}

/// The settings page: font size stepper, theme picker, cursor-blink
/// toggle, Apply writes the config file (hot reload picks it up).
/// Escape/Enter close it via the surface's key path.
fn settings_view(state: AppState) -> impl View {
    let theme_items: Vec<PickerItem<usize>> = THEME_CHOICES
        .iter()
        .enumerate()
        .map(|(i, name)| text(*name).tag(i))
        .collect();
    // hydrolysis-m3's stepper draws label and buttons but no value, and
    // its picker draws no label at all — label text + value are composed
    // manually here.
    let panel = vstack((
        text("Settings").foreground(Foreground),
        stepper(text!("Font size  {v}", v = state.set_font), &state.set_font),
        hstack((
            text("Theme").foreground(Foreground),
            Spacer::flexible(),
            picker("Theme", theme_items, &state.set_theme).hide_label(),
        )),
        hstack((
            text("Cursor blink").foreground(Foreground),
            Spacer::flexible(),
            toggle("Cursor blink", &state.set_blink).hide_label(),
        )),
        {
            let app = state.clone();
            button("Apply").action(move || app.apply_settings())
        },
    ))
    .spacing(8.0)
    .padding()
    .background(Surface);
    vstack((panel, Spacer::flexible())).background(Srgb::BLACK.with_opacity(0.45))
}
