//! Application state: sessions, tabs, split-pane trees, focus tracking,
//! and the actions surfaces trigger (new/close/cycle, splits, clipboard).

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use alacritty_terminal::term::Config;
use alacritty_terminal::tty::Shell;
use alacritty_terminal::vte::ansi::CursorStyle;
use nami::collection::{Collection, List as NamiList};
use nami::zip::zip;
use nami::{Binding, Signal, binding};
use waterui::state;
use waterui::theme::ColorScheme;
use waterui::Identifiable;
use waterui_core::id::SelfId;
use waterui::layout::frame::Frame;
use waterui::prelude::*;
use waterui::widget::condition::when;
use waterui::window::{Window, WindowState, WindowStyle, conditional_window};
use waterui::window::WindowPresentation;
use waterui::task::{sleep, spawn_local};
use waterui_core::layout::{Point, Rect, Size};
use waterui_graphics::SceneView;
use waterui::snackbar::{Snackbar, SnackbarManager};
use waterui::accessibility::{AccessibilityRole, AccessibilityState};
use waterui::drag_drop::DragData;
use waterui::theme::color::{Accent, Background, Border, Foreground, MutedForeground, Surface};
use waterui_graphics::color::{Color, Srgb, signal_color};
use waterui_text::FontCollection;

use crate::config::{AppConfig, ConfigWatcher};
use crate::keys::TermAction;
use crate::palette::Palette;
use crate::surface::TermSurface;
use crate::terminal::Terminal;
use waterui::form::picker::picker;

/// Fixed height of the tab strip at every window size.
const TAB_STRIP_HEIGHT: f32 = 30.0;
/// Payload prefix for internal tab-chip drags — must never reach a pane's
/// drop handler, where it would be pasted into the PTY as shell text.
/// Workaround for `DragData` having only Text/Url payloads; remove once
/// typed drag payloads land (water-rs/waterui#1254).
const TAB_DRAG_PREFIX: &str = "hydroterm-tab:";

/// Shared per-session UI state: the bindings a pane surface reads, plus
/// the owning `Terminal` (PTY + grid).
/// OSC 52 clipboard-read reply formatter (alacritty's `fmt` closure).
type ClipboardReply = std::sync::Arc<dyn Fn(&str) -> String + Sync + Send>;

pub struct Session {
    pub id: u64,
    /// The terminal (Term + I/O loop + PTY channel).
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
    /// The program currently holds mouse reporting (DECSET 1000/1002/
    /// 1006) — while on, secondary clicks belong to it and the context
    /// menu is suppressed (signal-driven `.context_menu` items).
    pub mouse_reporting: Binding<bool>,
    /// Configured `font-family` preference — hot-reload re-resolves the
    /// shaping stack when it changes.
    pub font_family: Binding<Str>,
    /// Clipboard text awaiting paste-protection confirmation.
    pub pending_paste: Binding<Option<Str>>,
    /// Live scrollback limit (the alacritty field is private — tracked
    /// here so config hot-reload can compare and `set_options`).
    pub scrollback: std::sync::Mutex<usize>,
    /// `word-select-chars` the session spawned with — same
    /// compare-and-`set_options` live-reload path as scrollback.
    pub word_chars: std::sync::Mutex<String>,
    /// Cursor style the session spawned with — preserved across
    /// `set_options` live reloads.
    pub cursor_style: std::sync::Mutex<CursorStyle>,
    /// Current kitty-keyboard flag state source (kept for set_options).
    pub kitty_keyboard: bool,
    /// The window's snackbar manager, captured by the pane's `on_appear` —
    /// used to show (and dismiss) the paste-protection confirmation.
    pub snackbar: RefCell<Option<SnackbarManager>>,
    /// `unfocused-split-opacity` — alpha applied when this pane is not
    /// the tab's focused split; live-reloaded via `poll_config`.
    pub unfocused_opacity: Binding<f32>,
    /// `right-click-action = context-menu` — the pane's `.context_menu`
    /// items exist only for the menu action; copy/paste/ignore deliver
    /// the secondary click to the scene instead.
    pub context_menu_enabled: Binding<bool>,
    /// cols×rows text while resizing (Ghostty `resize-overlay`);
    /// `None` when no recent size change.
    pub resize_label: Binding<Option<Str>>,
    /// Last logical size (points) the scene builder handed this pane —
    /// feeds the split-divider drag, which needs real pixel extents.
    pub pane_px: Binding<(f32, f32)>,
    /// `confirm-close` prompt: `Some((program_label, whole_tab))` while
    /// the snackbar asks before killing a busy pane/tab.
    pub pending_close: Binding<Option<(Str, bool)>>,
    /// Spawned with `command`/`-e` — `wait-after-command` holds this
    /// surface open on child exit instead of closing the tab.
    pub ran_command: bool,
    /// `clipboard-read = ask`: an OSC 52 `?` request waits for Allow /
    /// Enter; the response formatter is stashed alongside.
    pub pending_clipboard_read: Binding<bool>,
    /// The OSC 52 reply formatter captured while the prompt is up.
    pub pending_clipboard_fmt: Rc<RefCell<Option<ClipboardReply>>>,
    /// Ghostty `prompt_title` — the rename prompt is open over the pane.
    pub title_prompt_open: Binding<bool>,
    /// Live rename text — bound to the prompt's `TextField`; seeded
    /// with the current title when the prompt opens.
    pub title_query: Binding<Str>,
    /// Ghostty `inspector` — a chip reports the attributes of the cell
    /// under the terminal cursor, refreshed every rendered frame.
    pub inspector_open: Binding<bool>,
    /// Text of the inspector chip — rewritten each frame while open.
    pub inspector_label: Binding<Str>,
    /// Live cell metrics in points — set by `sync_size`; `split_pane` and
    /// `new_tab` read it to size a new pane's PTY at birth so the first
    /// `sync_size` is a same-size no-op (no ioctl, no SIGWINCH, no
    /// prompt-clear that a never-sent repaint can't refill).
    pub cell_px: std::cell::Cell<(f32, f32)>,
    /// A zoom action (`increase_font_size` / `decrease_font_size` /
    /// `set_font_size`) overrode the config size — config reloads must
    /// not clobber it (Ghostty: `font-size` applies to terminals that
    /// never changed it; `reset_font_size` clears the flag).
    pub font_size_override: std::cell::Cell<bool>,
    /// The open rename prompt writes the owning tab's title rather than
    /// the surface's (`prompt_tab_title` vs `prompt_surface_title`).
    pub title_prompt_writes_tab: std::cell::Cell<bool>,
}

impl Session {
    /// Queue an action the surface drains on its next frame — shared by
    /// the command palette and the pane's context menu.
    pub fn push_action(&self, action: TermAction) {
        self.pending_actions.borrow_mut().push(action);
        self.terminal.proxy.request_frame();
    }

    fn spawn(
        id: u64,
        cwd: Option<std::path::PathBuf>,
        cfg: &AppConfig,
        initial_size: Option<(usize, usize, (u16, u16))>,
    ) -> Self {
        let config = Config {
            scrolling_history: cfg.scrollback,
            // Loads reach the app's `clipboard-read` policy (allow/ask/deny)
            // instead of alacritty denying them upstream.
            osc52: alacritty_terminal::term::Osc52::CopyPaste,
            kitty_keyboard: true,
            default_cursor_style: CursorStyle {
                shape: cfg.cursor_shape,
                blinking: cfg.cursor_blink,
            },
            // `word-select-chars` — double-click word separators
            // (alacritty `semantic_escape_chars`).
            semantic_escape_chars: cfg.word_select_chars.clone(),
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
        // Born at the pane's expected grid size when the caller knows it
        // (split/tab spawn) — otherwise a reasonable initial grid that the
        // surface resizes on its first frame.
        let (cols, lines, cell_px) = initial_size.unwrap_or((120, 32, (9, 18)));
        let terminal = Terminal::spawn(
            config.clone(),
            cols,
            lines,
            cell_px,
            crate::terminal::SpawnOpts {
                cwd,
                shell,
                term_name: &cfg.term,
                env_extra: &cfg.env,
                shell_integration: cfg.shell_integration,
                shell_features: cfg.shell_features,
            },
        )
            .expect("failed to spawn PTY — is a shell available?");
        Self {
            id,
            terminal: Arc::new(terminal),
            title: binding(Str::from(cfg.title.clone().unwrap_or_else(|| "Shell".into()))),
            base_title: std::sync::Mutex::new(Str::from(
                cfg.title.clone().unwrap_or_else(|| "Shell".into()),
            )),
            notify_badge: std::sync::Mutex::new(false),
            font_size: Binding::f32(cfg.font_size),
            exited: Binding::bool(false),
            cwd: std::sync::Mutex::new(None),
            search_open: Binding::bool(false),
            search_query: binding(Str::from("")),
            search_status: binding(Str::from("")),
            kitty: Rc::new(RefCell::new(crate::kitty::KittyStore::default())),
            pending_actions: Rc::new(RefCell::new(Vec::new())),
            mouse_reporting: Binding::bool(false),
            font_family: binding(Str::from(cfg.font_family.clone())),
            pending_paste: Binding::default(),
            scrollback: std::sync::Mutex::new(cfg.scrollback),
            word_chars: std::sync::Mutex::new(cfg.word_select_chars.clone()),
            unfocused_opacity: Binding::f32(cfg.unfocused_split_opacity),
            context_menu_enabled: Binding::bool(
                cfg.right_click_action == crate::config::RightClickAction::ContextMenu,
            ),
            resize_label: Binding::default(),
            cell_px: std::cell::Cell::new((0.0, 0.0)),
            cursor_style: std::sync::Mutex::new(config.default_cursor_style),
            kitty_keyboard: config.kitty_keyboard,
            ran_command: cfg.command.is_some(),
            snackbar: RefCell::new(None),
            pane_px: Binding::default(),
            pending_close: Binding::default(),
            pending_clipboard_read: Binding::default(),
            pending_clipboard_fmt: Rc::new(RefCell::new(None)),
            title_prompt_open: Binding::bool(false),
            title_query: binding(Str::from("")),
            inspector_open: Binding::bool(false),
            inspector_label: binding(Str::from("")),
            font_size_override: std::cell::Cell::new(false),
            title_prompt_writes_tab: std::cell::Cell::new(false),
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

/// Ghostty `new_split:auto` — split along the pane's long axis so both
/// children stay closer to square: a pane wider than it is tall splits
/// side-by-side (right), a taller one splits top-to-bottom (down).
pub fn auto_split_dir(width: f32, height: f32) -> SplitDir {
    if width >= height {
        SplitDir::Row
    } else {
        SplitDir::Column
    }
}

/// Pane layout tree inside one tab. Leaves hold session ids.
#[derive(Clone)]
pub enum SplitNode {
    Leaf(u64),
    Split {
        dir: SplitDir,
        children: Vec<SplitNode>,
        /// Per-child main-axis sizes in points. Seeded from measured pane
        /// rects on first layout and kept by the divider drag; a
        /// `Binding` (not plain data) so the drag resizes the layout
        /// reactively without rewriting the watched tree mid-gesture.
        sizes: Binding<Vec<f32>>,
    },
}

impl SplitNode {
    /// Replace leaf `target` with a `dir` split containing
    /// `[target, new_leaf]` — the new pane takes half the target's slot,
    /// like tmux/iTerm pane splits. `slot_px` is the target leaf's
    /// main-axis extent in points — both children start at half of it.
    /// Split `target` along `dir`; `before` inserts the new leaf ahead of
    /// the target (left/up) instead of after it (right/down).
    fn split(
        &mut self,
        dir: SplitDir,
        target: u64,
        new_id: u64,
        slot_px: f32,
        before: bool,
    ) -> bool {
        match self {
            Self::Leaf(id) if *id == target => {
                // The divider claims DIVIDER_PX of the slot; seed equal
                // shares of what remains so the row's frames sum to
                // exactly slot_px and report no overflow minimum.
                let half = (slot_px - DIVIDER_PX) / 2.0;
                let children = if before {
                    vec![Self::Leaf(new_id), Self::Leaf(target)]
                } else {
                    vec![Self::Leaf(target), Self::Leaf(new_id)]
                };
                *self = Self::Split {
                    dir,
                    children,
                    sizes: binding(vec![half, half]),
                };
                true
            }
            Self::Leaf(_) => false,
            Self::Split { children, .. } => children
                .iter_mut()
                .any(|c| c.split(dir, target, new_id, slot_px, before)),
        }
    }

    /// Remove leaf `sid`; collapse single-child splits. Returns the
    /// surviving tree, or `None` when `sid` was the only leaf.
    fn remove(&self, sid: u64) -> Option<Self> {
        match self {
            Self::Leaf(id) if *id == sid => None,
            Self::Leaf(id) => Some(Self::Leaf(*id)),
            Self::Split {
                dir,
                children,
                sizes,
            } => {
                let recorded = sizes.snapshot();
                let kept: Vec<(f32, SplitNode)> = children
                    .iter()
                    .enumerate()
                    .filter_map(|(i, c)| {
                        c.remove(sid)
                            .map(|k| (recorded.as_slice().get(i).copied().unwrap_or(0.0), k))
                    })
                    .collect();
                match kept.as_slice() {
                    [] => None,
                    [(_, only)] => Some(only.clone()),
                    _ => Some(Self::Split {
                        dir: *dir,
                        children: kept.iter().map(|(_, c)| c.clone()).collect(),
                        sizes: binding(kept.iter().map(|(s, _)| *s).collect::<Vec<f32>>()),
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

    /// Does this subtree contain leaf `id`?
    fn contains(&self, id: u64) -> bool {
        match self {
            Self::Leaf(i) => *i == id,
            Self::Split { children, .. } => children.iter().any(|c| c.contains(id)),
        }
    }

    /// The leaf at the `first` (left/top) or last (right/bottom) edge.
    fn edge_leaf(&self, first: bool) -> u64 {
        match self {
            Self::Leaf(id) => *id,
            Self::Split { children, .. } => {
                children[if first { 0 } else { children.len() - 1 }].edge_leaf(first)
            }
        }
    }

    /// Where a directional focus move from `focus` lands — the sibling
    /// subtree across the nearest matching-axis split that has one, or
    /// `None` at the layout's edge. `horizontal` selects left/right
    /// (`Row` splits), `forward` = right/down.
    fn neighbor(&self, focus: u64, horizontal: bool, forward: bool) -> Option<u64> {
        let Self::Split { dir, children, .. } = self else {
            return None;
        };
        let i = children.iter().position(|c| c.contains(focus))?;
        if matches!(dir, SplitDir::Row) == horizontal {
            let j = if forward {
                i + 1
            } else {
                i.checked_sub(1).unwrap_or(usize::MAX)
            };
            if j < children.len() {
                // Enter the sibling from the edge facing the current pane.
                return Some(children[j].edge_leaf(forward));
            }
        }
        children[i].neighbor(focus, horizontal, forward)
    }

    /// Move the divider beside `focus` by `delta` main-axis points
    /// (positive = right/down). Picks the boundary on the signed side of
    /// the focused child — or its only boundary at an edge — and shifts
    /// it, clamping both panes at `MIN_PANE_PX` while the pair total
    /// stays constant. Recurses until a matching-axis split is found.
    fn resize_focus(&self, focus: u64, horizontal: bool, delta: f32) -> bool {
        let Self::Split {
            dir,
            children,
            sizes,
        } = self
        else {
            return false;
        };
        let Some(i) = children.iter().position(|c| c.contains(focus)) else {
            return false;
        };
        if matches!(dir, SplitDir::Row) != horizontal {
            return children[i].resize_focus(focus, horizontal, delta);
        }
        let mut v = sizes.snapshot();
        if v.len() != children.len() || !v.iter().all(|s| *s > 0.0) {
            // Not seeded yet — nothing measured to redistribute.
            return true;
        }
        let b = if delta > 0.0 {
            i.min(children.len() - 2)
        } else {
            i.saturating_sub(1)
        };
        let pair = v[b] + v[b + 1];
        v[b] = (v[b] + delta).clamp(MIN_PANE_PX, pair - MIN_PANE_PX);
        v[b + 1] = pair - v[b];
        sizes.set(v);
        true
    }

    /// Give every split's children equal shares of their measured total
    /// (`equalize_splits` — tmux `select-layout -E`). Sizes are absolute
    /// points, so equal shares = total / k; unseeded splits stay
    /// untouched (they equalize by layout anyway).
    fn equalize(&self) {
        if let Self::Split {
            children, sizes, ..
        } = self
        {
            let v = sizes.snapshot();
            if v.len() == children.len() && v.iter().all(|s| *s > 0.0) {
                let each = v.iter().sum::<f32>() / v.len() as f32;
                sizes.set(vec![each; v.len()]);
            }
            for c in children {
                c.equalize();
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
    /// Focused session id inside `tree` — the per-tab *record* of which
    /// pane is active (drives the unfocused-dim + title sync). The actual
    /// embedded key focus lives in `AppState::focus_owner` and syncs here
    /// through the `on_change` watcher in `tabs_view`.
    pub focused: Binding<u64>,
    /// Zoomed pane: `Some(id)` renders only that leaf (it fills the tab);
    /// `None` = normal split layout. tmux zoom / kitty overlay semantics.
    pub zoomed: Binding<Option<u64>>,
    /// Unseen-output marker: set when a parser wake lands while this tab
    /// is not selected; cleared when it becomes selected. kitty's
    /// `tab_activity_symbol`.
    pub activity: Binding<bool>,
    /// `set_tab_title` / `prompt_tab_title` override — while `Some`, the
    /// title sync writers leave `title` alone (Ghostty: a set tab title
    /// persists across focus changes within the tab).
    pub title_override: Binding<Option<Str>>,
    /// `bell-features` `attention` indicator — mirrors the owning
    /// session's `notify_badge` so the chip can react to it.
    pub badge: Binding<bool>,
}

/// Everything tabs and surfaces share.
#[derive(Clone)]
#[state]
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
    /// Current tab count — kept in sync at push/remove so the strip can
    /// gate on `tab-bar-min-tabs` reactively (NamiList itself is not a
    /// signal).
    pub tab_count: Binding<usize>,
    /// Live `keybind = global:chord=action` X11 grabs — each entry's
    /// flag stops its grab thread (dropping the connection releases
    /// the key). Config reloads diff this against the new keybinds.
    pub global_grabs: RefCell<Vec<(String, TermAction, std::sync::Arc<AtomicBool>)>>,
    /// The `quick-terminal-position`/`quick-terminal-size` pair the
    /// cached quick app was built with — a changed value drops it.
    applied_quick_geo: RefCell<QuickGeo>,
    /// Live-applied `tab-bar-min-tabs` config value.
    pub tab_bar_min: Binding<usize>,
    /// `toggle_tab_bar` manual override: `Some(true)` forces the strip
    /// visible, `Some(false)` hides it; `None` follows `tab-bar-min-tabs`.
    pub tab_bar_forced: Binding<Option<bool>>,
    /// Previously-selected tab id for `last_tab` (0 = none).
    pub last_tab_id: Rc<std::cell::Cell<u64>>,
    /// `(tab_id, session_id)` of the pane holding embedded key focus —
    /// the single source `.focused` modifiers consume. `None` while no
    /// pane is focused (e.g. focus on the search field). Written back by
    /// hydrolysis when pointer focus moves between surfaces.
    pub focus_owner: Binding<Option<(u64, u64)>>,
    /// Window title binding.
    pub window_title: Binding<Str>,
    /// Window state binding — normal/minimized/fullscreen/closed.
    /// Owned by us so keybinds can toggle fullscreen.
    pub window_state: Binding<WindowState>,
    /// The main window's `frame` binding (hydrolysis writes live geometry
    /// back on Moved/Resize) — captured in `main` so the save-state poller
    /// can persist it.
    pub window_frame: Rc<RefCell<Option<Binding<Rect>>>>,
    /// `window-theme` mapped to the WaterUI color scheme — `hydroterm::app`
    /// installs this binding as the environment's `Theme::color_scheme`, so
    /// a config reload switches the chrome scheme live (cli#188 contract).
    pub window_scheme: Binding<ColorScheme>,
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
    pub palette_sel: Binding<Option<usize>>,
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
    /// Hotkey fires but grab failed (no X11) — surface it once.
    pub quick_unavailable: RefCell<bool>,
    /// True for the drop-down's own AppState: it neither hosts a quick
    /// window itself nor spawns a second key grab.
    /// True after the first `spawn_session` — `command` is consumed as
    /// initial-surface-only and never re-applied by a hot reload.
    initial_spawn: std::cell::Cell<bool>,
    is_quick: bool,
    /// Weak handles to live terminals so the theme monitor thread can
    /// request frames (dirty is only read inside `poll_config`).
    theme_wakes: Arc<Mutex<Vec<std::sync::Weak<Terminal>>>>,
    next_id: Arc<AtomicU64>,
}

// `.state(&app)` injection rows read the state back through a plain
// `AppState` extractor parameter.


/// Extractor key for a pane's session — a local newtype because the
/// orphan rule won't let `Extractor` (foreign) be implemented for
/// `Rc<Session>` (also foreign). Per-pane `.state(&PaneSession(..))` lets
/// context-menu items and bar buttons take the session from the env.
#[state]
#[derive(Clone)]
pub struct PaneSession(pub Rc<Session>);

impl std::ops::Deref for PaneSession {
    type Target = Session;
    fn deref(&self) -> &Session {
        &self.0
    }
}

impl AppState {
    /// Create with one running session.
    // Sessions never leave the UI thread (PTY events arrive through a channel
    // and are consumed in `render`), so the `Arc`s only need UI confinement,
    // not Send+Sync — `Binding` is not Send+Sync by design.
    #[allow(clippy::arc_with_non_send_sync)]
    pub fn new(config_path: Option<std::path::PathBuf>, command: Option<Vec<String>>) -> Self {
        let mut watcher = ConfigWatcher::new(config_path);
        if command.is_some() {
            // `-e` wins over a config-file `command =`; neither survives
            // past the first session (initial-surface semantics).
            watcher.config.command = command;
        }
        for e in &watcher.errors {
            eprintln!("hydroterm config: {e}");
        }
        let palette = Palette::for_config(&watcher.config);
        #[cfg(target_os = "linux")]
        let theme_is_auto = matches!(watcher.config.theme, crate::config::ThemeRef::Auto)
            || matches!(watcher.config.window_theme, crate::config::WindowTheme::Auto);
        let applied_quick_geo = (
            watcher.config.quick_terminal_position,
            watcher.config.quick_terminal_size,
        );
        let quick_binding = Binding::container(WindowState::Closed);
        let state = Self {
            sessions: Rc::new(RefCell::new(Vec::new())),
            tabs: NamiList::new(),
            session_tab: Arc::new(Mutex::new(HashMap::new())),
            selected: Binding::u64(0),
            tab_count: Binding::usize(0),
            tab_bar_min: Binding::usize(watcher.config.tab_bar_min_tabs),
            tab_bar_forced: Binding::default(),
            last_tab_id: Rc::new(std::cell::Cell::new(0)),
            focus_owner: Binding::default(),
            window_title: binding({
                let base = watcher
                    .config
                    .title
                    .clone()
                    .unwrap_or_else(|| "hydroterm".into());
                match &watcher.config.window_subtitle {
                    Some(s) => Str::from(format!("{base} — {s}")),
                    None => Str::from(base.to_string()),
                }
            }),
            window_state: binding(WindowState::Normal),
            window_frame: Rc::new(RefCell::new(None)),
            window_scheme: Binding::container(crate::theme::scheme_for(
                &watcher.config.window_theme,
            )),
            cfg: Rc::new(RefCell::new(watcher)),
            palette: Rc::new(RefCell::new(palette)),
            env: Rc::new(std::cell::OnceCell::new()),
            palette_open: Binding::bool(false),
            palette_query: binding(Str::from("")),
            palette_sel: Binding::container(Some(0)),
            palette_scroll: ScrollController::new(0),
            settings_open: Binding::bool(false),
            set_font: Binding::i32(13),
            set_theme: Binding::usize(0),
            set_blink: Binding::bool(true),
            theme_dirty: Arc::new(AtomicBool::new(false)),
            quick_state: quick_binding.clone(),
            quick_presentation: WindowPresentation::new(&quick_binding),
            quick_app: RefCell::new(None),
            global_grabs: RefCell::new(Vec::new()),
            applied_quick_geo: RefCell::new(applied_quick_geo),
            quick_listener_started: Rc::new(AtomicBool::new(false)),
            quick_task: Rc::new(RefCell::new(None)),
            quick_unavailable: RefCell::new(false),
            initial_spawn: std::cell::Cell::new(false),
            is_quick: false,
            theme_wakes: Arc::new(Mutex::new(Vec::new())),
            next_id: Arc::new(AtomicU64::new(0)),
        };
        let first_tab = state.new_tab();
        // Seed the embedded-focus owner so `.focused` grants key focus to
        // the first pane at mount — the launch dead-keys fix (#29).
        if let Some(t) = state.tabs.iter().find(|t| t.id == first_tab) {
            state.focus_owner.set(Some((first_tab, t.focused.snapshot())));
        }
        // `-e` applies to the first session only (like xterm/kitty).
        state.cfg.borrow_mut().config.command = None;
        // `theme = auto` / `window-theme = auto`: watch the desktop
        // color-scheme. gsettings `monitor` prints a line per change;
        // flag dirty + poke every live surface so `poll_config`
        // re-resolves on its next frame.
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
            let config = &self.cfg.borrow().config;
            *self.palette.borrow_mut() = Palette::for_config(config);
            if matches!(config.window_theme, crate::config::WindowTheme::Auto) {
                self.window_scheme
                    .set(crate::theme::scheme_for(&config.window_theme));
            }
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
        self.apply_config(&config);
    }

    /// Grab one `global:chord` on the X11 root window and register it
    /// in `grabs` — shared by `start_global_hotkeys` and reloads.
    fn grab_global(
        &self,
        grabs: &mut Vec<(String, TermAction, std::sync::Arc<AtomicBool>)>,
        chord: &str,
        action: &TermAction,
    ) {
        // Canonical order: ctrl+alt+shift+super+key.
        let body = chord;
        let mut mods = [false; 4];
        let mut key = "";
        for part in body.split('+') {
            match part {
                "ctrl" => mods[0] = true,
                "alt" => mods[1] = true,
                "shift" => mods[2] = true,
                "super" => mods[3] = true,
                k => key = k,
            }
        }
        let Some(keysym) = crate::quickterm::key_name_to_keysym(key) else {
            tracing::warn!(chord, "global: unmapped key name");
            return;
        };
        let mod_bits =
            crate::quickterm::chord_mod_bits(mods[0], mods[1], mods[2], mods[3]);
        let (tx, rx) = async_channel::unbounded::<TermAction>();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        match crate::quickterm::spawn_hotkey(tx, keysym, mod_bits, action.clone(), stop.clone()) {
            Some(_) => {
                grabs.push((chord.to_string(), action.clone(), stop));
                let app = self.clone();
                spawn_local(async move {
                    while let Ok(action) = rx.recv().await {
                        while rx.try_recv().is_ok() {}
                        app.run_palette_action(action);
                    }
                })
                .detach();
            }
            None => {
                tracing::warn!(chord, "global: grab failed (taken or no X11)");
            }
        }
    }

    /// Sync `keybind = global:*` grabs with `config.keybinds`: drops
    /// chords that vanished or changed action (the flag stops the grab
    /// thread; its connection drop releases the key) and grabs new
    /// ones (Ghostty re-grabs global binds on config reload).
    fn regrab_globals(&self, config: &AppConfig) {
        let mut grabs = self.global_grabs.borrow_mut();
        grabs.retain(|(chord, action, stop)| {
            let keep = config
                .keybinds
                .iter()
                .any(|(t, a)| t.global && t.chord == *chord && a.as_ref() == Some(action));
            if !keep {
                stop.store(true, Ordering::SeqCst);
            }
            keep
        });
        let want: Vec<(String, TermAction)> = config
            .keybinds
            .iter()
            .filter(|(t, _)| t.global)
            .filter_map(|(t, a)| a.clone().map(|a| (t.chord.clone(), a)))
            .collect();
        for (chord, action) in want {
            if grabs.iter().any(|(c, a, _)| c == &chord && a == &action) {
                continue;
            }
            self.grab_global(&mut grabs, &chord, &action);
        }
    }

    /// `reload-config` action: re-read the file and apply it now —
    /// the same code path the mtime watcher uses.
    pub fn reload_config(&self) {
        let (config, errors) = {
            let mut w = self.cfg.borrow_mut();
            w.reload();
            (w.config.clone(), w.errors.clone())
        };
        for e in &errors {
            eprintln!("hydroterm config: {e}");
        }
        self.apply_config(&config);
    }

    /// Ghostty `goto_split`: focus the nth leaf of the selected tab.
    /// `usize::MAX` is the last leaf (`goto_split:bottom`).
    pub fn goto_split(&self, index: usize) {
        let tab_id = self.selected.snapshot();
        let Some(tab) = self.tabs.iter().find(|t| t.id == tab_id) else {
            return;
        };
        let tree: SplitNode = tab.tree.snapshot();
        // `.as_slice()` — nami's `Signal` blanket impl on `Vec` makes
        // `vec.get(i)` resolve to the 0-arg `Signal::get`.
        let leaves = tree.leaves();
        let index = if index == usize::MAX {
            leaves.len().saturating_sub(1)
        } else {
            index
        };
        if let Some(&sid) = leaves.as_slice().get(index) {
            self.focus_pane(sid);
        }
    }

    /// Live-apply a freshly parsed config — shared by `poll_config`
    /// (mtime watcher) and `reload_config` (the manual action).
    fn apply_config(&self, config: &AppConfig) {
        *self.palette.borrow_mut() = Palette::for_config(config);
        self.tab_bar_min.set(config.tab_bar_min_tabs);
        // A reload restores the `window-show-tab-bar` threshold — the
        // `toggle_tab_bar` manual override was documented to hold only
        // until the next reload.
        self.tab_bar_forced.set(None);
        self.window_scheme
            .set(crate::theme::scheme_for(&config.window_theme));
        // `title` — a configured window title re-applies on reload
        // (Ghostty updates every window's title).
        if let Some(t) = &config.title {
            self.window_title.set(self.title_with_subtitle(t));
        }
        // `quick-terminal-position`/`quick-terminal-size` are
        // new-window options — a changed value drops the cached quick
        // app so the next `toggle_quick_terminal` rebuilds with it.
        {
            let geo = (config.quick_terminal_position, config.quick_terminal_size);
            if *self.applied_quick_geo.borrow() != geo {
                self.applied_quick_geo.replace(geo);
                *self.quick_app.borrow_mut() = None;
            }
        }
        // `global:` chords — release removed/changed grabs, grab new
        // ones (Ghostty re-grabs global binds on config reload).
        self.regrab_globals(config);
        for s in self.sessions.borrow().iter() {
            // `font-size` applies on reload — but only to terminals
            // that never took a zoom override (`increase_font_size`, …).
            if !s.font_size_override.get() {
                s.font_size.set(config.font_size);
            }
            s.font_family.set_from(Str::from(config.font_family.clone()));
            s.unfocused_opacity.set(config.unfocused_split_opacity);
            s.context_menu_enabled
                .set(config.right_click_action == crate::config::RightClickAction::ContextMenu);
            // Live scrollback-limit / cursor-style change — `set_options`
            // is alacritty's own live-reconfigure path. Rebuild the Config
            // exactly as spawn does so kitty-keyboard survives intact.
            let cursor_style = CursorStyle {
                shape: config.cursor_shape,
                blinking: config.cursor_blink,
            };
            if *s.scrollback.lock().unwrap() != config.scrollback
                || *s.cursor_style.lock().unwrap() != cursor_style
                || *s.word_chars.lock().unwrap() != config.word_select_chars
            {
                let mut term = s.terminal.term.lock();
                term.set_options(alacritty_terminal::term::Config {
                    osc52: alacritty_terminal::term::Osc52::CopyPaste,
                    scrolling_history: config.scrollback,
                    kitty_keyboard: s.kitty_keyboard,
                    default_cursor_style: cursor_style,
                    semantic_escape_chars: config.word_select_chars.clone(),
                    ..Default::default()
                });
                *s.scrollback.lock().unwrap() = config.scrollback;
                *s.cursor_style.lock().unwrap() = cursor_style;
                *s.word_chars.lock().unwrap() = config.word_select_chars.clone();
            }
        }
    }

    /// Mark the tab owning `session_id` as having unseen output — the
    /// tab strip dots it until the tab is selected again. Called from
    /// each surface's parser-wake (which fires on new output).
    pub fn note_activity(&self, session_id: u64) {
        let Some(&tab_id) = self.session_tab.lock().unwrap().get(&session_id)
        else {
            return;
        };
        if self.selected.snapshot() == tab_id {
            return;
        }
        if let Some(t) = self.tabs.iter().find(|t| t.id == tab_id) {
            t.activity.set(true);
        }
    }

    /// Read-only access to the current config.
    pub fn config<R>(&self, f: impl FnOnce(&AppConfig) -> R) -> R {
        f(&self.cfg.borrow().config)
    }

    /// Toggle borderless fullscreen on the main window.
    pub fn toggle_fullscreen(&self) {
        let next = match self.window_state.snapshot() {
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
        let next = match self.quick_state.snapshot() {
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
        let (tx, rx) = async_channel::unbounded::<()>();
        match crate::quickterm::spawn_hotkey(
            tx,
            crate::quickterm::XK_F12,
            0,
            (),
            std::sync::Arc::new(AtomicBool::new(false)),
        ) {
            Some(_) => {
                let app = self.clone();
                let task = spawn_local(async move {
                    while let Ok(()) = rx.recv().await {
                        while rx.try_recv().is_ok() {}
                        app.toggle_quick();
                    }
                });
                *self.quick_task.borrow_mut() = Some(Box::new(task));
            }
            None => {
                *self.quick_unavailable.borrow_mut() = true;
            }
        }
        self.start_global_hotkeys();
    }

    /// Grab each `keybind = global:chord=action` on the X11 root window.
    /// A chord press anywhere fires the action through the same dispatch
    /// as a window-local keybind (focused session first, app-level
    /// fallback). Grabbed once per launch; config reloads re-sync the
    /// set through `regrab_globals`.
    fn start_global_hotkeys(&self) {
        let config = self.config(|c| c.clone());
        self.regrab_globals(&config);
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
        // `quick-terminal-position` — dock geometry on the primary
        // screen (top/bottom: full width × 45% height; left/right:
        // 40% width × full height; center: 70%×70% centered). The initial
        // frame is requested up front — position applied while the X11
        // window is still invisible is currently dropped by the WM
        // (hydrolysis#105), so the drop-down may land wherever the window
        // manager places it until that fix lands. No timed re-emit: racing
        // the WM is forbidden workaround, not a fix.
        if let Some((sw, sh)) = crate::quickterm::screen_size() {
            use crate::config::{QuickTermPosition as P, QuickTermSize as S};
            // `quick-terminal-size = <primary>[,<secondary>]` — the
            // primary axis is height for top/bottom, width for
            // left/right, and follows the monitor orientation for
            // center; the secondary axis is maximized for edge-docked
            // positions unless a second size is given (Ghostty).
            let size = self.config(|c| c.quick_terminal_size);
            let axis = |v: Option<S>, full: f64| match v {
                Some(S::Percent(p)) => full * p / 100.0,
                Some(S::Px(px)) => px,
                None => full,
            };
            let (x, y, w_px, h_px) = match self.config(|c| c.quick_terminal_position) {
                P::Top => {
                    let h = size.map_or(sh * 0.45, |(a, _)| axis(Some(a), sh));
                    let w = size
                        .and_then(|(_, b)| b)
                        .map_or(sw, |b| axis(Some(b), sw));
                    ((sw - w) / 2.0, 0.0, w, h)
                }
                P::Bottom => {
                    let h = size.map_or(sh * 0.45, |(a, _)| axis(Some(a), sh));
                    let w = size
                        .and_then(|(_, b)| b)
                        .map_or(sw, |b| axis(Some(b), sw));
                    ((sw - w) / 2.0, sh - h, w, h)
                }
                P::Left => {
                    let w = size.map_or(sw * 0.40, |(a, _)| axis(Some(a), sw));
                    let h = size
                        .and_then(|(_, b)| b)
                        .map_or(sh, |b| axis(Some(b), sh));
                    (0.0, (sh - h) / 2.0, w, h)
                }
                P::Right => {
                    let w = size.map_or(sw * 0.40, |(a, _)| axis(Some(a), sw));
                    let h = size
                        .and_then(|(_, b)| b)
                        .map_or(sh, |b| axis(Some(b), sh));
                    (sw - w, (sh - h) / 2.0, w, h)
                }
                P::Center => {
                    let landscape = sw >= sh;
                    // For center, the "primary" axis only decides which
                    // axis a single size configures; each axis defaults
                    // to 70% when unspecified.
                    let (a, b) = size.unwrap_or((S::Percent(70.0), Some(S::Percent(70.0))));
                    let primary = axis(Some(a), if landscape { sh } else { sw });
                    let (w, h) = if landscape {
                        (axis(b.or(Some(S::Percent(70.0))), sw), primary)
                    } else {
                        (primary, axis(b.or(Some(S::Percent(70.0))), sh))
                    };
                    ((sw - w) / 2.0, (sh - h) / 2.0, w, h)
                }
            };
            w.frame.set(Rect::new(
                Point::new(x as f32, y as f32),
                Size::new(w_px as f32, h_px as f32),
            ));
        }
        w
    }

    /// `window-width`/`window-height`, `window-position-x`/`y` and
    /// `window-save-state` — the launch-geometry options every window
    /// gets (the first window in `lib.rs` and `new_window` spawns).
    pub(crate) fn apply_launch_geometry(state: &AppState, window: &Window) {
        let rect = if state.config(|c| c.window_save_state) {
            load_window_state()
        } else {
            None
        };
        if let Some(rect) = rect {
            window.frame.set(rect);
        } else {
            let (w, h) = state.config(|c| (c.window_width, c.window_height));
            if w > 0.0 || h > 0.0 {
                let frame = window.frame.snapshot();
                let size = *frame.size();
                window.frame.set(Rect::new(
                    frame.origin(),
                    Size::new(
                        if w > 0.0 { w } else { size.width },
                        if h > 0.0 { h } else { size.height },
                    ),
                ));
            }
            let (wx, wy) = state.config(|c| (c.window_x, c.window_y));
            if wx.is_some() || wy.is_some() {
                let frame = window.frame.snapshot();
                let origin = frame.origin();
                window.frame.set(Rect::new(
                    Point::new(wx.unwrap_or(origin.x), wy.unwrap_or(origin.y)),
                    *frame.size(),
                ));
            }
        }
    }

    /// Spawn a whole new OS window with a fresh session set (same config
    /// file, independent tabs and sessions). Uses the runner's
    /// `WindowManager` — `Window::show` mounts a real winit window.
    pub fn new_window(&self) {
        let Some(env) = self.env.get() else { return };
        let state = AppState::new(Some(self.cfg.borrow().path.clone()), None);
        // Same launch-time transparency as the main window.
        let opacity = state.config(|c| c.background_opacity);
        // `background =` overrides the theme's fill (same as the grid).
        let bg = state.palette.borrow().background;
        let window = Window::new(
            state.window_title.clone(),
            state.window_state.clone(),
            {
                let state = state.clone();
                move || app_root(state.clone())
            },
        )
        // `window-decoration` applies to spawned windows too.
        .style(if state.config(|c| c.window_decoration) {
            WindowStyle::Titled
        } else {
            WindowStyle::Borderless
        })
        .background(Color::srgb(bg.r, bg.g, bg.b).with_opacity(opacity));
        Self::apply_launch_geometry(&state, &window);
        if state.config(|c| c.window_fullscreen) {
            state
                .window_state
                .set(WindowState::Fullscreen);
        }
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

    /// Every live session — `keybind = all:` applies a per-surface
    /// action to each one.
    pub fn all_sessions(&self) -> Vec<Rc<Session>> {
        self.sessions.borrow().iter().cloned().collect()
    }

    fn alloc_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// The focused session of the selected tab, if any.
    pub fn focused_session(&self) -> Option<Rc<Session>> {
        let tab_id = self.selected.snapshot();
        let focused = self
            .tabs.iter().find(|t| t.id == tab_id)
            .map(|t| t.focused.snapshot())?;
        self.session(focused)
    }

    /// Re-grant embedded key focus to `session_id`'s pane after an
    /// overlay (title prompt, confirm/paste snackbar, palette, search
    /// field) unmounts. `.focused` only fires on a `focus_owner` change,
    /// so the widget that held focus unmounting leaves keys orphaned —
    /// poking None → back re-fires the grant.
    pub fn refocus(&self, session_id: u64) {
        let tab_id = self.session_tab.lock().unwrap().get(&session_id).copied();
        if let Some(tab_id) = tab_id {
            self.focus_owner.set(None);
            self.focus_owner.set(Some((tab_id, session_id)));
        }
    }

    /// Refocus the selected tab's focused pane — used by overlay tap
    /// handlers that must return embedded keys to the surface after a
    /// press landed off it (off-surface presses clear embedded focus).
    pub fn refocus_selected(&self) {
        let tab_id = self.selected.snapshot();
        if let Some(session_id) = self
            .tabs
            .iter()
            .find(|t| t.id == tab_id)
            .map(|t| t.focused.snapshot())
        {
            self.refocus(session_id);
        }
    }

    /// Grid dims the new pane will measure — the reference terminal sizes
    /// the PTY at spawn from the GUI's known pane size; doing the same
    /// makes the first `sync_size` a no-op, so no resize ioctl lands
    /// while the shell is still arming its SIGWINCH handler.
    fn initial_grid_for(&self, pane_w: f32, pane_h: f32, cell: (f32, f32)) -> (usize, usize, (u16, u16)) {
        let (wpx, wpy) = self.config(|c| (c.window_padding_x, c.window_padding_y));
        let pad_x = crate::scene::PADDING + wpx;
        let pad_y = crate::scene::PADDING + wpy;
        let cols = ((pane_w - pad_x * 2.0) / cell.0).floor().max(2.0) as usize;
        let lines = ((pane_h - pad_y * 2.0) / cell.1).floor().max(1.0) as usize;
        (cols, lines, (cell.0 as u16, cell.1 as u16))
    }

    /// A focused pane's measured size + cell metrics — the estimate for a
    /// new surface's grid. `None` before the first pane has laid out.
    fn focused_grid_estimate(&self) -> Option<(usize, usize, (u16, u16))> {
        let s = self.focused_session()?;
        let (w, h) = s.pane_px.snapshot();
        let cell = s.cell_px.get();
        (w > 0.0 && cell.0 > 0.0).then(|| self.initial_grid_for(w, h, cell))
    }

    fn spawn_session(
        &self,
        cwd: Option<std::path::PathBuf>,
        initial_size: Option<(usize, usize, (u16, u16))>,
    ) -> Rc<Session> {
        let id = self.alloc_id();
        let mut cfg = self.cfg.borrow().config.clone();
        // `command` (config file or `-e`) is initial-surface only — a
        // hot reload re-populating it must not hijack later spawns.
        if self.initial_spawn.replace(true) {
            cfg.command = None;
        }
        // `working-directory` fills in when no OSC 7 cwd was inherited.
        let cwd = cwd.or_else(|| cfg.working_directory.clone());
        let session = Rc::new(Session::spawn(id, cwd, &cfg, initial_size));
        // `window-inherit-font-size`: a spawned surface takes the
        // focused surface's live zoom instead of the config value —
        // and its override flag, so a config reload doesn't reset it.
        if cfg.inherit_font_size
            && let Some(focused) = self.focused_session()
        {
            session.font_size.set(focused.font_size.snapshot());
            session
                .font_size_override
                .set(focused.font_size_override.get());
        }
        self.sessions.borrow_mut().push(session.clone());
        session
    }

    /// Spawn a session, wrap it in a new tab, select it. Inherits the OSC 7
    /// cwd of the currently focused session when the shell reported one
    /// (`window-inherit-working-directory`).
    pub fn new_tab(&self) -> u64 {
        let cwd = self
            .config(|c| c.inherit_working_directory)
            .then(|| {
                self.focused_session()
                    .and_then(|s| s.cwd.lock().unwrap().clone())
            })
            .flatten();
        let session = self.spawn_session(cwd, self.focused_grid_estimate());
        self.adopt_tab(session)
    }

    /// A new tab running an explicit command instead of the shell —
    /// scrollback-in-editor uses it for `$EDITOR <file>`.
    pub fn new_tab_command(&self, cmd: Vec<String>) -> u64 {
        let mut cfg = self.cfg.borrow().config.clone();
        cfg.command = Some(cmd);
        let id = self.alloc_id();
        let session = Rc::new(Session::spawn(id, None, &cfg, self.focused_grid_estimate()));
        self.sessions.borrow_mut().push(session.clone());
        self.adopt_tab(session)
    }

    fn adopt_tab(&self, session: Rc<Session>) -> u64 {
        let tab = PaneTab {
            id: self.alloc_id(),
            title: binding(session.title.snapshot()),
            tree: binding(SplitNode::Leaf(session.id)),
            focused: Binding::u64(session.id),
            zoomed: Binding::default(),
            activity: Binding::bool(false),
            title_override: Binding::default(),
            badge: Binding::bool(false),
        };
        self.session_tab
            .lock()
            .unwrap()
            .insert(session.id, tab.id);
        let tab_id = tab.id;
        // `window-new-tab-position = current` inserts right after the
        // selected tab instead of appending at the strip's end.
        match self.config(|c| c.new_tab_position) {
            crate::config::NewTabPosition::Current => {
                let sel = self.selected.snapshot();
                let at = self
                    .tabs
                    .iter()
                    .position(|t| t.id == sel)
                    .map(|i| i + 1)
                    .unwrap_or_else(|| self.tabs.len());
                self.tabs.insert(at, tab);
            }
            crate::config::NewTabPosition::End => self.tabs.push(tab),
        }
        self.tab_count.set(self.tabs.len());
        self.selected.set(tab_id);
        tab_id
    }

    /// Split the pane `target` of the selected tab in `dir`; the new pane
    /// inherits the target's cwd.
    /// `before` puts the new pane ahead of the target (left/up split).
    pub fn split_pane(&self, dir: SplitDir, target: u64, before: bool) -> Option<u64> {
        let tab_id = self.selected.snapshot();
        let tab = self
            .tabs.iter().find(|t| t.id == tab_id)?;
        let cwd = self
            .session(target)
            .and_then(|s| s.cwd.lock().unwrap().clone());
        // The new pane's slot: half the target's main-axis extent minus
        // the divider, full extent on the other axis — exact, from the
        // same math `SplitNode::split` seeds the shares with.
        let initial_size = self.session(target).and_then(|s| {
            let (w, h) = s.pane_px.snapshot();
            let cell = s.cell_px.get();
            if w <= 0.0 || cell.0 <= 0.0 {
                return None;
            }
            let (nw, nh) = match dir {
                SplitDir::Row => ((w - DIVIDER_PX) / 2.0, h),
                SplitDir::Column => (w, (h - DIVIDER_PX) / 2.0),
            };
            Some(self.initial_grid_for(nw, nh, cell))
        });
        let session = self.spawn_session(cwd, initial_size);
        let slot_px = self
            .session(target)
            .map(|s| {
                let px = s.pane_px.snapshot();
                match dir {
                    SplitDir::Row => px.0,
                    SplitDir::Column => px.1,
                }
            })
            .unwrap_or_default();
        let ok = tab
            .tree
            .with_mut(|tree| tree.split(dir, target, session.id, slot_px, before));
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
        self.focus_pane(session.id);
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
        if tab.focused.snapshot() == session_id {
            return;
        }
        tab.focused.set(session_id);
        if self.selected.snapshot() == tab_id {
            self.focus_owner.set(Some((tab_id, session_id)));
        }
        if tab.title_override.snapshot().is_none()
            && let Some(s) = self.session(session_id)
        {
            tab.title.set(s.title.snapshot());
        }
    }

    /// `window-subtitle` — the fixed subtitle appended to the window
    /// title (`base — subtitle`); empty config leaves the title alone.
    pub fn title_with_subtitle(&self, base: &str) -> Str {
        match &self.cfg.borrow().config.window_subtitle {
            Some(s) => Str::from(format!("{base} — {s}")),
            None => Str::from(base.to_string()),
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
                && tab.focused.snapshot() == session_id
            {
                if tab.title_override.snapshot().is_none() {
                    tab.title.set(title.clone());
                }
                if self.selected.snapshot() == tab_id {
                    self.window_title.set(self.title_with_subtitle(&title));
                }
            }
    }

    /// Mirror a session's `notify_badge` onto its owning tab chip
    /// (`bell-features` `attention` renders as the chip's 🔔 indicator).
    pub fn tab_badge(&self, session_id: u64, on: bool) {
        if let Some(&tab_id) = self.session_tab.lock().unwrap().get(&session_id)
            && let Some(tab) = self.tabs.iter().find(|t| t.id == tab_id)
        {
            tab.badge.set(on);
        }
    }

    /// `set_tab_title:text` / `prompt_tab_title` — a title on the
    /// session's owning tab that persists across pane-focus changes
    /// (Ghostty). `None` clears the override; the tab resyncs to the
    /// focused session's title.
    pub fn set_tab_title(&self, session_id: u64, title: Option<String>) {
        let Some(&tab_id) = self.session_tab.lock().unwrap().get(&session_id)
        else {
            return;
        };
        let Some(tab) = self.tabs.iter().find(|t| t.id == tab_id) else {
            return;
        };
        match title {
            Some(title) => {
                tab.title_override.set(Some(Str::from(title.clone())));
                tab.title.set_from(title);
            }
            None => {
                tab.title_override.set(None);
                if let Some(s) = self.session(tab.focused.snapshot()) {
                    tab.title.set(s.title.snapshot());
                }
            }
        }
    }

    /// The id of the tab owning `session_id`.
    pub fn tab_id_of(&self, session_id: u64) -> Option<u64> {
        self.session_tab.lock().unwrap().get(&session_id).copied()
    }

    /// The owning tab's current title for `session_id` (the
    /// `prompt_tab_title` seed).
    pub fn tab_title_of(&self, session_id: u64) -> Option<Str> {
        let tab_id = *self.session_tab.lock().unwrap().get(&session_id)?;
        self.tabs.iter().find(|t| t.id == tab_id).map(|t| t.title.snapshot())
    }

    /// Cycle pane focus within the selected tab.
    pub fn cycle_pane(&self, dir: isize) {
        let tab_id = self.selected.snapshot();
        let Some(tab) = self
            .tabs.iter().find(|t| t.id == tab_id)
        else {
            return;
        };
        let leaves = tab.tree.snapshot().leaves();
        if leaves.len() < 2 {
            return;
        }
        let pos = leaves
            .iter()
            .position(|&s| s == tab.focused.snapshot())
            .unwrap_or(0) as isize;
        let next = (pos + dir).rem_euclid(leaves.len() as isize) as usize;
        self.focus_pane(leaves[next]);
    }

    /// Directional pane focus inside the selected tab (Ghostty's
    /// goto_split): the nearest leaf across the matching-axis split.
    /// `horizontal` = left/right, `forward` = right/down.
    pub fn focus_pane_dir(&self, horizontal: bool, forward: bool) {
        let tab_id = self.selected.snapshot();
        let Some(tab) = self.tabs.iter().find(|t| t.id == tab_id) else {
            return;
        };
        if let Some(next) = tab
            .tree
            .snapshot()
            .neighbor(tab.focused.snapshot(), horizontal, forward)
        {
            self.focus_pane(next);
        }
    }

    /// Move the divider beside the focused pane by ~2 cells (48pt) in
    /// the chord's direction — the keyboard path for split resizing.
    /// The drag handle (`.gesture` on the divider) is the pointer path.
    /// `px` is the divider step in points (48 from the arrow chords,
    /// configurable through `keybind = resize_split:dir,px`).
    pub fn resize_pane_dir(&self, horizontal: bool, forward: bool, px: i32) {
        let tab_id = self.selected.snapshot();
        let Some(tab) = self.tabs.iter().find(|t| t.id == tab_id) else {
            return;
        };
        let delta = if forward { px as f32 } else { -px as f32 };
        let tree = tab.tree.snapshot();
        if tree.resize_focus(tab.focused.snapshot(), horizontal, delta) {
            tab.tree.set(tree);
        }
    }

    /// Reset all splits in the selected tab to equal shares
    /// (Ghostty `equalize_splits`).
    pub fn equalize_splits(&self) {
        let tab_id = self.selected.snapshot();
        let Some(tab) = self.tabs.iter().find(|t| t.id == tab_id) else {
            return;
        };
        let tree = tab.tree.snapshot();
        tree.equalize();
        tab.tree.set(tree);
    }

    /// Move the selected tab `dir` slots (wraps at both ends).
    pub fn move_tab(&self, dir: isize) {
        let cur = self.selected.snapshot();
        let tabs = self.tabs.snapshot();
        let Some(i) = tabs.iter().position(|t| t.id == cur) else {
            return;
        };
        if tabs.len() < 2 {
            return;
        }
        let j = (i as isize + dir).rem_euclid(tabs.len() as isize) as usize;
        if i == j {
            return;
        }
        let tab = tabs[i].clone();
        let _ = self.tabs.remove(i);
        self.tabs.insert(j, tab);
    }

    /// Drag-reorder: move tab `id` into the slot `target` occupies
    /// (Chrome/kitty semantics — the dragged tab lands on the slot it
    /// was dropped over).
    pub fn move_tab_before(&self, id: u64, target: u64) {
        if id == target {
            return;
        }
        let tabs = self.tabs.snapshot();
        let (Some(from), Some(to)) = (
            tabs.iter().position(|t| t.id == id),
            tabs.iter().position(|t| t.id == target),
        ) else {
            return;
        };
        let tab = tabs[from].clone();
        let _ = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        // The chip press that began the drag moved interaction focus off
        // the terminal surface; re-assert it on the selected tab's pane
        // (toggling the value so the watcher re-schedules a frame).
        let sel = self.selected.snapshot();
        if let Some(t) = self.tabs.iter().find(|t| t.id == sel) {
            let f = t.focused.snapshot();
            self.focus_owner.set(None);
            self.focus_owner.set(Some((sel, f)));
        }
    }

    /// Toggle pane zoom on the selected tab: the focused pane fills the
    /// whole tab; toggling again (or re-focusing then toggling) restores
    /// the split layout.
    pub fn toggle_pane_zoom(&self) {
        let tab_id = self.selected.snapshot();
        let Some(tab) = self
            .tabs.iter().find(|t| t.id == tab_id)
        else {
            return;
        };
        let focused = tab.focused.snapshot();
        tab.zoomed.with_mut(|z| *z = z.take().is_none().then_some(focused));
    }

    /// `confirm-close` gate on `close_pane`: when the pane's PTY has a
    /// program in its foreground process group, prompt via the snackbar
    /// first (Enter/“Close” confirms, Escape cancels). An idle shell
    /// (or `confirm-close = false`) closes immediately.
    pub fn try_close_pane(&self, session_id: u64) {
        use crate::config::ConfirmCloseSurface as C;
        let mode = self.config(|c| c.confirm_close);
        let session = self.session(session_id);
        let pending = session.as_ref().map(|s| s.pending_close.clone());
        let prog = session.and_then(|s| s.terminal.foreground_program());
        let prompted = match (mode, pending) {
            (C::False, _) | (_, None) => None,
            (C::True, Some(pending)) => {
                prog.map(|p| (Str::from(p), pending))
            }
            (C::Always, Some(pending)) => Some((
                prog.map(Str::from)
                    .unwrap_or_else(|| Str::from("a shell session")),
                pending,
            )),
        };
        if let Some((label, pending)) = prompted {
            // Re-arming while a prompt is already up is a no-op: a second
            // real `ctrl+shift+w` is a legitimate repeat press, and a chord
            // that straddles a window focus gain is also re-delivered by the
            // platform layer (winit FocusIn key replay — WATERUI_FEEDBACK
            // #47). Either way a duplicate `set` remounts the snackbar and
            // stacks a dead copy over the live one.
            if pending.snapshot().is_none() {
                pending.set(Some((label, false)));
            }
            return;
        }
        self.close_pane(session_id);
    }

    /// `confirm-close` gate on `close_tab` — any busy leaf prompts on
    /// the focused pane's snackbar; confirming closes the whole tab.
    pub fn try_close_tab(&self, tab_id: u64) {
        use crate::config::ConfirmCloseSurface as C;
        let mode = self.config(|c| c.confirm_close);
        let prompted = if mode == C::False {
            None
        } else if let Some(tab) = self.tabs.iter().find(|t| t.id == tab_id) {
            let sessions = self.sessions.borrow();
            let busy = tab
                .tree
                .snapshot()
                .leaves()
                .iter()
                .filter_map(|sid| sessions.iter().find(|s| s.id == *sid))
                .find_map(|s| s.terminal.foreground_program());
            let focus = tab.focused.snapshot();
            let pending = sessions
                .iter()
                .find(|s| s.id == focus)
                .map(|s| s.pending_close.clone());
            match mode {
                C::Always => pending.map(|pending| {
                    (
                        busy.map_or_else(|| Str::from("this tab"), Str::from),
                        Some(pending),
                    )
                }),
                C::True => busy.map(|prog| (Str::from(prog), pending)),
                C::False => None,
            }
        } else {
            None
        };
        if let Some((label, Some(pending))) = prompted {
            if pending.snapshot().is_none() {
                pending.set(Some((label, true)));
            }
            return;
        }
        self.close_tab(tab_id);
    }

    /// The user approved the `confirm-close` snackbar (Enter or its
    /// “Close” button) — performs the stashed pane/tab close.
    pub fn confirm_close(&self, session_id: u64) {
        let decision = {
            let sessions = self.sessions.borrow();
            sessions
                .iter()
                .find(|s| s.id == session_id)
                .map(|s| s.pending_close.snapshot().map(|(_, whole)| whole))
        };
        let Some(whole_tab) = decision.flatten() else { return };
        self.cancel_close_prompt(session_id);
        if whole_tab {
            if let Some(tab_id) = self.session_tab.lock().unwrap().get(&session_id).copied() {
                self.close_tab(tab_id);
            }
        } else {
            self.close_pane(session_id);
        }
    }

    /// Clear a `confirm-close` prompt without closing (Escape).
    pub fn cancel_close_prompt(&self, session_id: u64) {
        let sessions = self.sessions.borrow();
        if let Some(s) = sessions.iter().find(|s| s.id == session_id) {
            s.pending_close.set(None);
            if let Some(manager) = s.snackbar.borrow().as_ref() {
                manager.dismiss();
            }
        }
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
        match tab.tree.snapshot().remove(session_id) {
            Some(new_tree) => {
                // Focus a remaining leaf when the closed pane had focus.
                if tab.focused.snapshot() == session_id
                    && let Some(next) = new_tree.leaves().first()
                {
                    self.focus_pane(*next);
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
        let leaves = tab.tree.snapshot().leaves();
        for sid in &leaves {
            self.kill_session(*sid);
            self.session_tab.lock().unwrap().remove(sid);
        }
        let tabs = self.tabs.snapshot();
        if let Some(pos) = tabs.iter().position(|t| t.id == tab_id) {
            let _ = self.tabs.remove(pos);
            self.tab_count.set(self.tabs.len());
            if self.selected.snapshot() == tab_id {
                let remaining = self.tabs.snapshot();
                let idx = pos.min(remaining.len().saturating_sub(1));
                if let Some(next) = remaining.as_slice().get(idx) {
                    self.selected.set(next.id);
                }
            }
        }
        // `quit-after-last-window-closed` (default on, Ghostty/Linux):
        // the last tab is gone, so is every session — exit.
        if self.tabs.is_empty()
            && self.config(|c| c.quit_after_last_window_closed)
        {
            self.quit();
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

    /// Jump back to the previously-selected tab (kitty `goto_tab -1` /
    /// tmux `last-window`). No-op until a second tab was selected once.
    pub fn select_last_tab(&self) {
        let prev = self.last_tab_id.get();
        if prev != 0 && self.tabs.iter().any(|t| t.id == prev) {
            self.selected.set(prev);
        }
    }

    /// `close_window`: close every tab in this window (each goes through
    /// `confirm-close` where configured). Empty tab list quits via
    /// `quit-after-last-window-closed`.
    pub fn close_window(&self) {
        let ids: Vec<u64> = self.tabs.iter().map(|t| t.id).collect();
        for id in ids {
            self.try_close_tab(id);
        }
    }

    /// Ghostty `close_all_tabs` — same sweep as `close_window`.
    pub fn close_all_tabs(&self) {
        self.close_window();
    }

    /// Ghostty `close_other_tabs` — close every tab but the selected
    /// one, `confirm-close` prompts where configured.
    pub fn close_other_tabs(&self) {
        let sel = self.selected.snapshot();
        let ids: Vec<u64> = self
            .tabs
            .iter()
            .filter(|t| t.id != sel)
            .map(|t| t.id)
            .collect();
        for id in ids {
            self.try_close_tab(id);
        }
    }

    /// `toggle_tab_bar`: force the strip on/off until the next toggle;
    /// `tab-bar-min-tabs` governs again once the override is unset
    /// (the toggle flips relative to the strip's current visibility).
    pub fn toggle_tab_bar(&self) {
        let cur = self
            .tab_bar_forced
            .snapshot()
            .unwrap_or_else(|| self.tab_count.snapshot() >= self.tab_bar_min.snapshot());
        self.tab_bar_forced.set(Some(!cur));
    }

    /// Cycle tabs by `dir` (+1/-1).
    pub fn cycle_tab(&self, dir: isize) {
        let tabs = self.tabs.snapshot();
        if tabs.is_empty() {
            return;
        }
        let cur = self.selected.snapshot();
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
    /// The owning tab's id and its focused-pane record — `.focused` on
    /// the surface reads `AppState::focus_owner`, the dim compares this.
    tab_id: u64,
    focused: Binding<u64>,
}

impl View for PaneLeaf {
    fn body(self, env: &Environment) -> impl View {
        let menu_enabled = self.session.context_menu_enabled.clone();
        let env = env.clone();
        // `right-click-action = context-menu` — the pane's `.context_menu`
        // item list collapses to empty while the program reports mouse
        // input (a secondary click is program input under DECSET
        // 1000/1002/1006). Built outside `assemble` so the `when` below
        // can attach the modifier on the menu branch — the zstack isn't
        // Clone, so the attach wraps the branch's returned AnyView.
        let reporting = self.session.mouse_reporting.clone();
        let menu = zip(reporting, menu_enabled.clone())
            .map(|(reporting, menu_enabled)| -> Vec<MenuItem> {
                if reporting || !menu_enabled {
                    Vec::new()
                } else {
                    vec![
                        "Copy".action(|s: PaneSession| s.push_action(TermAction::Copy)).into(),
                        "Paste".action(|s: PaneSession| s.push_action(TermAction::Paste)).into(),
                        "Select All".action(|s: PaneSession| s.push_action(TermAction::SelectAll)).into(),
                        "Clear".action(|s: PaneSession| s.push_action(TermAction::ClearScrollback)).into(),
                        "Search".action(|s: PaneSession| s.push_action(TermAction::Search)).into(),
                    ]
                }
            })
            .computed();
        let session_state = PaneSession(self.session.clone());
        let assemble = std::rc::Rc::new(move || -> AnyView {
        // Search bar: a real WaterUI row that appears above the surface —
        // the field is a sibling, so toggling it never remounts the
        // SceneView or drops its keyboard focus.
        let query = self.session.search_query.clone();
        let status = self.session.search_status.clone();
        let open = self.session.search_open.clone();
        let term_surface = TermSurface::new(
            self.session.clone(),
            self.state.clone(),
            self.state.palette.clone(),
            FontCollection::from_env(&env),
        );
        // Reactive IBeam/pointing-hand over Ctrl-hovered links.
        let hover_cursor = term_surface.hover_cursor.clone();
        // `.focused` is hydrolysis's programmatic embedded-focus grant
        // (#132): when `focus_owner == (this tab, this pane)` the surface
        // takes key focus without a click — at launch, after a tab switch,
        // and across split rebuilds. Pointer focus writes back into
        // `focus_owner`; the `on_change` watchers in `tabs_view` keep it
        // and `tab.focused` in sync. Hidden tabs' panes never match the
        // owner, so only the visible pane can hold embedded focus.
        let surface = SceneView::new(term_surface)
            .focused(&self.state.focus_owner, (self.tab_id, self.session.id))
            .cursor(hover_cursor)
            // Drag-and-drop: a file dropped on the pane pastes its
            // shell-quoted path into the PTY (Ghostty/kitty behaviour).
            .drop_destination(|session: PaneSession, data: DragData| {
                let text = data.as_str();
                if !text.starts_with(TAB_DRAG_PREFIX) {
                    session.push_action(TermAction::DropText(text.to_string()));
                }
            });
        let surface = Frame::new(surface);
        // Paste-protection confirm: multi-line clipboard content waits in
        // `pending_paste` for an explicit Paste/Cancel (or Enter/Escape).
        let pending = self.session.pending_paste.clone();
        let session = PaneSession(self.session.clone()); // `.state` stores a clone
        // Paste-protection confirmation rides the framework's own snackbar
        // overlay (mounted by `Window::new`), so it layers above the pane
        // correctly. The `when` gate mounts a zero-size trigger whose
        // `on_appear` presents the Snackbar; `pending_paste` still gates
        // keystrokes (Enter = Paste, Escape = Cancel) on the surface side.
        let paste_overlay = when(
            pending.is_some(),
            move || {
                let preview: Str = pending
                    .snapshot()
                    .map(|t| {
                        let lines = t.lines().count();
                        let first: String = t.lines().next().unwrap_or_default().chars().take(60).collect();
                        Str::from(format!("Paste {lines} lines? {first}…"))
                    })
                    .unwrap_or_else(|| Str::from("Paste?"));
                Spacer::new(0.0).on_appear(move |manager: SnackbarManager, s: PaneSession| {
                    *s.0.snackbar.borrow_mut() = Some(manager.clone());
                    manager.show(
                        Snackbar::new(preview)
                            .action("Paste", |s: PaneSession| s.push_action(TermAction::PasteConfirm))
                            .duration(Duration::ZERO)
                            .state(&PaneSession(s.0.clone())),
                    );
                })
            },
        )
        .anyview();
        // `confirm-close`: closing a pane/tab whose PTY runs a program
        // asks via the snackbar — “Close”/Enter confirms, Escape cancels
        // (the snackbar's single action slot; the surface gate does Esc).
        // `clipboard-read = ask`: an OSC 52 read waits on Allow / Deny
        // (or Enter / Escape on the surface's key gate). Ghostty asks
        // with both actions visible; the framework's Snackbar has a
        // single action slot and no two-choice transient primitive at
        // this pin (WATERUI_FEEDBACK #30), so the prompt is a composed
        // card — Allow is the filled primary (`BorderedProminent`), Deny
        // the lower-emphasis `Bordered` secondary (not the snackbar's
        // close ✕, which would silently deny).
        let pending_clip = session.0.pending_clipboard_read.clone();
        let clip_overlay = vstack((
            Spacer::flexible(),
            when(pending_clip, move || {
                Card::new(
                    vstack((
                        text("Program wants to read the clipboard"),
                        hstack((
                            button("Deny")
                                .bordered()
                                .action(|s: PaneSession| {
                                    s.push_action(TermAction::ClipboardReadDeny)
                                }),
                            button("Allow")
                                .bordered_prominent()
                                .action(|s: PaneSession| {
                                    s.push_action(TermAction::ClipboardReadConfirm)
                                }),
                        ))
                        .spacing(8.0),
                    ))
                    .spacing(8.0),
                )
                .style(CardStyle::Elevated)
            })
            .padding_with(16.0),
        ))
        .anyview();
        let pending_close = session.0.pending_close.clone();
        let close_overlay = when(
            pending_close.is_some(),
            move || {
                let label: Str = pending_close
                    .snapshot()
                    .map(|(prog, whole)| {
                        let scope = match whole {
                            true => "Close tab? ",
                            false => "Close? ",
                        };
                        Str::from(format!("{scope}{prog} is still running"))
                    })
                    .unwrap_or_else(|| Str::from("Close?"));
                Spacer::new(0.0).on_appear(move |manager: SnackbarManager, s: PaneSession| {
                    *s.0.snackbar.borrow_mut() = Some(manager.clone());
                    manager.show(
                        Snackbar::new(label)
                            .action("Close", |s: PaneSession| s.push_action(TermAction::CloseConfirm))
                            .duration(Duration::ZERO)
                            .state(&PaneSession(s.0.clone())),
                    );
                })
            },
        )
        .anyview();
        let bar = when(open, move || {
            hstack((
                // Hit-transparent: a clicked-in field would take real key
                // focus and swallow Escape/Enter/arrows before the surface
                // sees them (no bubbling — WATERUI_FEEDBACK #50). The
                // surface owns all editing through `search_query`.
                field("find in buffer", &query).hittable(false),
                text(status.clone()).muted(),
                text("\u{2191}").on_tap(|s: PaneSession| s.push_action(TermAction::NavigateSearch(-1))),
                text("\u{2193}").on_tap(|s: PaneSession| s.push_action(TermAction::NavigateSearch(1))),
            ))
            .spacing(6.0)
            .padding_horizontal(8.0)
            .padding_vertical(4.0)
            // The bar is part of the pane's keyboard world: a click
            // anywhere on it returns embedded focus to the surface (the
            // field itself is hit-transparent, WATERUI_FEEDBACK #50).
            .on_tap(|app: AppState, s: PaneSession| app.refocus(s.0.id))
        })
        .anyview();
        // `unfocused-split-opacity`: the focused pane stays opaque, every
        // other leaf in this tab fades to the configured alpha — a
        // signal-driven dim like Ghostty's.
        let session_id = session.0.id;
        let pane_alpha = zip(
            self.focused.equal_to(session_id),
            session.0.unfocused_opacity.clone(),
        )
        .map(|(is_focused, unfocused)| if is_focused { 1.0 } else { unfocused });
        // `resize-overlay`: a cols×rows chip at the pane's bottom edge
        // while the terminal resizes; `resize_label` is Some only in
        // the display window. A Spacer above the `when` pushes the chip
        // to the pane's bottom edge.
        let resize_label = session.0.resize_label.clone();
        let show_resize = resize_label.is_some();
        let resize_badge = vstack((
            Spacer::flexible(),
            when(show_resize, move || {
                text(resize_label.unwrap_or_default().computed())
                    .foreground(Foreground)
                    .padding_horizontal(10.0)
                    .padding_vertical(4.0)
                    .background(Surface)
            })
            .padding_vertical(8.0),
        ));
        // `prompt_title` — a rename prompt over the pane. The TextField
        // can't take keyboard focus (hydrolysis#90), so the surface
        // routes keys + text into `title_query` and the field mirrors it.
        let title_prompt_open = session.0.title_prompt_open.clone();
        let title_query = session.0.title_query.clone();
        let title_prompt = vstack((
            Spacer::flexible(),
            when(title_prompt_open, move || {
                Card::new(vstack((
                    text("Rename tab title").muted(),
                    // Same hit-transparent rule as the search field
                    // (WATERUI_FEEDBACK #50).
                    field("tab title", &title_query).hittable(false),
                    text("Enter: rename · Esc: cancel").muted(),
                ))
                .spacing(8.0))
                .style(CardStyle::Elevated)
                .padding_with(24.0)
            }),
            Spacer::flexible(),
        ))
        // Click on the prompt overlay returns keys to the pane (the field
        // is hit-transparent — WATERUI_FEEDBACK #50).
        .on_tap(|app: AppState, s: PaneSession| app.refocus(s.0.id));
        // `inspector` — a bottom-edge chip reporting the attributes of
        // the cell under the terminal cursor; `inspector_label` is
        // rewritten every rendered frame while the inspector is open.
        let inspector_open = session.0.inspector_open.clone();
        let inspector_label = session.0.inspector_label.clone();
        let inspector_badge = vstack((
            Spacer::flexible(),
            when(inspector_open, move || {
                text(inspector_label.computed())
                    .foreground(Foreground)
                    .padding_horizontal(10.0)
                    .padding_vertical(4.0)
                    .background(Surface)
            })
            .padding_vertical(8.0),
        ));
        let stack = zstack((
            vstack((bar, surface)).spacing(0.0).opacity(pane_alpha),
            paste_overlay,
            close_overlay,
            clip_overlay,
            resize_badge,
            title_prompt,
            inspector_badge,
        ));
        stack.state(&session).anyview()
        });
        // `right-click-action`: only `context-menu` attaches the framework
        // modifier — an attached modifier registers a menu target that
        // claims every secondary click (debug builds also mount the
        // inspect item even over an empty item list), so copy / paste /
        // ignore must carry no target at all and let the scene's own
        // Secondary arm handle the button.
        let assemble_menu = assemble.clone();
        let menu_items = menu.clone();
        when(menu_enabled, move || {
            assemble_menu()
                .context_menu(menu_items.clone())
                .state(&session_state)
        })
        .otherwise(move || assemble())
    }
}

/// Render one pane node as WaterUI views.
fn pane_view(node: &SplitNode, focused: &Binding<u64>, tab_id: u64, state: &AppState) -> AnyView {
    match node {
        SplitNode::Leaf(sid) => {
            let session = state.session(*sid);
            match session {
                Some(session) => PaneLeaf {
                    session,
                    state: state.clone(),
                    tab_id,
                    focused: focused.clone(),
                }
                .anyview(),
                None => text("pane closed").anyview(),
            }
        }
        SplitNode::Split {
            dir,
            children,
            sizes,
        } => {
            let k = children.len();
            // Seed once from the measured layout so children keep the sizes
            // the stack just gave them; later the divider drag owns it.
            if sizes.snapshot().len() != k {
                let seeded: Vec<f32> = children
                    .iter()
                    .map(|c| subtree_px(c, state, *dir))
                    .collect();
                if seeded.iter().all(|s| *s > MIN_PANE_PX / 2.0) {
                    sizes.set(seeded);
                }
            }
            let sized = sizes.snapshot().len() == k && sizes.snapshot().iter().all(|s| *s > 0.0);
            let mut views: Vec<AnyView> = Vec::with_capacity(2 * k - 1);
            for (j, child) in children.iter().enumerate() {
                if j > 0 {
                    views.push(
                        divider_handle(*dir, j, children, sizes, state.clone()).anyview(),
                    );
                }
                let framed = when(
                    sized,
                    {
                        let sz = sizes.clone();
                        let extent = sz
                            .map(move |v: Vec<f32>| v.as_slice().get(j).copied().unwrap_or(0.0));
                        let child = child.clone();
                        let foc = focused.clone();
                        let st = state.clone();
                        let d = *dir;
                        move || {
                            let child_view = pane_view(&child, &foc, tab_id, &st);
                            let ext = extent.clone();
                            let frame = Frame::new(child_view);
                            match d {
                                SplitDir::Row => frame.width(ext),
                                SplitDir::Column => frame.height(ext),
                            }
                        }
                    },
                )
                .otherwise({
                    let child = child.clone();
                    let foc = focused.clone();
                    let st = state.clone();
                    // No measured extent yet — let the stack share space
                    // equally until the seed lands.
                    move || pane_view(&child, &foc, tab_id, &st)
                });
                views.push(framed.anyview());
            }
            // Vec<AnyView> collects straight into a stack — no ForEach ids.
            match dir {
                SplitDir::Row => views.into_iter().collect::<HStack<_>>().spacing(0.0).anyview(),
                SplitDir::Column => views.into_iter().collect::<VStack<_>>().spacing(0.0).anyview(),
            }
        }
    }
}

/// Smallest pane extent the divider drag honors, in points.
const MIN_PANE_PX: f32 = 48.0;
/// Main-axis points one divider claims from its split (1pt line + 2×3pt
/// grab padding).
const DIVIDER_PX: f32 = 7.0;

/// The main-axis extent `node` actually rendered last frame, in points —
/// measured leaf rects rolled up the split tree. `along` is the axis of
/// the split that contains `node` (its parent's direction).
fn subtree_px(node: &SplitNode, state: &AppState, along: SplitDir) -> f32 {
    match node {
        SplitNode::Leaf(id) => {
            let px = state
                .session(*id)
                .map(|s| s.pane_px.snapshot())
                .unwrap_or_default();
            match along {
                SplitDir::Row => px.0,
                SplitDir::Column => px.1,
            }
        }
        SplitNode::Split {
            dir,
            children,
            sizes: _,
        } if dir == &along => {
            children
                .iter()
                .map(|c| subtree_px(c, state, along))
                .sum::<f32>()
                + DIVIDER_PX * (children.len().saturating_sub(1) as f32)
        }
        SplitNode::Split { children, .. } => children
            .first()
            .map(|c| subtree_px(c, state, along))
            .unwrap_or_default(),
    }
}

/// A 1pt theme-Border line padded to a 7pt grab zone that drags the two
/// panes it separates — `children[j-1]` against `children[j]`.
fn divider_handle(
    dir: SplitDir,
    j: usize,
    children: &[SplitNode],
    sizes: &Binding<Vec<f32>>,
    state: AppState,
) -> impl View {
    use waterui::cursor::CursorStyle;
    use waterui::gesture::{DragEvent, DragGesture, GesturePhase};
    use waterui_core::extract::{State, Use};

    let left = children[j - 1].clone();
    let right = children[j].clone();
    // (left, right) extents measured when the drag begins — re-seeded so a
    // stale `sizes` (e.g. after a window resize) can't make the panes jump.
    let grab = Binding::<Option<(f32, f32)>>::default();
    let sizes = sizes.clone();
    let app = state.clone();
    let cursor = match dir {
        SplitDir::Row => CursorStyle::ResizeLeftRight,
        SplitDir::Column => CursorStyle::ResizeUpDown,
    };
    // Baseline: a 1pt `Border` token line centred in the 7pt grab zone —
    // the M3 `Divider` primitive draws `outline_variant`, which is nearly
    // invisible on light themes; the `Border` token (`outline`) is the
    // theme's designated visible separator and resolves per-flush, so theme
    // reloads restyle it. `split-divider-color` swaps the token line for a
    // flat custom-colour fill covering the whole grab zone.
    let custom = app.config(|c| c.split_divider_color);
    let handle = match (dir, custom) {
        (SplitDir::Row, Some(c)) => Frame::new(Color::srgb(c.r, c.g, c.b))
            .width(DIVIDER_PX)
            .max_height(f32::INFINITY)
            .anyview(),
        (SplitDir::Row, None) => Frame::new(hstack((
            Spacer::flexible(),
            Frame::new(Color::new(Border))
                .width(1.0)
                .max_height(f32::INFINITY),
            Spacer::flexible(),
        )))
        .width(DIVIDER_PX)
        .max_height(f32::INFINITY)
        .anyview(),
        (SplitDir::Column, Some(c)) => Frame::new(Color::srgb(c.r, c.g, c.b))
            .height(DIVIDER_PX)
            .max_width(f32::INFINITY)
            .anyview(),
        (SplitDir::Column, None) => Frame::new(vstack((
            Spacer::flexible(),
            Frame::new(Color::new(Border))
                .height(1.0)
                .max_width(f32::INFINITY),
            Spacer::flexible(),
        )))
        .height(DIVIDER_PX)
        .max_width(f32::INFINITY)
        .anyview(),
    };
    handle
        .cursor(cursor)
        .gesture(
            DragGesture::new(0.0),
            move |event: Option<Use<DragEvent>>,
                  State(sizes): State<Binding<Vec<f32>>>,
                  State(grab): State<Binding<Option<(f32, f32)>>>| {
                let present = event.is_some();
                let Some(event) = event.map(|e| e.0) else { return };
                tracing::debug!(?dir, present, phase = ?event.phase, t = ?event.translation, "divider");
                match event.phase {
                    GesturePhase::Started => {
                        // A press just inside a pane's edge starts a drag
                        // selection that this gesture then steals — the
                        // drag owns the pointer now, so drop the
                        // half-formed highlight on both sides.
                        for leaf in left.leaves().into_iter().chain(right.leaves()) {
                            if let Some(s) = app.session(leaf) {
                                let had = s.terminal.term.lock().selection.take().is_some();
                                tracing::debug!(leaf, had, "divider clears selection");
                            }
                        }
                        grab.set(Some((
                            subtree_px(&left, &app, dir),
                            subtree_px(&right, &app, dir),
                        )));
                    }
                    GesturePhase::Updated => {
                        let Some((l, r)) = grab.snapshot() else { return };
                        let delta = match dir {
                            SplitDir::Row => event.translation.x,
                            SplitDir::Column => event.translation.y,
                        };
                        // Clamp at the smaller pane's minimum: keep the
                        // pair's total constant so neighbours don't shift.
                        let clamped = delta.clamp(MIN_PANE_PX - l, r - MIN_PANE_PX);
                        let mut v = sizes.snapshot();
                        if j < v.len() {
                            v[j - 1] = l + clamped;
                            v[j] = r - clamped;
                            sizes.set(v);
                        }
                    }
                    GesturePhase::Ended | GesturePhase::Cancelled => {
                        grab.set(None);
                    }
                }
            },
        )
        .state(&sizes)
        .state(&grab)
}

/// Root of every hydroterm window: `body` runs inside the environment, so
/// it captures `env` for `AppState::new_window` before rendering the tabs.
struct AppRoot {
    state: AppState,
}

impl View for AppRoot {
    fn body(self, env: &Environment) -> impl View {
        let _ = self.state.env.set(env.clone());
        // `window-save-state`: persist the live frame whenever it changes.
        // hydrolysis writes real Moved/Resize geometry back into this
        // binding, so a 1s poll sees every user resize/move.
        if self.state.config(|c| c.window_save_state)
            && let Some(frame) = self.state.window_frame.borrow().clone()
        {
            spawn_local(async move {
                let mut last = Rect::new(Point::new(f32::NAN, f32::NAN), Size::zero());
                loop {
                    sleep(std::time::Duration::from_millis(500)).await;
                    let f = frame.snapshot();
                    if f != last {
                        last = f;
                        save_window_state(f);
                    }
                }
            })
            .detach();
        }
        tabs_view(self.state)
    }
}

/// `~/.config/hydroterm/window-state` — sibling of the config file,
/// `x y w h` in points on one line.
fn window_state_path() -> std::path::PathBuf {
    crate::config::default_path()
        .parent()
        .map(|d| d.join("window-state"))
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp/hydroterm-window-state"))
}

/// Read a persisted window frame; anything malformed yields `None`.
pub fn load_window_state() -> Option<Rect> {
    let text = std::fs::read_to_string(window_state_path()).ok()?;
    let mut it = text.split_whitespace().map(|t| t.parse::<f32>().ok());
    let (x, y, w, h) = (it.next()??, it.next()??, it.next()??, it.next()??);
    (w >= 100.0 && h >= 100.0)
        .then(|| Rect::new(Point::new(x, y), Size::new(w, h)))
}

fn save_window_state(frame: Rect) {
    let path = window_state_path();
    let o = frame.origin();
    let s = frame.size();
    let _ = std::fs::write(
        path,
        format!("{} {} {} {}\n", o.x, o.y, s.width, s.height),
    );
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
        let tab_focused = tab.focused.clone();
        let tab_id = tab.id;
        move |z: Option<u64>| {
            if let Some(z) = z.filter(|z| app.session(*z).is_some()) {
                pane_view(&SplitNode::Leaf(z), &tab_focused, tab_id, &app)
            } else {
                watch(tree.clone(), {
                    let app = app.clone();
                    let tab_focused = tab_focused.clone();
                    move |node: SplitNode| pane_view(&node, &tab_focused, tab_id, &app)
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
/// `AppState` is injected once at the root (`.state(&state)`); every
/// handler below takes it back as an `AppState` extractor parameter —
/// the natural shape now that missing injections are a fail-fast panic,
/// not something to route around.
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

    // `tab-bar-min-tabs`: the strip is unmounted (space reclaimed) until
    // the tab count reaches the configured floor — `visible(false)` would
    // leave an empty band, so the whole bar sits behind `when`. A
    // `toggle_tab_bar` override wins over the count rule.
    let show_strip = zip(
        zip(state.tab_count.clone(), state.tab_bar_min.clone()),
        state.tab_bar_forced.clone(),
    )
    .map(|((count, min), forced)| forced.unwrap_or(count >= min));
    let strip_bar = when(show_strip, {
        let state = state.clone();
        move || {
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
                    vstack((
                        hstack((
                            // `tab-activity` dot: parser output landed while
                            // the tab was not selected (kitty
                            // `tab_activity_symbol`).
                            when(tab.activity.clone(), || {
                                text("●").foreground(Accent)
                            }),
                            // `bell-features` `attention` indicator.
                            when(tab.badge.clone(), || text("🔔")),
                            text(tab.title.clone()).foreground(label_color),
                            text("×")
                                .muted()
                                .padding_with([3.0, 0.0, 4.0, 4.0])
                                .a11y_label("Close tab")
                                .a11y_role(AccessibilityRole::Button)
                                .on_tap(move |app: AppState| app.try_close_tab(tab_id)),
                        ))
                        .padding_with([4.0, 0.0, 8.0, 4.0]),
                        Frame::new(indicator_color).height(3.0),
                    ))
                    .spacing(0.0)
                    .height(TAB_STRIP_HEIGHT)
                    .on_tap(move |app: AppState| app.selected.set(tab_id))
                    .a11y_role(AccessibilityRole::Tab)
                    .a11y_state_signal(
                        active.map(|a| AccessibilityState::new().selected(a)),
                    )
                    // Drag-to-reorder: the chip carries its tab id as
                    // text payload; every sibling chip is a drop slot.
                    .draggable(drag_drop::DragData::text(format!(
                        "{TAB_DRAG_PREFIX}{tab_id}"
                    )))
                    .drop_destination(move |app: AppState, data: drag_drop::DragData| {
                        if let Some(id) = data
                            .as_str()
                            .strip_prefix(TAB_DRAG_PREFIX)
                            .and_then(|s| s.parse::<u64>().ok())
                        {
                            app.move_tab_before(id, tab_id);
                        }
                    })
                })
            };
            hstack((
                strip.a11y_role(AccessibilityRole::TabList),
                text("+")
                    .muted()
                    .padding_horizontal(4.0)
                    .height(TAB_STRIP_HEIGHT)
                    .a11y_label("New tab")
                    .a11y_role(AccessibilityRole::Button)
                    .on_tap(|app: AppState| _ = app.new_tab()),
            ))
            .spacing(4.0)
            .padding()
        }
    });

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
        vstack((strip_bar, content)).spacing(0.0).leading(),
        palette_overlay,
        settings_overlay,
        quick,
    ))
    // Tab switch → grant embedded focus to that tab's remembered pane;
    // also track the previously-selected tab for `last_tab`.
    .on_change(&state.selected, {
        let app = state.clone();
        let prev_seen = Rc::new(std::cell::Cell::new(0u64));
        move |sel: u64| {
            let prev = prev_seen.get();
            if prev != sel {
                app.last_tab_id.set(prev);
                prev_seen.set(sel);
            }
            if let Some(t) = app.tabs.iter().find(|t| t.id == sel) {
                t.activity.set(false);
                app.focus_owner.set(Some((sel, t.focused.snapshot())));
            }
        }
    })
    // Pointer/programmatic focus write-back → keep the per-tab focus
    // record (unfocused dim, title sync) following the real owner. Only
    // the selected tab can ever appear here, so a hidden tab's pane can
    // never be recorded as focused.
    .on_change(&state.focus_owner, {
        let app = state.clone();
        move |o: Option<(u64, u64)>| {
            let Some((tab_id, session_id)) = o else { return };
            let Some(t) = app.tabs.iter().find(|t| t.id == tab_id) else {
                return;
            };
            if t.focused.snapshot() != session_id {
                t.focused.set(session_id);
                if t.title_override.snapshot().is_none()
                    && let Some(s) = app.session(session_id)
                {
                    t.title.set(s.title.snapshot());
                }
            }
        }
    })
    .state(&state)
}

/// The geometry pair `quick_terminal` snapshots at spawn.
type QuickGeo = (
    crate::config::QuickTermPosition,
    Option<(
        crate::config::QuickTermSize,
        Option<crate::config::QuickTermSize>,
    )>,
);

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
    PaletteItem { name: "Reload Config", chord: "ctrl+shift+,", action: TermAction::ReloadConfig },
    PaletteItem { name: "Close Pane / Tab", chord: "ctrl+shift+w", action: TermAction::CloseTab },
    PaletteItem { name: "Split Right", chord: "ctrl+shift+e", action: TermAction::SplitRight },
    PaletteItem { name: "Split Down", chord: "ctrl+shift+d", action: TermAction::SplitDown },
    PaletteItem { name: "Toggle Pane Zoom", chord: "ctrl+shift+z", action: TermAction::PaneZoom },
    PaletteItem { name: "Equalize Splits", chord: "", action: TermAction::EqualizeSplits },
    PaletteItem { name: "Focus Next Pane", chord: "ctrl+shift+]", action: TermAction::FocusNextPane },
    PaletteItem { name: "Focus Previous Pane", chord: "ctrl+shift+[", action: TermAction::FocusPrevPane },
    PaletteItem { name: "Copy", chord: "ctrl+shift+c", action: TermAction::Copy },
    PaletteItem { name: "Paste", chord: "ctrl+shift+v", action: TermAction::Paste },
    PaletteItem { name: "Select All", chord: "ctrl+shift+a", action: TermAction::SelectAll },
    PaletteItem { name: "Find in Buffer", chord: "ctrl+shift+f", action: TermAction::Search },
    PaletteItem { name: "Settings", chord: "ctrl+shift+,", action: TermAction::Settings },
    PaletteItem { name: "Clear Scrollback", chord: "ctrl+shift+k", action: TermAction::ClearScrollback },
    PaletteItem { name: "Clear Screen", chord: "ctrl+shift+l", action: TermAction::ClearScreen },
    PaletteItem { name: "Reset Terminal", chord: "", action: TermAction::Reset },
    PaletteItem { name: "Write Screen to File", chord: "", action: TermAction::WriteScreenFile(crate::keys::FileSink::Open) },
    PaletteItem { name: "Write Scrollback to File", chord: "", action: TermAction::WriteScrollbackFile(crate::keys::FileSink::Open) },
    PaletteItem { name: "Write Selection to File", chord: "", action: TermAction::WriteSelectionFile(crate::keys::FileSink::Open) },
    PaletteItem { name: "Write Last Output to File", chord: "", action: TermAction::WriteLastOutputFile(crate::keys::FileSink::Open) },
    PaletteItem { name: "Scroll to Selection", chord: "", action: TermAction::ScrollToSelection },
    PaletteItem { name: "Clear Selection", chord: "", action: TermAction::ClearSelection },
    PaletteItem { name: "Open Config", chord: "", action: TermAction::OpenConfig },
    PaletteItem { name: "Increase Font Size", chord: "ctrl+shift+=", action: TermAction::IncreaseFontSize(1) },
    PaletteItem { name: "Decrease Font Size", chord: "ctrl+shift+-", action: TermAction::DecreaseFontSize(1) },
    PaletteItem { name: "Reset Font Size", chord: "ctrl+shift+0", action: TermAction::FontReset },
    PaletteItem { name: "Jump to Previous Prompt", chord: "ctrl+shift+up", action: TermAction::JumpToPrompt(-1) },
    PaletteItem { name: "Jump to Next Prompt", chord: "ctrl+shift+down", action: TermAction::JumpToPrompt(1) },
    PaletteItem { name: "Scroll to Top", chord: "ctrl+shift+home", action: TermAction::ScrollToTop },
    PaletteItem { name: "Scroll to Bottom", chord: "ctrl+shift+end", action: TermAction::ScrollToBottom },
    PaletteItem { name: "Scroll Page Up", chord: "shift+pageup", action: TermAction::ScrollPageUp },
    PaletteItem { name: "Scroll Page Down", chord: "shift+pagedown", action: TermAction::ScrollPageDown },
    PaletteItem { name: "Scroll Line Up", chord: "shift+up", action: TermAction::ScrollPageLines(-1) },
    PaletteItem { name: "Scroll Line Down", chord: "shift+down", action: TermAction::ScrollPageLines(1) },
    PaletteItem { name: "Move Tab Left", chord: "ctrl+shift+pageup", action: TermAction::MoveTab(-1) },
    PaletteItem { name: "Move Tab Right", chord: "ctrl+shift+pagedown", action: TermAction::MoveTab(1) },
    PaletteItem { name: "Focus Pane Left", chord: "ctrl+shift+alt+left", action: TermAction::FocusPaneDir { horizontal: true, forward: false } },
    PaletteItem { name: "Focus Pane Right", chord: "ctrl+shift+alt+right", action: TermAction::FocusPaneDir { horizontal: true, forward: true } },
    PaletteItem { name: "Focus Pane Up", chord: "ctrl+shift+alt+up", action: TermAction::FocusPaneDir { horizontal: false, forward: false } },
    PaletteItem { name: "Focus Pane Down", chord: "ctrl+shift+alt+down", action: TermAction::FocusPaneDir { horizontal: false, forward: true } },
    PaletteItem { name: "URL Hints (open link by number)", chord: "ctrl+shift+u", action: TermAction::UrlHints },
    PaletteItem { name: "Copy Last Command Output", chord: "ctrl+shift+o", action: TermAction::CopyLastOutput },
    PaletteItem { name: "Next Tab", chord: "ctrl+tab", action: TermAction::NextTab },
    PaletteItem { name: "Previous Tab", chord: "ctrl+shift+tab", action: TermAction::PrevTab },
    PaletteItem { name: "Last Tab (previously selected)", chord: "", action: TermAction::LastTab },
    PaletteItem { name: "Close Window (all tabs)", chord: "", action: TermAction::CloseWindow },
    PaletteItem { name: "Close All Tabs", chord: "", action: TermAction::CloseAllTabs },
    PaletteItem { name: "Close Other Tabs", chord: "", action: TermAction::CloseOtherTabs },
    PaletteItem { name: "Toggle Tab Bar", chord: "", action: TermAction::ToggleTabBar },
    PaletteItem { name: "Start Selection (keyboard select)", chord: "", action: TermAction::StartSelection },
    PaletteItem { name: "Toggle Fullscreen", chord: "f11", action: TermAction::Fullscreen },
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
        // A light/dark pair has no picker row — show `auto`, the
        // nearest semantic (it also follows the desktop scheme).
        crate::config::ThemeRef::Pair { .. } => 0,
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
        let next = !self.palette_open.snapshot();
        if next {
            self.palette_query.set_from("");
            self.palette_sel.set(Some(0));
            self.palette_scroll.scroll_to(0);
        }
        self.palette_open.set(next);
    }

    /// Run the `i`-th match of the current query (Up/Down selection or
    /// a row tap).
    pub fn run_palette_at(&self, i: usize) {
        let q = self.palette_query.snapshot().to_string();
        let matches = palette_matches(&q);
        let Some(item) = matches.as_slice().get(i) else {
            self.palette_open.set(false);
            return;
        };
        self.run_palette_action(item.action.clone());
    }

    /// Open/close the settings page (Ctrl+Shift+,). Opening snapshots
    /// the live config into the edit bindings.
    pub fn toggle_settings(&self) {
        let next = !self.settings_open.snapshot();
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
        let theme = THEME_CHOICES[self.set_theme.snapshot().min(THEME_CHOICES.len() - 1)];
        let blink = self.set_blink.snapshot();
        crate::config::upsert_config_key(&path, "font-size", &self.set_font.snapshot().to_string());
        crate::config::upsert_config_key(&path, "theme", theme);
        crate::config::upsert_config_key(
            &path,
            "cursor-style-blink",
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
            TermAction::ToggleQuickTerminal => self.toggle_quick(),
            TermAction::LastTab => self.select_last_tab(),
            TermAction::CloseWindow => self.close_window(),
            TermAction::ToggleTabBar => self.toggle_tab_bar(),
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
                let items = items.clone();
                move |i: SelfId<usize>| {
                    let i = *i;
                    let item = items[i];
                    // Rows activate through `button` — the List puts
                    // ButtonStyle::Plain + ListRowChrome into the row env, so a
                    // row tap is the framework's own button path (a bare
                    // `.on_tap` on row content does not fire).
                    let row = button(Label::new(item.name, {
                        let name = item.name;
                        let chord = item.chord;
                        move || hstack((
                            text(name).foreground(Foreground),
                            Spacer::flexible(),
                            text(chord).muted(),
                        ))
                    }))
                    .action(move |app: AppState| app.run_palette_at(i));
                    ListItem::new(row)
                }
            })
            .selection(&state.palette_sel)
            .scroll_controller(&state.palette_scroll)
            .anyview()
        }
    });
    // Hit-transparent like the search/title fields (WATERUI_FEEDBACK #50).
    let panel = vstack((
        field("type a command", &state.palette_query).hittable(false),
        list,
    ))
    .spacing(4.0)
    .padding()
    .max_height(430.0)
    .background(Surface)
    // A click on the panel returns embedded focus to the live pane —
    // without it the dead region would clear focus and trap the keys.
    .on_tap(|app: AppState| app.refocus_selected());
    // The dim scrim is part of the palette's world too — a click on it
    // returns keys to the pane rather than dropping focus to nothing.
    vstack((panel, Spacer::flexible()))
        .background(Srgb::BLACK.with_opacity(0.45))
        .on_tap(|app: AppState| app.refocus_selected())
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
        button("Apply").action(|app: AppState| app.apply_settings()),
    ))
    .spacing(8.0)
    .padding()
    .background(Surface);
    vstack((panel, Spacer::flexible())).background(Srgb::BLACK.with_opacity(0.45))
}

#[cfg(test)]
mod tests {
    use super::{SplitDir, SplitNode, auto_split_dir};
    use nami::Signal;

    #[test]
    fn auto_split_picks_by_aspect() {
        // Ghostty `new_split:auto` — a wider-than-tall pane splits to the
        // right (Row), a taller-than-wide pane splits down (Column).
        assert_eq!(auto_split_dir(200.0, 100.0), SplitDir::Row);
        assert_eq!(auto_split_dir(100.0, 200.0), SplitDir::Column);
        assert_eq!(auto_split_dir(100.0, 100.0), SplitDir::Row);
    }

    /// [0 | 1] split side-by-side, then 1 split down → [0 | {1 / 2}].
    fn nested() -> SplitNode {
        let mut t = SplitNode::Leaf(0);
        assert!(t.split(SplitDir::Row, 0, 1, 800.0, false));
        assert!(t.split(SplitDir::Column, 1, 2, 400.0, false));
        t
    }

    #[test]
    fn focus_navigates_directionally() {
        let t = nested();
        // Right from leaf 0 crosses the row split into leaf 1's edge.
        assert_eq!(t.neighbor(0, true, true), Some(1));
        // Left from leaf 1 returns to 0; left from 2 wraps to the edge
        // leaf facing it on the same side.
        assert_eq!(t.neighbor(1, true, false), Some(0));
        assert_eq!(t.neighbor(2, true, false), Some(0));
        // Down from 1 crosses the nested column split to 2, and back.
        assert_eq!(t.neighbor(1, false, true), Some(2));
        assert_eq!(t.neighbor(2, false, false), Some(1));
        // Edges: past the right/bottom of the layout there is no neighbor.
        assert_eq!(t.neighbor(2, true, true), None);
        assert_eq!(t.neighbor(2, false, true), None);
    }

    /// A split seeds both children at half the parent's measured slot
    /// minus the divider, so the row's frames sum to the slot;
    /// `remove` reseeds the surviving subtree's own sizes.
    #[test]
    fn split_seeds_and_remove_reseeds_sizes() {
        let mut t = SplitNode::Leaf(0);
        assert!(t.split(SplitDir::Row, 0, 1, 800.0, false));
        let SplitNode::Split { children, sizes, .. } = &t else {
            panic!("not a split");
        };
        assert_eq!(sizes.snapshot().as_slice(), &[396.5, 396.5]);
        assert_eq!(children.len(), 2);
        // Nested split inside child 1 reseeds its own slot to halves
        // minus the divider.
        assert!(t.split(SplitDir::Column, 1, 2, 400.0, false));
        let rest = t.remove(0).expect("tree survives removing leaf 0");
        let SplitNode::Split { sizes, children, .. } = &rest else {
            panic!("expected the nested column split to remain");
        };
        assert_eq!(children.len(), 2);
        assert_eq!(sizes.snapshot().as_slice(), &[196.5, 196.5]);
    }
}
