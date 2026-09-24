//! X11 PRIMARY selection (Ghostty `copy-on-select` + middle-click paste).
//!
//! ICCCM ownership: to *serve* PRIMARY we must answer `SelectionRequest`
//! events, and the owner window must be one we created — so a dedicated
//! thread owns a 1x1 window on its own connection and serves requests for
//! the text the app last claimed. Middle-click paste is the requestor
//! path: `convert_selection` + wait for `SelectionNotify`. Off-X11 (or on
//! connect failure) every entry point is inert.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt, CreateWindowAux, PropMode, SelectionNotifyEvent,
    SelectionRequestEvent, Window, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::x11_utils::Serialize;

#[derive(Clone, Copy)]
struct Atoms {
    primary: Atom,
    utf8: Atom,
    text: Atom,
    targets: Atom,
    prop: Atom,
}

fn intern(conn: &RustConnection) -> Option<Atoms> {
    let get = |name: &[u8]| {
        conn.intern_atom(false, name)
            .ok()?
            .reply()
            .ok()
            .map(|r| r.atom)
    };
    Some(Atoms {
        primary: get(b"PRIMARY")?,
        utf8: get(b"UTF8_STRING")?,
        text: get(b"TEXT")?,
        targets: get(b"TARGETS")?,
        prop: get(b"HYDROTERM_SEL")?,
    })
}

fn small_window(conn: &RustConnection, root: Window) -> Option<Window> {
    let win = conn.generate_id().ok()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        win,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        x11rb::COPY_FROM_PARENT,
        &CreateWindowAux::new(),
    )
    .ok()?;
    Some(win)
}

/// Answers one `SelectionRequest` against `text`: TARGETS gets the atom
/// list, UTF8_STRING/STRING/TEXT get the bytes, anything else is refused
/// (property `NONE` in the notify).
fn respond(conn: &RustConnection, atoms: &Atoms, text: &[u8], r: &SelectionRequestEvent) {
    // Obsolete clients send property NONE — answer on `target` instead.
    let mut prop = if r.property == x11rb::NONE {
        r.target
    } else {
        r.property
    };
    if r.target == atoms.targets {
        let data = [atoms.targets, atoms.utf8, atoms.text, AtomEnum::STRING.into()];
        let _ = conn.change_property32(PropMode::REPLACE, r.requestor, prop, AtomEnum::ATOM, &data);
    } else if r.target == atoms.utf8
        || r.target == atoms.text
        || r.target == Atom::from(AtomEnum::STRING)
    {
        let _ = conn.change_property8(PropMode::REPLACE, r.requestor, prop, r.target, text);
    } else {
        prop = x11rb::NONE;
    }
    let notify = SelectionNotifyEvent {
        response_type: 31, // SelectionNotify
        sequence: r.sequence,
        time: r.time,
        requestor: r.requestor,
        selection: r.selection,
        target: r.target,
        property: prop,
    };
    // serialize() yields the 24-byte body; the wire event is 32 bytes.
    let mut wire = [0u8; 32];
    wire[..24].copy_from_slice(&notify.serialize());
    let _ = conn.send_event(
        false,
        r.requestor,
        x11rb::protocol::xproto::EventMask::from(0u32),
        wire,
    );
    let _ = conn.flush();
}

/// The PRIMARY owner thread: claims PRIMARY whenever the app sends new
/// text, then serves requests until another client takes ownership.
fn owner_loop(rx: Receiver<Vec<u8>>) {
    let Some((conn, screen)) = RustConnection::connect(None).ok() else {
        return;
    };
    let Some(root) = conn.setup().roots.get(screen).map(|s| s.root) else {
        return;
    };
    let (Some(win), Some(atoms)) = (small_window(&conn, root), intern(&conn)) else {
        return;
    };
    let mut text = Vec::new();
    loop {
        while let Ok(t) = rx.try_recv() {
            text = t;
            let _ = conn.set_selection_owner(win, atoms.primary, x11rb::CURRENT_TIME);
            let _ = conn.flush();
        }
        if let Ok(Some(ev)) = conn.poll_for_event() {
            match ev {
                Event::SelectionRequest(r) => respond(&conn, &atoms, &text, &r),
                Event::SelectionClear(_) => {}
                _ => {}
            }
        } else {
            thread::sleep(Duration::from_millis(5));
        }
    }
}

/// PRIMARY clipboard handle: `claim` feeds the owner thread, `read`
/// requests the current PRIMARY contents.
pub struct Xsel {
    tx: Sender<Vec<u8>>,
}

impl Xsel {
    /// Spawn the owner thread on X11; `None` on Wayland-only or failure.
    pub fn new() -> Option<Self> {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() && std::env::var_os("DISPLAY").is_none() {
            return None;
        }
        // Fail fast when no server is reachable rather than spawn a dead thread.
        let (conn, _) = RustConnection::connect(None).ok()?;
        drop(conn);
        let (tx, rx) = channel::<Vec<u8>>();
        thread::spawn(move || owner_loop(rx));
        Some(Self { tx })
    }

    /// Become the PRIMARY owner, serving `text` to requestors.
    pub fn claim(&self, text: String) {
        let _ = self.tx.send(text.into_bytes());
    }

    /// Read PRIMARY (middle-click paste). Fresh connection per call —
    /// cheap enough at click rate.
    pub fn read() -> Option<String> {
        let (conn, screen) = RustConnection::connect(None).ok()?;
        let root = conn.setup().roots.get(screen)?.root;
        let win = small_window(&conn, root)?;
        let atoms = intern(&conn)?;
        conn.convert_selection(
            win,
            atoms.primary,
            atoms.utf8,
            atoms.prop,
            x11rb::CURRENT_TIME,
        )
        .ok()?;
        conn.flush().ok()?;
        let deadline = Instant::now() + Duration::from_millis(250);
        loop {
            if let Ok(Some(Event::SelectionNotify(n))) = conn.poll_for_event() {
                if n.property == x11rb::NONE {
                    return None;
                }
                let reply = conn
                    .get_property(true, win, atoms.prop, AtomEnum::ANY, 0, u32::MAX / 4)
                    .ok()?
                    .reply()
                    .ok()?;
                return String::from_utf8(reply.value).ok();
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(3));
        }
    }
}
