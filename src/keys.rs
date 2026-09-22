//! Keyboard input → bytes to the PTY. Covers the xterm/VT encoding matrix:
//! application-cursor modes, modifier parameters, function keys, Alt
//! meta-prefixed text, and the kitty keyboard-protocol disambiguation flag.

use alacritty_terminal::term::TermMode;
use keyboard_types::{Code, Key, Modifiers, NamedKey};

/// Build the byte sequence for a key press. `text` is the already-composed
/// text for the key when the platform produced it (text input comes via
/// `TextInput` separately; here we only care about control/alt paths and
/// named keys).
pub fn key_to_bytes(key: &Key, code: Code, mods: Modifiers, mode: TermMode) -> Option<Vec<u8>> {
    let shift = mods.contains(Modifiers::SHIFT);
    let ctrl = mods.contains(Modifiers::CONTROL);
    let alt = mods.contains(Modifiers::ALT) || mods.contains(Modifiers::META);
    let kitty = mode.intersects(TermMode::KITTY_KEYBOARD_PROTOCOL);

    if let Key::Named(named) = key {
        if let Some(bytes) = named_key_bytes(*named, code, mods, mode) {
            return Some(prefix_alt(bytes, alt));
        }
        if kitty && let Some(bytes) = kitty_named(*named, mods, mode) {
            return Some(bytes);
        }
        return None;
    }

    let Key::Character(text) = key else { return None };
    let ch = text.chars().next()?;

    // Ctrl+letter → C0 control codes; this is also the terminal-action lane
    // (Ctrl+Shift+C/V are intercepted before this function).
    if ctrl {
        let base = ch.to_ascii_lowercase();
        let n = match base {
            'a'..='z' => Some(base as u8 - b'a' + 1),
            ' ' => Some(0),
            '2' | '@' => Some(0),
            '3' | '[' => Some(0x1b),
            '4' | '\\' => Some(0x1c),
            '5' | ']' => Some(0x1d),
            '6' | '^' => Some(0x1e),
            '7' | '_' => Some(0x1f),
            '8' | '?' => Some(0x7f),
            _ => None,
        };
        if let Some(n) = n {
            if kitty && mode.contains(TermMode::DISAMBIGUATE_ESC_CODES) {
                return Some(kitty_csi_u(ch as u32, mods, false));
            }
            return Some(prefix_alt(vec![n], alt));
        }
        // Unmapped Ctrl+key: kitty disambiguate reports it as CSI u.
        if kitty && mode.contains(TermMode::DISAMBIGUATE_ESC_CODES) {
            return Some(kitty_csi_u(ch as u32, mods, false));
        }
        return None;
    }

    // Alt+char → ESC prefix.
    if alt {
        let mut out = vec![0x1b];
        if shift {
            out.extend(text.to_uppercase().as_bytes());
        } else {
            out.extend(text.as_bytes());
        }
        return Some(out);
    }

    // Plain printable characters arrive via TextInput — nothing to do here.
    None
}

/// Bytes for the release half of a key event (kitty report-event-types).
pub fn key_release_bytes(key: &Key, mods: Modifiers, mode: TermMode) -> Option<Vec<u8>> {
    if !mode.contains(TermMode::REPORT_EVENT_TYPES) {
        return None;
    }
    if let Key::Character(text) = key && let Some(ch) = text.chars().next() {
        let m = kitty_mod(mods);
        return Some(format!("\x1b[{};{m}:3u", ch as u32).into_bytes());
    }
    if let Key::Named(named) = key && let Some(num) = kitty_named_number(*named) {
        let m = kitty_mod(mods);
        return Some(format!("\x1b[{num};{m}:3u").into_bytes());
    }
    None
}

/// ESC-prefix helper: legacy meta = prepend ESC.
fn prefix_alt(mut bytes: Vec<u8>, alt: bool) -> Vec<u8> {
    if alt {
        bytes.insert(0, 0x1b);
    }
    bytes
}

/// xterm modifier parameter: 1 + shift|alt|ctrl|super.
fn xterm_mod(mods: Modifiers) -> u8 {
    1 + mods.contains(Modifiers::SHIFT) as u8
        + (mods.contains(Modifiers::ALT) || mods.contains(Modifiers::META)) as u8 * 2
        + mods.contains(Modifiers::CONTROL) as u8 * 4
        + mods.contains(Modifiers::META) as u8 * 8
}

fn kitty_mod(mods: Modifiers) -> u8 {
    xterm_mod(mods)
}

/// CSI with a modifier parameter: `\x1b[<n>;<m><final>`.
fn csi_mod(n: u8, mods: Modifiers, fin: char) -> Vec<u8> {
    let m = xterm_mod(mods);
    if m == 1 {
        format!("\x1b[{n}{fin}").into_bytes()
    } else {
        format!("\x1b[{n};{m}{fin}").into_bytes()
    }
}

/// SS3 with optional modifier → `\x1bO<m><final>` or plain `\x1bO<f>`.
fn ss3(mods: Modifiers, fin: char) -> Vec<u8> {
    let m = xterm_mod(mods);
    if m == 1 {
        format!("\x1bO{fin}").into_bytes()
    } else {
        format!("\x1b[1;{m}{fin}").into_bytes()
    }
}

/// Named keys → xterm sequences (respecting application-cursor mode).
fn named_key_bytes(
    named: NamedKey,
    code: Code,
    mods: Modifiers,
    mode: TermMode,
) -> Option<Vec<u8>> {
    use NamedKey::*;
    let app = mode.contains(TermMode::APP_CURSOR);
    let num = |n: u8| -> Vec<u8> { csi_mod(n, mods, '~') };
    Some(match named {
        ArrowUp => {
            if app { ss3(mods, 'A') } else { csi_mod(1, mods, 'A') }
        }
        ArrowDown => {
            if app { ss3(mods, 'B') } else { csi_mod(1, mods, 'B') }
        }
        ArrowRight => {
            if app { ss3(mods, 'C') } else { csi_mod(1, mods, 'C') }
        }
        ArrowLeft => {
            if app { ss3(mods, 'D') } else { csi_mod(1, mods, 'D') }
        }
        Home => {
            if app { ss3(mods, 'H') } else { csi_mod(1, mods, 'H') }
        }
        End => {
            if app { ss3(mods, 'F') } else { csi_mod(1, mods, 'F') }
        }
        PageUp => num(5),
        PageDown => num(6),
        Insert => num(2),
        Delete => num(3),
        Enter => {
            if mode.contains(TermMode::APP_KEYPAD) && code == Code::NumpadEnter {
                vec![0x1b, b'O', b'M']
            } else if mods.contains(Modifiers::SHIFT) {
                // Shift+Enter (kitty-aware apps read this; harmless otherwise).
                b"\x1b[13;2u".to_vec()
            } else {
                vec![b'\r']
            }
        }
        Tab => {
            if mods.contains(Modifiers::SHIFT) {
                b"\x1b[Z".to_vec()
            } else {
                vec![b'\t']
            }
        }
        Backspace => vec![0x7f],
        Escape => vec![0x1b],
        F1 => ss3(mods, 'P'),
        F2 => ss3(mods, 'Q'),
        F3 => ss3(mods, 'R'),
        F4 => ss3(mods, 'S'),
        F5 => num(15),
        F6 => num(17),
        F7 => num(18),
        F8 => num(19),
        F9 => num(20),
        F10 => num(21),
        F11 => num(23),
        F12 => num(24),
        _ => {
            // Numpad digits/operators under application keypad mode.
            if mode.contains(TermMode::APP_KEYPAD)
                && let Some(fin) = numpad_application_key(code)
            {
                return Some(vec![0x1b, b'O', fin]);
            }
            return None;
        }
    })
}

/// Application-keypad mappings (DECPAM): numpad keys → SS3 letter.
fn numpad_application_key(code: Code) -> Option<u8> {
    Some(match code {
        Code::NumpadEnter => b'M',
        Code::NumpadMultiply => b'j',
        Code::NumpadAdd => b'k',
        Code::NumpadComma => b'l',
        Code::NumpadSubtract => b'm',
        Code::NumpadDecimal => b'n',
        Code::NumpadDivide => b'o',
        Code::Numpad0 => b'p',
        Code::Numpad1 => b'q',
        Code::Numpad2 => b'r',
        Code::Numpad3 => b's',
        Code::Numpad4 => b't',
        Code::Numpad5 => b'u',
        Code::Numpad6 => b'v',
        Code::Numpad7 => b'w',
        Code::Numpad8 => b'x',
        Code::Numpad9 => b'y',
        _ => return None,
    })
}

/// Kitty CSI-u key numbers for named keys.
fn kitty_named_number(named: NamedKey) -> Option<u32> {
    use NamedKey::*;
    Some(match named {
        Escape => 27,
        Enter => 13,
        Tab => 9,
        Backspace => 127,
        Insert => 57399,
        Delete => 57398,
        ArrowLeft => 1,
        ArrowRight => 2,
        ArrowUp => 3,
        ArrowDown => 4,
        PageUp => 5,
        PageDown => 6,
        Home => 7,
        End => 8,
        F1 => 11,
        F2 => 12,
        F3 => 13,
        F4 => 14,
        F5 => 15,
        F6 => 17,
        F7 => 18,
        F8 => 19,
        F9 => 20,
        F10 => 21,
        F11 => 23,
        F12 => 24,
        _ => return None,
    })
}

/// Kitty-protocol encoding for named keys when DISAMBIGUATE is on.
fn kitty_named(named: NamedKey, mods: Modifiers, mode: TermMode) -> Option<Vec<u8>> {
    let num = kitty_named_number(named)?;
    let m = kitty_mod(mods);
    // Keys that already have unambiguous sequences stay legacy unless the
    // program asked for event types / alternates — mirrors what kitty itself
    // sends for these keys.
    let _ = mode;
    if m == 1 && !mode.contains(TermMode::REPORT_EVENT_TYPES | TermMode::REPORT_ALTERNATE_KEYS) {
        return None;
    }
    Some(format!("\x1b[{num};{m}u").into_bytes())
}

/// `\x1b[<codepoint>;<mods>u` or `<codepoint>;<mods>:3u` for releases.
fn kitty_csi_u(codepoint: u32, mods: Modifiers, release: bool) -> Vec<u8> {
    let m = kitty_mod(mods);
    if release {
        format!("\x1b[{codepoint};{m}:3u").into_bytes()
    } else {
        format!("\x1b[{codepoint};{m}u").into_bytes()
    }
}

/// The terminal-action chord test: Ctrl+Shift (Linux/Windows convention).
/// Returns the action name so the caller can run it instead of writing to
/// the PTY.
pub fn action_chord(key: &Key, mods: Modifiers) -> Option<TermAction> {
    if !(mods.contains(Modifiers::CONTROL) && mods.contains(Modifiers::SHIFT)) {
        return None;
    }
    if let Key::Named(named) = key {
        return Some(match named {
            NamedKey::ArrowUp => TermAction::PromptPrev,
            NamedKey::ArrowDown => TermAction::PromptNext,
            NamedKey::F11 => TermAction::Fullscreen,
            _ => return None,
        });
    }
    let Key::Character(text) = key else { return None };
    Some(match text.to_ascii_lowercase().as_str() {
        "c" => TermAction::Copy,
        "v" => TermAction::Paste,
        "t" => TermAction::NewTab,
        "w" => TermAction::CloseTab,
        "n" => TermAction::NewWindow,
        "a" => TermAction::SelectAll,
        "e" => TermAction::SplitRight,
        "d" => TermAction::SplitDown,
        "]" | "}" => TermAction::FocusNextPane,
        "[" | "{" => TermAction::FocusPrevPane,
        "+" | "=" => TermAction::FontBigger,
        "-" | "_" => TermAction::FontSmaller,
        "0" | ")" => TermAction::FontReset,
        "k" => TermAction::ClearScrollback,
        "f" => TermAction::Search,
        "p" => TermAction::Palette,
        "," | "<" => TermAction::Settings,
        _ => return None,
    })
}

/// Ctrl (no shift) digits select tabs 1-8; Ctrl+Tab / Ctrl+Shift+Tab cycle.
pub fn tab_chord(key: &Key, code: Code, mods: Modifiers) -> Option<TermAction> {
    if mods.contains(Modifiers::CONTROL) && !mods.contains(Modifiers::SHIFT) {
        let digit = match code {
            Code::Digit1 => Some(1),
            Code::Digit2 => Some(2),
            Code::Digit3 => Some(3),
            Code::Digit4 => Some(4),
            Code::Digit5 => Some(5),
            Code::Digit6 => Some(6),
            Code::Digit7 => Some(7),
            Code::Digit8 => Some(8),
            _ => None,
        };
        if let Some(n) = digit {
            return Some(TermAction::SelectTab(n));
        }
        if matches!(key, Key::Named(NamedKey::Tab)) {
            return Some(TermAction::NextTab);
        }
        if matches!(key, Key::Named(NamedKey::PageDown)) {
            return Some(TermAction::NextTab);
        }
        if matches!(key, Key::Named(NamedKey::PageUp)) {
            return Some(TermAction::PrevTab);
        }
    }
    if mods.contains(Modifiers::CONTROL | Modifiers::SHIFT)
        && matches!(key, Key::Named(NamedKey::Tab))
    {
        return Some(TermAction::PrevTab);
    }
    None
}

/// Actions the app performs rather than forwarding as bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermAction {
    Copy,
    Paste,
    NewTab,
    CloseTab,
    /// Open a whole new OS window (its own tabs and sessions).
    NewWindow,
    NextTab,
    PrevTab,
    SelectTab(usize),
    FontBigger,
    FontSmaller,
    FontReset,
    ClearScrollback,
    Search,
    /// Jump viewport to the previous/next OSC 133 prompt mark.
    PromptPrev,
    PromptNext,
    /// Select the whole viewport.
    SelectAll,
    /// Scroll the viewport to the top of scrollback / the bottom.
    ScrollToTop,
    ScrollToBottom,
    /// Shut down all sessions and exit.
    Quit,
    /// Split the focused pane right/down (new pane takes half its slot).
    SplitRight,
    SplitDown,
    /// Cycle pane focus within the tab.
    FocusNextPane,
    FocusPrevPane,
    /// Toggle borderless fullscreen.
    Fullscreen,
    /// Open the command palette.
    Palette,
    /// Open the settings page.
    Settings,
}

