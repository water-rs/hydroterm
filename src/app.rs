//! Application state: sessions, tabs, split-pane trees, focus tracking,
//! and the actions surfaces trigger (new/close/cycle, splits, clipboard).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use alacritty_terminal::term::Config;
use alacritty_terminal::tty::Shell;
use alacritty_terminal::vte::ansi::CursorStyle;
use nami::{Binding, binding};
use waterui::prelude::*;
use waterui::widget::condition::when;
use waterui_graphics::GpuSurface;

use crate::config::{AppConfig, ConfigWatcher};
use crate::palette::Palette;
use crate::surface::TermSurface;
use crate::terminal::Terminal;

/// Default font size in points (the config file may override).
pub const FONT_SIZE: f32 = 13.0;

/// Shared per-session UI state: the bindings a pane surface reads, plus
/// the owning `Terminal` (PTY + grid).
pub struct Session {
    pub id: u64,
    /// The terminal (Term + EventLoop + PTY channel).
    pub terminal: Arc<Terminal>,
    /// OSC-set title (propagates to the tab when the pane is focused).
    pub title: Binding<Str>,
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
            font_size: Binding::f32(cfg.font_size),
            exited: Binding::bool(false),
            cwd: std::sync::Mutex::new(None),
            search_open: Binding::bool(false),
            search_query: binding(Str::from("")),
            search_status: binding(Str::from("")),
            kitty: Rc::new(RefCell::new(crate::kitty::KittyStore::default())),
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
pub struct PaneTab {
    pub id: u64,
    pub title: Binding<Str>,
    pub tree: Binding<SplitNode>,
    /// Focused session id inside `tree`.
    pub focused: Binding<u64>,
}

/// Everything tabs and surfaces share.
#[derive(Clone)]
pub struct AppState {
    /// Session list — panes index into it.
    sessions: Rc<RefCell<Vec<Rc<Session>>>>,
    /// Tabs in display order; `selected` holds the active tab's id.
    tabs: Rc<RefCell<Vec<Rc<PaneTab>>>>,
    /// session id → owning tab id.
    session_tab: Arc<Mutex<HashMap<u64, u64>>>,
    /// Selected tab id.
    pub selected: Binding<u64>,
    /// Tab membership as tab ids — `watch` rebuilds `Tabs` from it,
    /// since hydrolysis takes a static `Vec<Tab>`.
    pub tab_ids: Binding<Vec<u64>>,
    /// Window title binding.
    pub window_title: Binding<Str>,
    /// Config file state (parsed values + mtime watch).
    pub cfg: Rc<RefCell<ConfigWatcher>>,
    /// The active palette — swapped wholesale on theme reload.
    pub palette: Rc<RefCell<Palette>>,
    next_id: Arc<AtomicU64>,
}

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
        let state = Self {
            sessions: Rc::new(RefCell::new(Vec::new())),
            tabs: Rc::new(RefCell::new(Vec::new())),
            session_tab: Arc::new(Mutex::new(HashMap::new())),
            selected: Binding::u64(0),
            tab_ids: Binding::default(),
            window_title: binding(Str::from("hydroterm")),
            cfg: Rc::new(RefCell::new(watcher)),
            palette: Rc::new(RefCell::new(palette)),
            next_id: Arc::new(AtomicU64::new(0)),
        };
        state.new_tab();
        // `-e` applies to the first session only (like xterm/kitty).
        state.cfg.borrow_mut().config.command = None;
        state
    }

    /// Re-read the config file when it changed; live-applies font size,
    /// theme and keybinds. Called from each surface's render loop.
    pub fn poll_config(&self) {
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
            .tabs
            .borrow()
            .iter()
            .find(|t| t.id == tab_id)
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
        let tab = Rc::new(PaneTab {
            id: self.alloc_id(),
            title: session.title.clone(),
            tree: binding(SplitNode::Leaf(session.id)),
            focused: Binding::u64(session.id),
        });
        self.session_tab
            .lock()
            .unwrap()
            .insert(session.id, tab.id);
        self.tabs.borrow_mut().push(tab.clone());
        self.selected.set(tab.id);
        self.tab_ids.append(tab.id);
        tab.id
    }

    /// Split the pane `target` of the selected tab in `dir`; the new pane
    /// inherits the target's cwd.
    pub fn split_pane(&self, dir: SplitDir, target: u64) -> Option<u64> {
        let tab_id = self.selected.get();
        let tab = self
            .tabs
            .borrow()
            .iter()
            .find(|t| t.id == tab_id)
            .cloned()?;
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
            .tabs
            .borrow()
            .iter()
            .find(|t| t.id == tab_id)
            .cloned()
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
                .tabs
                .borrow()
                .iter()
                .find(|t| t.id == tab_id)
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
            .tabs
            .borrow()
            .iter()
            .find(|t| t.id == tab_id)
            .cloned()
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

    /// Close a pane; when it's the tab's last pane, close the tab.
    pub fn close_pane(&self, session_id: u64) {
        let Some(tab_id) = self.session_tab.lock().unwrap().get(&session_id).copied()
        else {
            return;
        };
        let Some(tab) = self
            .tabs
            .borrow()
            .iter()
            .find(|t| t.id == tab_id)
            .cloned()
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
            .tabs
            .borrow()
            .iter()
            .find(|t| t.id == tab_id)
            .cloned()
        else {
            return;
        };
        let leaves = tab.tree.get().leaves();
        for sid in &leaves {
            self.kill_session(*sid);
            self.session_tab.lock().unwrap().remove(sid);
        }
        let mut tabs = self.tabs.borrow_mut();
        if let Some(pos) = tabs.iter().position(|t| t.id == tab_id) {
            tabs.remove(pos);
            if self.selected.get() == tab_id {
                let idx = pos.min(tabs.len().saturating_sub(1));
                if let Some(next) = tabs.as_slice().get(idx) {
                    self.selected.set(next.id);
                }
            }
        }
        drop(tabs);
        self.tab_ids.with_mut(|ids| ids.retain(|&x| x != tab_id));
    }

    /// Select the tab at 1-based index `n`.
    pub fn select_tab(&self, n: usize) {
        let id = self
            .tabs
            .borrow()
            .as_slice()
            .get(n.saturating_sub(1))
            .map(|t| t.id);
        if let Some(id) = id {
            self.selected.set(id);
        }
    }

    /// Cycle tabs by `dir` (+1/-1).
    pub fn cycle_tab(&self, dir: isize) {
        let tabs = self.tabs.borrow();
        if tabs.is_empty() {
            return;
        }
        let cur = self.selected.get();
        let pos = tabs.iter().position(|t| t.id == cur).unwrap_or(0) as isize;
        let next = (pos + dir).rem_euclid(tabs.len() as isize) as usize;
        self.selected.set(tabs[next].id);
    }
}

/// Render one pane node as WaterUI views.
fn pane_view(node: &SplitNode, state: &AppState, palette: &Rc<RefCell<Palette>>) -> AnyView {
    match node {
        SplitNode::Leaf(sid) => {
            let session = state.session(*sid);
            match session {
                Some(session) => {
                    // Search bar: a real WaterUI row that appears above the
                    // surface — the field is a sibling, so toggling it never
                    // remounts the GpuSurface or drops its keyboard focus.
                    let query = session.search_query.clone();
                    let status = session.search_status.clone();
                    let open = session.search_open.clone();
                    let surface = GpuSurface::new(TermSurface::new(
                        session,
                        state.clone(),
                        palette.clone(),
                    ))
                    .anyview();
                    let bar = when(open, move || {
                        hstack((
                            text("/ "),
                            field("find in buffer", &query),
                            text(status.clone()),
                        ))
                    })
                    .anyview();
                    vstack((bar, surface)).spacing(0.0).anyview()
                }
                None => text("pane closed").anyview(),
            }
        }
        SplitNode::Split { dir, children } => {
            let views: Vec<AnyView> = children
                .iter()
                .map(|child| pane_view(child, state, palette))
                .collect();
            // Vec<AnyView> collects straight into a stack — no ForEach ids.
            match dir {
                SplitDir::Row => views.into_iter().collect::<HStack<_>>().spacing(0.0).anyview(),
                SplitDir::Column => views.into_iter().collect::<VStack<_>>().spacing(0.0).anyview(),
            }
        }
    }
}

/// Build the tabs view: one pane-tree per tab.
// `Tabs::new` needs a fully materialized `Vec<Tab>` — hydrolysis has no
// reactive-collection tab API yet, so the whole set is rebuilt on change.
// Each `GpuSurface` keeps its `Arc<Terminal>` alive across rebuilds.
#[allow(watch_over_collection)]
pub fn tabs_view(state: AppState) -> impl View {
    watch(state.tab_ids.clone(), move |ids| {
        let tabs = state.tabs.borrow().clone();
        let tab_views: Vec<Tab<u64>> = ids
            .iter()
            .filter_map(|id| tabs.iter().find(|t| t.id == *id).cloned())
            .map(|tab| {
                let app = state.clone();
                let palette = state.palette.clone();
                let tree = tab.tree.clone();
                Tab::container(tab.id, tab.title.clone(), move || {
                    watch(tree.clone(), {
                        let app = app.clone();
                        let palette = palette.clone();
                        move |node: SplitNode| pane_view(&node, &app, &palette)
                    })
                })
            })
            .collect();
        if tab_views.is_empty() {
            // Hydrolysis requires at least one tab — a lone placeholder while
            // the last session exits.
            let t = Tab::container(0u64, "hydroterm", || text("No sessions"));
            return Tabs::new(&state.selected, vec![t]);
        }
        Tabs::new(&state.selected, tab_views)
    })
}
