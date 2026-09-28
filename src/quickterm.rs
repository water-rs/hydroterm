//! Drop-down "quick terminal": an X11 root-window key grab toggles a
//! borderless top-docked window. The grab runs on its own thread and
//! forwards presses over an async channel; the drain future on the main
//! thread flips the quick window's `WindowState` binding — same wake
//! pattern the PTY notifier uses (SceneInvalidator is `Rc`/`!Send`).

use std::thread;

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{ConnectionExt as _, GrabMode, Keycode, ModMask};
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

/// Canonical keybind key name → X11 keysym, for `global:` chords. Covers
/// the whole `canonical_key_name` set: single chars, F1–F24, and the
/// named navigation keys.
pub fn key_name_to_keysym(name: &str) -> Option<u32> {
    let mut ch = name.chars();
    if let (Some(c), None) = (ch.next(), ch.next()) {
        return Some(u32::from(c));
    }
    if let Some(digits) = name.strip_prefix('f')
        && let Ok(d) = digits.parse::<u32>()
        && (1..=24).contains(&d)
    {
        return Some(0xffbd + d); // XK_F1 = 0xffbe
    }
    Some(match name {
        "arrowup" => 0xff52,
        "arrowdown" => 0xff54,
        "arrowleft" => 0xff51,
        "arrowright" => 0xff53,
        "pageup" => 0xff55,
        "pagedown" => 0xff56,
        "home" => 0xff50,
        "end" => 0xff57,
        "insert" => 0xff63,
        "delete" => 0xffff,
        "backspace" => 0xff08,
        "tab" => 0xff09,
        "enter" => 0xff0d,
        "escape" => 0xff1b,
        "space" => 0x20,
        _ => return None,
    })
}

/// `ctrl+alt+shift+super` bits of a canonical chord → an X11 `ModMask`
/// bitfield (Shift=0x01, Control=0x04, Mod1/Alt=0x08, Mod4/Super=0x40).
pub fn chord_mod_bits(ctrl: bool, alt: bool, shift: bool, sup: bool) -> u8 {
    (shift as u8) | ((alt as u8) << 3) | ((ctrl as u8) << 2) | ((sup as u8) << 6)
}

/// Spawn the grab thread. `Some` only on a reachable X11 display where the
/// key is free — under Wayland, headless, or when another client already
/// owns the hotkey it returns `None` and the caller marks the feature
/// unavailable. `mod_bits` are extra modifiers that must be held
/// (`chord_mod_bits`). Setting `stop` ends the thread — its connection
/// drop releases every grab it made (config reloads re-grab `global:`
/// chords this way).
pub fn spawn_hotkey<E: Clone + Send + 'static>(
    send: async_channel::Sender<E>,
    keysym: u32,
    mod_bits: u8,
    event: E,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
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
    // desktop shell's Ctrl+F12 makes it fail), so grab the chord in all
    // lock-mask variants instead — CapsLock, NumLock, and both together.
    let variants = [
        ModMask::from(mod_bits),
        ModMask::LOCK | ModMask::from(mod_bits),
        ModMask::M2 | ModMask::from(mod_bits),
        ModMask::LOCK | ModMask::M2 | ModMask::from(mod_bits),
    ];
    for mask in variants {
        let cookie = conn.grab_key(false, root, mask, keycode, GrabMode::ASYNC, GrabMode::ASYNC);
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
    eprintln!(
        "quickterm: grabbed {keysym:#x}+{mod_bits:#x} as keycode {keycode} on root {root:#x}"
    );
    Some(thread::spawn(move || {
        loop {
            // Poll rather than block on `wait_for_event` so `stop` can tear
            // the grab down without a wakeup mechanism on this connection.
            if stop.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            match conn.poll_for_event() {
                Ok(Some(Event::KeyPress(e))) if e.detail == keycode => {
                    let _ = send.try_send(event.clone());
                }
                Ok(Some(_)) => {}
                Ok(None) => thread::sleep(std::time::Duration::from_millis(60)),
                Err(_) => break,
            }
        }
    }))
}
