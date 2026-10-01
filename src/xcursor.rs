//! Hide-the-pointer-while-typing (Ghostty `mouse-hide-while-typing`).
//! X11 only: XFixes `HideCursor`/`ShowCursor` against our own toplevel,
//! found on `_NET_CLIENT_LIST` by `_NET_WM_PID`. On any other session
//! type the whole thing is inert.

use x11rb::connection::Connection;
use x11rb::protocol::xfixes;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, Window};
use x11rb::rust_connection::RustConnection;

/// Lazily-opened X11 connection + our toplevel window id.
pub struct CursorHider {
    conn: RustConnection,
    win: Window,
    hidden: bool,
}

impl CursorHider {
    /// Connect and resolve our toplevel XID, or `None` off-X11.
    pub fn new() -> Option<Self> {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() && std::env::var_os("DISPLAY").is_none() {
            return None;
        }
        let (conn, screen) = RustConnection::connect(None).ok()?;
        let root = conn.setup().roots.get(screen)?.root;
        let client_list = conn
            .intern_atom(false, b"_NET_CLIENT_LIST")
            .ok()?
            .reply()
            .ok()?
            .atom;
        let wm_pid = conn
            .intern_atom(false, b"_NET_WM_PID")
            .ok()?
            .reply()
            .ok()?
            .atom;
        let list = conn
            .get_property(false, root, client_list, AtomEnum::WINDOW, 0, 1024)
            .ok()?
            .reply()
            .ok()?;
        let pid = std::process::id();
        let win = list.value32().and_then(|mut ids| {
            ids.find(|&w| {
                conn.get_property(false, w, wm_pid, AtomEnum::CARDINAL, 0, 1)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .and_then(|r| r.value32().and_then(|mut v| v.next()))
                    == Some(pid)
            })
        })?;
        Some(Self {
            conn,
            win,
            hidden: false,
        })
    }

    /// Called from the typed-input path: hide once.
    pub fn hide(&mut self) {
        if !self.hidden {
            let r = xfixes::hide_cursor(&self.conn, self.win).map(|c| c.sequence_number());
            let _ = self.conn.flush();
            if std::env::var_os("HYDROTERM_DEBUG_INPUT").is_some() {
                eprintln!("[cursor-hide] win={:#x} -> {r:?}", self.win);
            }
            self.hidden = true;
        }
    }

    /// Called from every pointer event: show again once.
    pub fn show(&mut self) {
        if self.hidden {
            let r = xfixes::show_cursor(&self.conn, self.win).map(|c| c.sequence_number());
            let _ = self.conn.flush();
            if std::env::var_os("HYDROTERM_DEBUG_INPUT").is_some() {
                eprintln!("[cursor-show] win={:#x} -> {r:?}", self.win);
            }
            self.hidden = false;
        }
    }
}
