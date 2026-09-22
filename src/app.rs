//! Application state: the session list, tab selection, and the actions the
//! surfaces trigger (new/close/cycle tabs, font size, clipboard).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use alacritty_terminal::term::Config;
use nami::{Binding, binding};
use waterui::prelude::*;
use waterui_graphics::GpuSurface;

use crate::surface::TermSurface;
use crate::terminal::Terminal;

/// How many lines of scrollback each session keeps.
pub const HISTORY_LINES: usize = 10_000;
/// Default font size in points.
pub const FONT_SIZE: f32 = 13.0;

/// Shared per-session UI state: the bindings a tab label / window title /
/// surface read, plus the owning `Terminal` (PTY + grid).
pub struct Session {
    pub id: u64,
    /// The terminal (Term + EventLoop + PTY channel).
    pub terminal: Arc<Terminal>,
    /// OSC-set title, shown in the tab and window title.
    pub title: Binding<Str>,
    /// Current font size in points.
    pub font_size: Binding<f32>,
    /// Child process exited.
    pub exited: Binding<bool>,
    /// Absolute grid rows of OSC 133 prompt-start marks (`history + line`).
    pub prompt_marks: std::sync::Mutex<Vec<i64>>,
    /// Latest working directory reported via OSC 7.
    pub cwd: std::sync::Mutex<Option<std::path::PathBuf>>,
}

impl Session {
    fn spawn(id: u64, cwd: Option<std::path::PathBuf>) -> Self {
        let config = Config {
            scrolling_history: HISTORY_LINES,
            kitty_keyboard: true,
            ..Default::default()
        };
        // A reasonable initial grid; the surface resizes on its first frame.
        let terminal = Terminal::spawn(config, 120, 32, (9, 18), cwd)
            .expect("failed to spawn PTY — is a shell available?");
        Self {
            id,
            terminal: Arc::new(terminal),
            title: binding(Str::from("Shell")),
            font_size: Binding::f32(FONT_SIZE),
            exited: Binding::bool(false),
            prompt_marks: std::sync::Mutex::new(Vec::new()),
            cwd: std::sync::Mutex::new(None),
        }
    }
}

/// Everything tabs and surfaces share.
#[derive(Clone)]
pub struct AppState {
    /// Session list — the backing store `tab_ids` indexes into.
    sessions: Arc<Mutex<Vec<Arc<Session>>>>,
    /// Selected session id.
    pub selected: Binding<u64>,
    /// Tab membership as session ids — `watch` rebuilds `Tabs` from it,
    /// since hydrolysis takes a static `Vec<Tab>`.
    pub tab_ids: Binding<Vec<u64>>,
    /// Window title binding.
    pub window_title: Binding<Str>,
    next_id: Arc<AtomicU64>,
}

impl AppState {
    /// Create with one running session.
    // Sessions never leave the UI thread (PTY events arrive through a channel
    // and are consumed in `render`), so the `Arc`s only need UI confinement,
    // not Send+Sync — `Binding` is not Send+Sync by design.
    #[allow(clippy::arc_with_non_send_sync)]
    pub fn new() -> Self {
        let state = Self {
            sessions: Arc::new(Mutex::new(Vec::new())),
            selected: Binding::u64(0),
            tab_ids: Binding::default(),
            window_title: binding(Str::from("hydroterm")),
            next_id: Arc::new(AtomicU64::new(0)),
        };
        state.new_tab();
        state
    }

    /// Snapshot of the session list.
    pub fn sessions(&self) -> Vec<Arc<Session>> {
        self.sessions.lock().unwrap().clone()
    }

    fn alloc_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Spawn a session, append a tab, select it. Inherits the OSC 7 cwd
    /// of the currently selected session when the shell reported one.
    #[allow(clippy::arc_with_non_send_sync)]
    pub fn new_tab(&self) -> u64 {
        let id = self.alloc_id();
        let cwd = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .find(|s| s.id == self.selected.get())
            .and_then(|s| s.cwd.lock().unwrap().clone());
        let session = Arc::new(Session::spawn(id, cwd));
        self.sessions.lock().unwrap().push(session);
        self.selected.set(id);
        self.tab_ids.append(id);
        id
    }

    /// Kill a session and drop its tab; selects a neighbor.
    pub fn close_tab(&self, id: u64) {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(pos) = sessions.iter().position(|s| s.id == id) {
            sessions[pos].terminal.shutdown();
            sessions.remove(pos);
            if self.selected.get() == id {
                let idx = pos.min(sessions.len().saturating_sub(1));
                if let Some(next) = sessions.as_slice().get(idx) {
                    self.selected.set(next.id);
                }
            }
            drop(sessions);
            self.tab_ids.with_mut(|ids| ids.retain(|&x| x != id));
        }
    }

    /// Select the tab at 1-based index `n`.
    pub fn select_tab(&self, n: usize) {
        let id = self
            .sessions
            .lock()
            .unwrap()
            .as_slice()
            .get(n.saturating_sub(1))
            .map(|s| s.id);
        if let Some(id) = id {
            self.selected.set(id);
        }
    }

    /// Cycle tabs by `dir` (+1/-1).
    pub fn cycle_tab(&self, dir: isize) {
        let sessions = self.sessions.lock().unwrap();
        if sessions.is_empty() {
            return;
        }
        let cur = self.selected.get();
        let pos = sessions.iter().position(|s| s.id == cur).unwrap_or(0) as isize;
        let next = (pos + dir).rem_euclid(sessions.len() as isize) as usize;
        self.selected.set(sessions[next].id);
    }

}

/// Build the tabs view: one GpuSurface per session.
// `Tabs::new` needs a fully materialized `Vec<Tab>` — hydrolysis has no
// reactive-collection tab API yet, so the whole set is rebuilt on change.
// Each `GpuSurface` keeps its `Arc<Terminal>` alive across rebuilds.
#[allow(watch_over_collection)]
pub fn tabs_view(state: AppState) -> impl View {
    watch(state.tab_ids.clone(), move |ids| {
        let sessions = state.sessions();
        let tabs: Vec<Tab<u64>> = ids
            .iter()
            .filter_map(|id| sessions.iter().find(|s| s.id == *id).cloned())
            .map(|session| {
                let app = state.clone();
                Tab::container(session.id, session.title.clone(), move || {
                    GpuSurface::new(TermSurface::new(session.clone(), app.clone()))
                })
            })
            .collect();
        if tabs.is_empty() {
            // Hydrolysis requires at least one tab — a lone placeholder while
            // the last session exits.
            let t = Tab::container(0u64, "hydroterm", || text("No sessions"));
            return Tabs::new(&state.selected, vec![t]);
        }
        Tabs::new(&state.selected, tabs)
    })
}
