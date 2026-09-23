//! Drop-down "quick terminal": an X11 root-window key grab toggles a
//! borderless top-docked window. The grab runs on its own thread and
//! forwards presses over an async channel; the drain future on the main
//! thread flips the quick window's `WindowState` binding — same wake
//! pattern the PTY notifier uses (SceneInvalidator is `Rc`/`!Send`).

use std::thread;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, GrabMode, Keycode, ModMask};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

/// X11 keysym for F12 (X11 keysymdef `XK_F12`).
pub const XK_F12: u32 = 0xffc9;

/// Resolve a keysym to a keycode via `GetKeyboardMapping` (keycodes vary
/// by keymap — hardcoding F12 = 96 is not portable).
fn keysym_to_keycode(conn: &RustConnection, keysym: u32) -> Option<Keycode> {
    let (min, max) = {
        let setup = conn.setup();
        (setup.min_keycode, setup.max_keycode)
    };
    let count = max.wrapping_sub(min).wrapping_add(1);
    let reply = conn.get_keyboard_mapping(min, count).ok()?.reply().ok()?;
    let per = reply.keysyms_per_keycode as usize;
    for (i, chunk) in reply.keysyms.chunks(per.max(1)).enumerate() {
        if chunk.contains(&keysym) {
            return Some(min.wrapping_add(i as u8));
        }
    }
    None
}

/// Screen size of the connection's default screen (used to size the
/// drop-down window: full width, 45% height, docked at y = 0).
pub fn screen_size() -> Option<(f64, f64)> {
    let (conn, screen) = RustConnection::connect(None).ok()?;
    let root = conn.setup().roots.get(screen)?;
    Some((
        f64::from(root.width_in_pixels),
        f64::from(root.height_in_pixels),
    ))
}

/// Spawn the grab thread. `Some` only on a reachable X11 display where the
/// key is free — under Wayland, headless, or when another client already
/// owns the hotkey it returns `None` and the caller marks the feature
/// unavailable.
pub fn spawn_hotkey<E: Clone + Send + 'static>(
    send: async_channel::Sender<E>,
    keysym: u32,
    event: E,
) -> Option<thread::JoinHandle<()>> {
    let Ok((conn, screen)) = RustConnection::connect(None) else {
        eprintln!("quickterm: X11 connect failed");
        return None;
    };
    let root = conn.setup().roots.get(screen)?.root;
    let Some(keycode) = keysym_to_keycode(&conn, keysym) else {
        eprintln!("quickterm: keysym {keysym:#x} has no keycode in the keymap");
        return None;
    };
    // `ModMask::ANY` conflicts with *every* existing grab on the key (a
    // desktop shell's Ctrl+F12 makes it fail), so grab the bare key in all
    // lock-mask variants instead — CapsLock, NumLock, and both together.
    let variants = [
        ModMask::from(0u8),
        ModMask::LOCK,
        ModMask::M2,
        ModMask::LOCK | ModMask::M2,
    ];
    for mask in variants {
        let cookie = conn.grab_key(
            false,
            root,
            mask,
            keycode,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
        );
        let failed = match cookie {
            Ok(c) => c.check().err().map(|e| e.to_string()),
            Err(e) => Some(e.to_string()),
        };
        if let Some(e) = failed {
            eprintln!("quickterm: grab_key({mask:?}) failed: {e}");
            return None;
        }
    }
    conn.flush().ok()?;
    eprintln!("quickterm: F12 grabbed as keycode {keycode} on root {root:#x}");
    Some(thread::spawn(move || loop {
        match conn.wait_for_event() {
            Ok(Event::KeyPress(e)) if e.detail == keycode => {
                let _ = send.try_send(event.clone());
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }))
}
