//! Keyboard input → bytes to the PTY. Covers the xterm/VT encoding matrix:
//! application-cursor modes, modifier parameters, function keys, Alt
//! meta-prefixed text, and the kitty keyboard-protocol disambiguation flag.

use std::sync::LazyLock;

use alacritty_terminal::term::TermMode;
use keyboard_types::{Code, Key, Modifiers, NamedKey};
use waterui::component::{Shortcut, ShortcutKey};

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

    let Key::Character(text) = key else {
        return None;
    };
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
    if let Key::Character(text) = key
        && let Some(ch) = text.chars().next()
    {
        let m = kitty_mod(mods);
        return Some(format!("\x1b[{};{m}:3u", ch as u32).into_bytes());
    }
    if let Key::Named(named) = key
        && let Some(num) = kitty_named_number(*named)
    {
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
        // Both params at their default → omit them entirely: `\e[D`,
        // not `\e[1D` (readline binds only the param-less form).
        if n == 1 {
            format!("\x1b[{fin}").into_bytes()
        } else {
            format!("\x1b[{n}{fin}").into_bytes()
        }
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
            if app {
                ss3(mods, 'A')
            } else {
                csi_mod(1, mods, 'A')
            }
        }
        ArrowDown => {
            if app {
                ss3(mods, 'B')
            } else {
                csi_mod(1, mods, 'B')
            }
        }
        ArrowRight => {
            if app {
                ss3(mods, 'C')
            } else {
                csi_mod(1, mods, 'C')
            }
        }
        ArrowLeft => {
            if app {
                ss3(mods, 'D')
            } else {
                csi_mod(1, mods, 'D')
            }
        }
        Home => {
            if app {
                ss3(mods, 'H')
            } else {
                csi_mod(1, mods, 'H')
            }
        }
        End => {
            if app {
                ss3(mods, 'F')
            } else {
                csi_mod(1, mods, 'F')
            }
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

#[derive(Clone)]
struct ActionBinding {
    shortcut: Shortcut,
    action: TermAction,
}

fn shortcut(key: impl Into<ShortcutKey>, mods: Modifiers) -> Shortcut {
    let mut shortcut = Shortcut::new(key);
    if mods.contains(Modifiers::CONTROL) {
        shortcut = shortcut.control();
    }
    if mods.contains(Modifiers::ALT) {
        shortcut = shortcut.option();
    }
    if mods.contains(Modifiers::SHIFT) {
        shortcut = shortcut.shift();
    }
    if mods.contains(Modifiers::META) {
        shortcut = shortcut.command();
    }
    shortcut
}

fn binding(key: impl Into<ShortcutKey>, mods: Modifiers, action: TermAction) -> ActionBinding {
    ActionBinding {
        shortcut: shortcut(key, mods),
        action,
    }
}

fn shortcut_key_matches(shortcut: &Shortcut, key: &Key, case_sensitive: bool) -> bool {
    if case_sensitive {
        shortcut.key.to_key() == *key
    } else {
        match (&shortcut.key, key) {
            (ShortcutKey::Character(expected), Key::Character(pressed)) => {
                pressed.to_ascii_lowercase() == expected.to_string()
            }
            (ShortcutKey::Named(expected), Key::Named(pressed)) => expected == pressed,
            _ => false,
        }
    }
}

fn binding_matches(
    binding: &ActionBinding,
    key: &Key,
    mods: Modifiers,
    case_sensitive: bool,
) -> bool {
    let required = &binding.shortcut.modifiers;
    let mut expected = Modifiers::empty();
    if required.control() {
        expected |= Modifiers::CONTROL;
    }
    if required.option() {
        expected |= Modifiers::ALT;
    }
    if required.shift() {
        expected |= Modifiers::SHIFT;
    }
    if required.command() {
        expected |= Modifiers::META;
    }
    expected == mods && shortcut_key_matches(&binding.shortcut, key, case_sensitive)
}

fn lookup_action(
    bindings: &[ActionBinding],
    key: &Key,
    mods: Modifiers,
    case_sensitive: bool,
) -> Option<TermAction> {
    bindings
        .iter()
        .find(|binding| binding_matches(binding, key, mods, case_sensitive))
        .map(|binding| binding.action.clone())
}

fn shared_action_bindings() -> &'static [ActionBinding] {
    static BINDINGS: LazyLock<Vec<ActionBinding>> = LazyLock::new(|| {
        vec![binding(
            NamedKey::F11,
            Modifiers::empty(),
            TermAction::Fullscreen,
        )]
    });
    &BINDINGS
}

#[cfg(not(target_os = "macos"))]
fn platform_action_bindings() -> &'static [ActionBinding] {
    static BINDINGS: LazyLock<Vec<ActionBinding>> = LazyLock::new(|| {
        let c = Modifiers::CONTROL;
        let h = Modifiers::SHIFT;
        let a = Modifiers::ALT;
        vec![
            binding('c', c | h, TermAction::Copy),
            binding('v', c | h, TermAction::Paste),
            binding('t', c | h, TermAction::NewTab),
            binding('w', c | h, TermAction::CloseTab),
            binding('n', c | h, TermAction::NewWindow),
            binding('a', c | h, TermAction::SelectAll),
            binding('e', c | h, TermAction::SplitRight),
            binding('d', c | h, TermAction::SplitDown),
            binding('z', c | h, TermAction::PaneZoom),
            binding(']', c | h, TermAction::FocusNextPane),
            binding('}', c | h, TermAction::FocusNextPane),
            binding('[', c | h, TermAction::FocusPrevPane),
            binding('{', c | h, TermAction::FocusPrevPane),
            binding('=', c | h, TermAction::IncreaseFontSize(1)),
            binding('+', c | h, TermAction::IncreaseFontSize(1)),
            binding('-', c | h, TermAction::DecreaseFontSize(1)),
            binding('_', c | h, TermAction::DecreaseFontSize(1)),
            binding('0', c | h, TermAction::FontReset),
            binding(')', c | h, TermAction::FontReset),
            binding('k', c | h, TermAction::ClearScrollback),
            binding('o', c | h, TermAction::CopyLastOutput),
            binding('u', c | h, TermAction::UrlHints),
            binding('g', c | h, TermAction::OpenScrollbackEditor),
            binding('f', c | h, TermAction::Search),
            binding('p', c | h, TermAction::Palette),
            binding(',', c | h, TermAction::ReloadConfig),
            binding('<', c | h, TermAction::ReloadConfig),
            binding('l', c | h, TermAction::ClearScreen),
            binding(NamedKey::ArrowUp, c | h, TermAction::JumpToPrompt(-1)),
            binding(NamedKey::ArrowDown, c | h, TermAction::JumpToPrompt(1)),
            binding(NamedKey::PageUp, c | h, TermAction::MoveTab(-1)),
            binding(NamedKey::PageDown, c | h, TermAction::MoveTab(1)),
            binding(NamedKey::Home, c | h, TermAction::ScrollToTop),
            binding(NamedKey::End, c | h, TermAction::ScrollToBottom),
            binding(NamedKey::Enter, c | h, TermAction::PaneZoom),
            binding('1', c | a, TermAction::GotoSplit(0)),
            binding('2', c | a, TermAction::GotoSplit(1)),
            binding('3', c | a, TermAction::GotoSplit(2)),
            binding('4', c | a, TermAction::GotoSplit(3)),
            binding('5', c | a, TermAction::GotoSplit(4)),
            binding('6', c | a, TermAction::GotoSplit(5)),
            binding('7', c | a, TermAction::GotoSplit(6)),
            binding('8', c | a, TermAction::GotoSplit(7)),
            binding('9', c | a, TermAction::GotoSplit(8)),
            binding(
                NamedKey::ArrowLeft,
                c | h | a,
                TermAction::FocusPaneDir {
                    horizontal: true,
                    forward: false,
                },
            ),
            binding(
                NamedKey::ArrowRight,
                c | h | a,
                TermAction::FocusPaneDir {
                    horizontal: true,
                    forward: true,
                },
            ),
            binding(
                NamedKey::ArrowUp,
                c | h | a,
                TermAction::FocusPaneDir {
                    horizontal: false,
                    forward: false,
                },
            ),
            binding(
                NamedKey::ArrowDown,
                c | h | a,
                TermAction::FocusPaneDir {
                    horizontal: false,
                    forward: true,
                },
            ),
            binding(NamedKey::PageUp, c | h | a, TermAction::FocusPrevPane),
            binding(NamedKey::PageDown, c | h | a, TermAction::FocusNextPane),
            binding(
                NamedKey::ArrowLeft,
                a | h,
                TermAction::ResizePane {
                    horizontal: true,
                    forward: false,
                    px: 48,
                },
            ),
            binding(
                NamedKey::ArrowRight,
                a | h,
                TermAction::ResizePane {
                    horizontal: true,
                    forward: true,
                    px: 48,
                },
            ),
            binding(
                NamedKey::ArrowUp,
                a | h,
                TermAction::ResizePane {
                    horizontal: false,
                    forward: false,
                    px: 48,
                },
            ),
            binding(
                NamedKey::ArrowDown,
                a | h,
                TermAction::ResizePane {
                    horizontal: false,
                    forward: true,
                    px: 48,
                },
            ),
            binding(NamedKey::PageUp, h, TermAction::ScrollPageUp),
            binding(NamedKey::PageDown, h, TermAction::ScrollPageDown),
            binding(NamedKey::ArrowUp, h, TermAction::ScrollPageLines(-1)),
            binding(NamedKey::ArrowDown, h, TermAction::ScrollPageLines(1)),
            binding(NamedKey::Insert, h, TermAction::Paste),
            binding(NamedKey::Tab, c, TermAction::NextTab),
            binding(NamedKey::Tab, c | h, TermAction::PrevTab),
            binding(NamedKey::PageDown, c, TermAction::NextTab),
            binding(NamedKey::PageUp, c, TermAction::PrevTab),
        ]
    });
    &BINDINGS
}

#[cfg(target_os = "macos")]
fn platform_action_bindings() -> &'static [ActionBinding] {
    static BINDINGS: LazyLock<Vec<ActionBinding>> = LazyLock::new(|| {
        let s = Modifiers::META;
        let c = Modifiers::CONTROL;
        let a = Modifiers::ALT;
        let h = Modifiers::SHIFT;
        vec![
            binding('t', s, TermAction::NewTab),
            binding('n', s, TermAction::NewWindow),
            binding(',', s | h, TermAction::ReloadConfig),
            binding(',', s, TermAction::OpenConfig),
            binding('c', s, TermAction::Copy),
            binding('v', s, TermAction::Paste),
            binding('=', s, TermAction::IncreaseFontSize(1)),
            binding('+', s, TermAction::IncreaseFontSize(1)),
            binding('-', s, TermAction::DecreaseFontSize(1)),
            binding('0', s, TermAction::FontReset),
            binding('q', s, TermAction::Quit),
            binding('k', s, TermAction::ClearScreen),
            binding('a', s, TermAction::SelectAll),
            binding('j', s | c | h, TermAction::WriteScreenFile(FileSink::Copy)),
            binding('j', s | h, TermAction::WriteScreenFile(FileSink::Paste)),
            binding('j', s | h | a, TermAction::WriteScreenFile(FileSink::Open)),
            binding('j', s, TermAction::ScrollToSelection),
            binding('t', s | h, TermAction::Undo),
            binding('z', s, TermAction::Undo),
            binding('z', s | h, TermAction::Redo),
            binding('w', s | h | a, TermAction::CloseAllWindows),
            binding('w', s | h, TermAction::CloseWindow),
            binding('w', s | a, TermAction::CloseTab),
            binding('w', s, TermAction::CloseSurface),
            binding('{', s | h, TermAction::PrevTab),
            binding('}', s | h, TermAction::NextTab),
            binding('d', s, TermAction::SplitRight),
            binding('d', s | h, TermAction::SplitDown),
            binding('[', s, TermAction::FocusPrevPane),
            binding(']', s, TermAction::FocusNextPane),
            binding('=', s | c, TermAction::EqualizeSplits),
            binding('f', s, TermAction::StartSearch),
            binding('e', s, TermAction::SearchSelection),
            binding('f', s | h, TermAction::EndSearch),
            binding('g', s, TermAction::NavigateSearch(1)),
            binding('g', s | h, TermAction::NavigateSearch(-1)),
            binding('i', s | a, TermAction::Inspector),
            binding('f', s | c, TermAction::Fullscreen),
            binding('v', s | h, TermAction::PasteFromSelection),
            binding('p', s | h, TermAction::Palette),
            binding(NamedKey::Escape, Modifiers::empty(), TermAction::EndSearch),
            binding(NamedKey::Tab, c, TermAction::NextTab),
            binding(NamedKey::Tab, c | h, TermAction::PrevTab),
            binding(
                NamedKey::ArrowLeft,
                h,
                TermAction::AdjustSelection(AdjustSel::Left),
            ),
            binding(
                NamedKey::ArrowRight,
                h,
                TermAction::AdjustSelection(AdjustSel::Right),
            ),
            binding(
                NamedKey::ArrowUp,
                h,
                TermAction::AdjustSelection(AdjustSel::Up),
            ),
            binding(
                NamedKey::ArrowDown,
                h,
                TermAction::AdjustSelection(AdjustSel::Down),
            ),
            binding(
                NamedKey::PageUp,
                h,
                TermAction::AdjustSelection(AdjustSel::PageUp),
            ),
            binding(
                NamedKey::PageDown,
                h,
                TermAction::AdjustSelection(AdjustSel::PageDown),
            ),
            binding(
                NamedKey::Home,
                h,
                TermAction::AdjustSelection(AdjustSel::Home),
            ),
            binding(
                NamedKey::End,
                h,
                TermAction::AdjustSelection(AdjustSel::End),
            ),
            binding(NamedKey::Home, s, TermAction::ScrollToTop),
            binding(NamedKey::End, s, TermAction::ScrollToBottom),
            binding(NamedKey::PageUp, s, TermAction::ScrollPageUp),
            binding(NamedKey::PageDown, s, TermAction::ScrollPageDown),
            binding(NamedKey::ArrowUp, s, TermAction::JumpToPrompt(-1)),
            binding(NamedKey::ArrowDown, s, TermAction::JumpToPrompt(1)),
            binding(NamedKey::ArrowUp, s | h, TermAction::JumpToPrompt(-1)),
            binding(NamedKey::ArrowDown, s | h, TermAction::JumpToPrompt(1)),
            binding(
                NamedKey::ArrowLeft,
                s | a,
                TermAction::FocusPaneDir {
                    horizontal: true,
                    forward: false,
                },
            ),
            binding(
                NamedKey::ArrowRight,
                s | a,
                TermAction::FocusPaneDir {
                    horizontal: true,
                    forward: true,
                },
            ),
            binding(
                NamedKey::ArrowUp,
                s | a,
                TermAction::FocusPaneDir {
                    horizontal: false,
                    forward: false,
                },
            ),
            binding(
                NamedKey::ArrowDown,
                s | a,
                TermAction::FocusPaneDir {
                    horizontal: false,
                    forward: true,
                },
            ),
            binding(
                NamedKey::ArrowLeft,
                s | c,
                TermAction::ResizePane {
                    horizontal: true,
                    forward: false,
                    px: 48,
                },
            ),
            binding(
                NamedKey::ArrowRight,
                s | c,
                TermAction::ResizePane {
                    horizontal: true,
                    forward: true,
                    px: 48,
                },
            ),
            binding(
                NamedKey::ArrowUp,
                s | c,
                TermAction::ResizePane {
                    horizontal: false,
                    forward: false,
                    px: 48,
                },
            ),
            binding(
                NamedKey::ArrowDown,
                s | c,
                TermAction::ResizePane {
                    horizontal: false,
                    forward: true,
                    px: 48,
                },
            ),
            binding(NamedKey::ArrowRight, s, TermAction::TypeText("\x05".into())),
            binding(NamedKey::ArrowLeft, s, TermAction::TypeText("\x01".into())),
            binding(NamedKey::Backspace, s, TermAction::TypeText("\x15".into())),
            binding(NamedKey::ArrowLeft, a, TermAction::EscSeq("b".into())),
            binding(NamedKey::ArrowRight, a, TermAction::EscSeq("f".into())),
            binding('1', s, TermAction::SelectTab(1)),
            binding('2', s, TermAction::SelectTab(2)),
            binding('3', s, TermAction::SelectTab(3)),
            binding('4', s, TermAction::SelectTab(4)),
            binding('5', s, TermAction::SelectTab(5)),
            binding('6', s, TermAction::SelectTab(6)),
            binding('7', s, TermAction::SelectTab(7)),
            binding('8', s, TermAction::SelectTab(8)),
            binding('9', s, TermAction::LastTab),
        ]
    });
    &BINDINGS
}

pub fn default_action_shortcut(action: &TermAction) -> Option<Shortcut> {
    platform_action_bindings()
        .iter()
        .chain(shared_action_bindings())
        .find(|binding| binding.action == *action)
        .map(|binding| binding.shortcut.clone())
}

/// The terminal-action chord test: Ctrl+Shift (Linux/Windows convention).
/// Returns the action name so the caller can run it instead of writing to
/// the PTY.
pub fn action_chord(key: &Key, mods: Modifiers) -> Option<TermAction> {
    if let Some(action) = lookup_action(
        shared_action_bindings(),
        key,
        mods,
        cfg!(target_os = "macos"),
    ) {
        return Some(action);
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(action) = macos_action_chord(key, mods) {
            return Some(action);
        }
        // An unbound Cmd+key is consumed silently — a terminal never
        // sends super-modified chords to the pty (bound or not, they
        // must never reach `key_to_bytes`).
        if mods.contains(Modifiers::META) {
            return Some(TermAction::Ignore);
        }
        None
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Shift-only PageUp/PageDown scroll one page — the xterm/alacritty
        // convention (Ctrl+Shift+PageUp/Down stays bound to tab cycling).
        if mods.contains(Modifiers::SHIFT)
            && !mods.contains(Modifiers::CONTROL)
            && !mods.contains(Modifiers::ALT)
            && !mods.contains(Modifiers::META)
        {
            return lookup_action(platform_action_bindings(), key, Modifiers::SHIFT, false);
        }
        // Ctrl+Shift+Alt+Arrow: directional pane focus — Ghostty's own
        // Linux goto_split default (`ctrl+shift+alt+left/right/up/down`).
        // PageUp/PageDown in the same chord are Ghostty's goto_split
        // previous/next.
        if mods.contains(Modifiers::CONTROL)
            && mods.contains(Modifiers::ALT)
            && mods.contains(Modifiers::SHIFT)
            && !mods.contains(Modifiers::META)
        {
            return lookup_action(
                platform_action_bindings(),
                key,
                Modifiers::CONTROL | Modifiers::ALT | Modifiers::SHIFT,
                false,
            );
        }
        // Ctrl+Alt+1..9: jump straight to the nth split. (Plain
        // Ctrl+Alt+Arrow was the directional-focus chord until r23 — KDE and
        // GNOME both grab it globally for workspace switching, so the app
        // never sees it; directional focus now sits on Ghostty's own Linux
        // default, Ctrl+Shift+Alt+Arrow.)
        if mods.contains(Modifiers::CONTROL)
            && mods.contains(Modifiers::ALT)
            && !mods.contains(Modifiers::SHIFT)
            && !mods.contains(Modifiers::META)
        {
            return lookup_action(
                platform_action_bindings(),
                key,
                Modifiers::CONTROL | Modifiers::ALT,
                false,
            );
        }
        // Alt+Shift+Arrow: move the divider beside the focused pane — the
        // keyboard half of split resizing (pointer half is the divider drag
        // handle). Ctrl+Alt+Shift+Arrow was the chord until r23 but GNOME
        // grabs it globally (move window to workspace); no common desktop
        // binds Alt+Shift+Arrow.
        if mods.contains(Modifiers::ALT)
            && mods.contains(Modifiers::SHIFT)
            && !mods.contains(Modifiers::CONTROL)
            && !mods.contains(Modifiers::META)
        {
            return lookup_action(
                platform_action_bindings(),
                key,
                Modifiers::ALT | Modifiers::SHIFT,
                false,
            );
        }
        if !(mods.contains(Modifiers::CONTROL) && mods.contains(Modifiers::SHIFT)) {
            return None;
        }
        lookup_action(
            platform_action_bindings(),
            key,
            Modifiers::CONTROL | Modifiers::SHIFT,
            false,
        )
    }
}

/// macOS terminal-action chords — the reference terminal's Darwin default
/// keybind table (ghostty `src/config/Config.zig` `Keybinds.init`, the
/// `isDarwin()` block at :7074 plus the shared binds whose modifier is
/// `ctrlOrSuper` → super: :6603 `,`, :6647 c/v, :6674 =/+/-/0, :6690 j,
/// :6983 digits, :6710 expand-selection, :6751 ctrl+tab cycling, and the
/// natural-editing text/esc binds at :7330). Cmd+key never produces pty
/// bytes: a matched chord runs its action and an unmatched super chord is
/// consumed upstream via [`TermAction::Ignore`].
#[cfg(target_os = "macos")]
fn macos_action_chord(key: &Key, mods: Modifiers) -> Option<TermAction> {
    lookup_action(platform_action_bindings(), key, mods, true)
}

/// Ctrl (no shift) digits select tabs 1-8; Alt+digits select tabs 1-9;
/// Ctrl+Tab / Ctrl+Shift+Tab cycle.
pub fn tab_chord(key: &Key, code: Code, mods: Modifiers) -> Option<TermAction> {
    // macOS covers tab cycling in its own table (`macos_action_chord`);
    // binding ctrl/alt+digit here would shadow the shell's word-jump
    // input, which the Darwin defaults deliberately leave unbound.
    #[cfg(target_os = "macos")]
    {
        let _ = (key, code, mods);
        None
    }
    #[cfg(not(target_os = "macos"))]
    {
        if mods.contains(Modifiers::CONTROL) && !mods.contains(Modifiers::SHIFT)
            || mods.contains(Modifiers::ALT) && !mods.contains(Modifiers::CONTROL)
        {
            let digit = match code {
                Code::Digit1 => Some(1),
                Code::Digit2 => Some(2),
                Code::Digit3 => Some(3),
                Code::Digit4 => Some(4),
                Code::Digit5 => Some(5),
                Code::Digit6 => Some(6),
                Code::Digit7 => Some(7),
                Code::Digit8 => Some(8),
                Code::Digit9 if mods.contains(Modifiers::ALT) => Some(9),
                _ => None,
            };
            if let Some(n) = digit {
                return Some(TermAction::SelectTab(n));
            }
        }
        let cycling_mods = if mods.contains(Modifiers::CONTROL | Modifiers::SHIFT) {
            Modifiers::CONTROL | Modifiers::SHIFT
        } else if mods.contains(Modifiers::CONTROL) {
            Modifiers::CONTROL
        } else {
            mods
        };
        if let Some(action) = lookup_action(platform_action_bindings(), key, cycling_mods, false)
            && matches!(action, TermAction::NextTab | TermAction::PrevTab)
        {
            return Some(action);
        }
        None
    }
}

/// Actions the app performs rather than forwarding as bytes.
#[derive(Debug, Clone, PartialEq)]
pub enum TermAction {
    Copy,
    Paste,
    NewTab,
    /// Ghostty `close_tab` — close the whole tab (every split in it).
    CloseTab,
    /// Ghostty `close_surface` — close only the focused pane; a sole
    /// pane takes its tab with it.
    CloseSurface,
    /// Open a whole new OS window (its own tabs and sessions).
    NewWindow,
    /// Flip the drop-down quick terminal open/closed (Ghostty
    /// `toggle_quick_terminal`; works as a window-local keybind and as
    /// the `global:` hotkey target).
    ToggleQuickTerminal,
    /// Jump back to the previously-selected tab (kitty `goto_tab -1`,
    /// tmux `last-window`).
    LastTab,
    /// Close every tab in this window (Ghostty `close_window`), with
    /// `confirm-close` prompts where configured.
    CloseWindow,
    /// Close every tab in this window (Ghostty `close_all_tabs`) —
    /// same effect as `close_window`, kept as the reference's name.
    CloseAllTabs,
    /// Close every tab except the selected one (Ghostty
    /// `close_other_tabs`), `confirm-close` prompts where configured.
    CloseOtherTabs,
    /// Ghostty `close_all_windows` — deprecated upstream ("no effect";
    /// the supported path is `all:close_window`). Bound by name and
    /// implemented as the same every-window sweep `all:close_window`
    /// performs.
    CloseAllWindows,
    /// Show/hide the tab strip on demand, overriding `tab-bar-min-tabs`
    /// until the next config reload (kitty `toggle_tab_bar`).
    ToggleTabBar,
    NextTab,
    PrevTab,
    SelectTab(usize),
    /// Ghostty `reset_font_size` — restore the configured `font-size`.
    FontReset,
    /// Ghostty `increase_font_size:pt` / `decrease_font_size:pt` —
    /// parameterized zoom steps (whole points).
    IncreaseFontSize(i32),
    DecreaseFontSize(i32),
    /// Ghostty `set_font_size:pt` — absolute font size (fractional).
    SetFontSize(f32),
    ClearScrollback,
    /// Ghostty `clear_screen` — erase the display AND the scrollback,
    /// cursor home (no mode reset).
    ClearScreen,
    /// Ghostty `reset` — RIS: reset modes + erase everything.
    Reset,
    Search,
    /// Ghostty `jump_to_prompt:N` — scroll the viewport N prompt marks
    /// (negative = previous).
    JumpToPrompt(i32),
    /// Select the whole viewport.
    SelectAll,
    /// Keyboard selection mode (Ghostty `start_selection`): anchor a
    /// selection at the cursor, arrows/Home/End/PageUp/PageDown move its
    /// end, Enter copies, Escape cancels; any other key exits.
    StartSelection,
    /// Scroll the viewport to the top of scrollback / the bottom.
    ScrollToTop,
    ScrollToBottom,
    /// Scroll one page up/down through scrollback (Shift+PageUp/Down).
    ScrollPageUp,
    ScrollPageDown,
    /// Ghostty `scroll_page_lines:N` — scroll N lines through
    /// scrollback (negative = up; Shift+Up/Down defaults use ±1).
    ScrollPageLines(i32),
    /// Ghostty `move_tab:N` — move the current tab N slots
    /// (negative = left; Ctrl+Shift+PageUp/Down defaults use ±1).
    MoveTab(i32),
    /// Focus the neighboring pane in a direction (Ctrl+Alt+Arrow).
    FocusPaneDir {
        /// Left/right (Row splits) when true, up/down (Column) when false.
        horizontal: bool,
        /// Right/down when true, left/up when false.
        forward: bool,
    },
    /// Move the divider beside the focused pane in a direction
    /// (Ctrl+Alt+Shift+Arrow / `keybind = resize_split:dir[,px]`) —
    /// keyboard split resize; `px` is the step in points.
    ResizePane {
        /// Left/right (Row splits) when true, up/down (Column) when false.
        horizontal: bool,
        /// Right/down when true, left/up when false.
        forward: bool,
        /// Main-axis points to move the divider by.
        px: i32,
    },
    /// URL hint mode: number the visible links, type digits + Enter to
    /// open one without the mouse.
    UrlHints,
    /// Paste-protection confirm: write the stashed clipboard text.
    /// Cancel is Escape on the surface, not an action — the snackbar
    /// has a single action slot.
    PasteConfirm,
    /// `confirm-close`: the user approved closing a pane/tab that has a
    /// program running in the foreground.
    CloseConfirm,
    /// Drag-and-drop: paste the dropped item's path (shell-quoted).
    DropText(String),
    /// Typed file drops keep path boundaries even when filenames contain spaces.
    DropFiles(Vec<String>),
    /// Copy the output of the last finished (or running) command — the
    /// rows between its OSC 133 `C` and `D` marks.
    CopyLastOutput,
    /// Context-menu \"Open Link\" — the URL the secondary press landed on
    /// travels with the action so the opener need not re-resolve the
    /// pointer cell.
    OpenUrl(String),
    /// Dump the scrollback + screen to a temp file and open it in
    /// `$VISUAL`/`$EDITOR` inside a new tab (kitty/WezTerm style).
    OpenScrollbackEditor,
    /// `write_screen_file` / `write_scrollback_file` /
    /// `write_selection_file` / `write_last_output_file` (Ghostty
    /// actions, `:action` parameter): dump the visible viewport / full
    /// scrollback+screen / current selection / last command output to a
    /// temp file, then `open` it in `$VISUAL`/`$EDITOR` in a new tab,
    /// `copy` the path to the clipboard, or `paste` the path to the pty.
    WriteScreenFile(FileSink),
    WriteScrollbackFile(FileSink),
    WriteSelectionFile(FileSink),
    WriteLastOutputFile(FileSink),
    /// Ghostty `open_config` — open the live config file in
    /// `$VISUAL`/`$EDITOR` in a new tab.
    OpenConfig,
    /// Ghostty `scroll_to_selection` — scroll the viewport so the
    /// selection's start is the top row.
    ScrollToSelection,
    /// Ghostty `clear_selection` — drop the current selection.
    ClearSelection,
    /// Ghostty `navigate_search:next|previous` — step the search match
    /// cursor forward / back.
    NavigateSearch(i32),
    /// Shut down all sessions and exit.
    Quit,
    /// Split the focused pane (new pane takes half its slot); Left/Up
    /// insert the new pane ahead of the target like Ghostty's
    /// `new_split:left`/`up`.
    SplitRight,
    /// Ghostty `new_split:auto` — pick Row/Column by the pane's
    /// aspect ratio (wider than tall → right, else down).
    SplitAuto,
    SplitDown,
    SplitLeft,
    SplitUp,
    /// Toggle: the focused pane fills the whole tab (tmux zoom).
    PaneZoom,
    /// Focus the nth pane in the tab (Ghostty `goto_split`,
    /// Ctrl+Alt+1..9 — Ctrl+Alt+Arrows already do directional focus).
    GotoSplit(usize),
    /// Re-read the config file and live-apply it (Ctrl+Shift+,).
    ReloadConfig,
    /// `clipboard-read = ask`: the program's OSC 52 read was approved
    /// by the Allow action or Enter.
    ClipboardReadConfirm,
    /// The OSC 52 read was refused — replies with an empty payload and
    /// drops the request (Ghostty's Deny button / Escape).
    ClipboardReadDeny,
    /// Cycle pane focus within the tab.
    FocusNextPane,
    FocusPrevPane,
    /// Toggle borderless fullscreen.
    Fullscreen,
    /// Ghostty `toggle_maximize` — maximize/restore the window.
    ToggleMaximize,
    /// Ghostty `toggle_window_float_on_top` — always-on-top
    /// stacking level.
    ToggleWindowFloatOnTop,
    /// Show or hide decorations on the current window.
    ToggleWindowDecorations,
    /// Toggle this window between opaque and its configured background opacity.
    ToggleBackgroundOpacity,
    /// Reset every split in the tab to equal shares
    /// (Ghostty `equalize_splits`, tmux `select-layout -E`).
    EqualizeSplits,
    /// Open the command palette.
    Palette,
    /// Open the settings page.
    Settings,
    /// Ghostty `keybind = chord=text:"…"` — write literal bytes to the
    /// focused pane as if typed.
    TypeText(String),
    /// Ghostty `keybind = chord=esc:"…"` — write `\e` + payload.
    EscSeq(String),
    /// Ghostty `keybind = chord=csi:"…"` — write `\e[` + payload.
    CsiSeq(String),
    /// Ghostty `paste_from_selection` — paste the PRIMARY selection
    /// (the clipboard `paste` covers `paste_from_clipboard`).
    PasteFromSelection,
    /// Ghostty `scroll_to_fraction` — jump the viewport to a fraction
    /// of the scrollback (0.0 = bottom / latest, 1.0 = top / oldest).
    ScrollToFraction(f64),
    /// Ghostty `scroll_to_row` — jump the viewport N rows back into
    /// scrollback (0 = bottom / latest screen row).
    ScrollToRow(usize),
    /// Ghostty `prompt_surface_title` — interactive rename of the
    /// focused session's title (until the next OSC 0/1/2 override).
    PromptTitle,
    /// Ghostty `prompt_tab_title` — interactive rename of the owning
    /// tab; a tab title persists across pane-focus changes.
    PromptTabTitle,
    /// Ghostty `set_surface_title:text` — set the session title
    /// directly (empty resets to the running program's title).
    SetSurfaceTitle(String),
    /// Ghostty `set_tab_title:text` — set a title on the owning tab
    /// that survives pane-focus changes; empty removes it.
    SetTabTitle(String),
    /// Ghostty `inspector` / `inspector:toggle` — overlay chip reporting
    /// the attributes of the cell under the terminal cursor.
    Inspector,
    /// `inspector:show` / `inspector:hide`.
    InspectorSet(bool),
    /// Ghostty `ignore` — consume the chord and do nothing (bytes do
    /// NOT reach the pty; `unbind` lets them through).
    Ignore,
    /// Ghostty `copy_url_to_clipboard` — copy the URL under the mouse
    /// cursor (OSC 8 hyperlink or detected) to the clipboard.
    CopyUrlToClipboard,
    /// Ghostty `copy_title_to_clipboard` — copy the focused session's
    /// title to the clipboard.
    CopyTitleToClipboard,
    /// Ghostty `toggle_mouse_visibility` — hide/show the pointer
    /// cursor; unlike `mouse-hide-while-typing` the hidden state
    /// survives pointer motion until toggled back.
    ToggleMouseVisibility,
    /// Ghostty `adjust_selection:dir` — move the active end of the
    /// keyboard selection; with no selection active it starts one at
    /// the cursor cell; `escape` clears the selection.
    AdjustSelection(AdjustSel),
    /// Ghostty `start_search` — open the search bar.
    StartSearch,
    /// Ghostty `end_search` — close the search bar and drop the query.
    EndSearch,
    /// Ghostty `search_selection` — open search seeded with the
    /// current selection text.
    SearchSelection,
    /// Ghostty `search:text` — set the search query (empty cancels).
    SearchFor(String),
    /// Ghostty `scroll_page_fractional:f` — scroll a fraction of the
    /// page (negative = up).
    ScrollPageFractional(f64),
    /// Ghostty `keybind = chord=sequence:a,b` — run every action in
    /// order on one chord press.
    Sequence(Vec<TermAction>),
    /// Ghostty `undo` — reopen the most recently closed tab with its
    /// scrollback restored (cell-faithful: the grid is serialized with
    /// its SGR attributes and replayed into the new surface before the
    /// shell's first prompt).
    Undo,
    /// Ghostty `redo` — re-close the surface `undo` just restored,
    /// pushing it back onto the undo stack so undo can restore again.
    /// Only fires while the restored tab still exists.
    Redo,
    /// Ghostty `toggle_mark` — mark/unmark the line the cursor sits on;
    /// `jump_to_mark` scrolls back to it. Invisible like the reference.
    ToggleMark,
    /// Ghostty `jump_to_mark:previous|next` — scroll the viewport to
    /// the nearest toggled mark (-1 / +1).
    JumpToMark(i32),
    /// Ghostty `cursor_key:<key>` — emit the escape sequence a physical
    /// cursor keypress would produce, honoring the terminal's DECCKM
    /// application-cursor mode.
    CursorKey(CursorKeyDir),
    /// Ghostty `hide_all_windows` — minimize every window of this
    /// instance (main, spawned, torn-off).
    HideAllWindows,
    /// Ghostty `move_tab_to_new_window` — detach the focused tab (with
    /// its live sessions) into a new OS window.
    MoveTabToNewWindow,
    /// Ghostty `prompt_window_title` — interactive rename of the window
    /// title (until the next title change).
    PromptWindowTitle,
    /// Ghostty `set_window_title:text` — set the window title directly.
    SetWindowTitle(String),
    /// Ghostty `toggle_mouse_reporting` — stop forwarding pointer events
    /// to the program; the app recaptures them (selection, scroll,
    /// context menu) until toggled again. Per-surface.
    ToggleMouseReporting,
    /// Ghostty `toggle_readonly` — the surface stops accepting input
    /// (key bytes, committed text, pastes, `text:`/`csi:`/`esc:`
    /// payloads) until toggled again; protocol replies and keybinds
    /// still work. Per-surface.
    ToggleReadonly,
    /// Ghostty `cancel` — dismiss the open transient (hints, keyboard
    /// selection, prompts, paste-confirm, search, palette).
    Cancel,
    /// Ghostty `open_url` — open the link under the pointer.
    OpenUrlUnderCursor,
    /// Ghostty `activate_key_table:name` — push a modal keybind layer;
    /// stays until `deactivate_key_table` (or all-clear).
    ActivateKeyTable(String),
    /// Ghostty `activate_key_table_once:name` — like
    /// `activate_key_table` but auto-pops when any binding fires.
    ActivateKeyTableOnce(String),
    /// Ghostty `deactivate_key_table` — pop the innermost key table.
    DeactivateKeyTable,
    /// Ghostty `deactivate_all_key_tables` — clear the whole stack.
    DeactivateAllKeyTables,
    /// Ghostty `end_key_sequence` — inside a `>` sequence, flush only
    /// the already-typed prefix keys to the program and exit the
    /// sequence (the completing key itself is consumed by the bind).
    EndKeySequence,
    /// Ghostty `crash` / `crash:<cause>` — crash the process on purpose
    /// (the reference uses it to exercise crash reporting).
    Crash,
}

/// Ghostty `cursor_key:<key>` key set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorKeyDir {
    /// Arrow keys — `\e[A`…`\e[D`, or SS3 `\eOA`…`\eOD` under DECCKM.
    Up,
    Down,
    Right,
    Left,
    /// `\e[H` / `\eOH` under DECCKM.
    Home,
    /// `\e[F` / `\eOF` under DECCKM.
    End,
    /// `\e[5~` (same sequence in both modes).
    PageUp,
    /// `\e[6~` (same sequence in both modes).
    PageDown,
}

/// Ghostty `adjust_selection:dir` direction set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdjustSel {
    /// One cell left / right.
    Left,
    Right,
    /// One row up / down.
    Up,
    Down,
    /// Line start / end.
    Home,
    End,
    /// One viewport up / down.
    PageUp,
    PageDown,
    /// Clear the selection and leave keyboard selection.
    Escape,
}

/// Ghostty's `:action` suffix for `write_*_file` — what to do with the
/// written temp file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileSink {
    /// Open it in `$VISUAL`/`$EDITOR` inside a new tab (default).
    Open,
    /// Copy the file path to the clipboard.
    Copy,
    /// Paste the file path into the terminal.
    Paste,
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn cmd_shift_p_is_palette() {
        assert_eq!(
            action_chord(
                &Key::Character("p".into()),
                Modifiers::META | Modifiers::SHIFT
            ),
            Some(TermAction::Palette)
        );
        assert_eq!(
            default_action_shortcut(&TermAction::NewTab)
                .expect("New Tab has a macOS default")
                .to_string(),
            "⌘T"
        );
        assert_eq!(
            default_action_shortcut(&TermAction::Palette)
                .expect("Palette has a macOS default")
                .to_string(),
            "⇧⌘P"
        );
        assert_eq!(
            action_chord(&Key::Named(NamedKey::F11), Modifiers::empty()),
            Some(TermAction::Fullscreen)
        );
    }

    #[test]
    fn macos_chords_keep_exact_modifiers_and_character_case() {
        assert_eq!(
            macos_action_chord(&Key::Character("T".into()), Modifiers::META),
            None
        );
        assert_eq!(
            action_chord(&Key::Character("T".into()), Modifiers::META),
            Some(TermAction::Ignore)
        );
        assert_eq!(
            macos_action_chord(
                &Key::Character("t".into()),
                Modifiers::META | Modifiers::SHIFT
            ),
            Some(TermAction::Undo)
        );
        assert_eq!(
            macos_action_chord(
                &Key::Character("t".into()),
                Modifiers::META | Modifiers::CAPS_LOCK
            ),
            None
        );
        assert_eq!(
            action_chord(&Key::Named(NamedKey::ArrowLeft), Modifiers::META),
            Some(TermAction::TypeText("\x01".into()))
        );
        assert_eq!(
            action_chord(&Key::Named(NamedKey::ArrowLeft), Modifiers::ALT),
            Some(TermAction::EscSeq("b".into()))
        );
    }

    #[test]
    fn dispatch_table_chord_bindings_round_trip() {
        for binding in platform_action_bindings()
            .iter()
            .chain(shared_action_bindings())
        {
            let key = binding.shortcut.key.to_key();
            let mut mods = Modifiers::empty();
            if binding.shortcut.modifiers.control() {
                mods |= Modifiers::CONTROL;
            }
            if binding.shortcut.modifiers.option() {
                mods |= Modifiers::ALT;
            }
            if binding.shortcut.modifiers.shift() {
                mods |= Modifiers::SHIFT;
            }
            if binding.shortcut.modifiers.command() {
                mods |= Modifiers::META;
            }
            let action =
                action_chord(&key, mods).or_else(|| tab_chord(&key, Code::Unidentified, mods));
            assert_eq!(action, Some(binding.action.clone()));
        }
    }

    #[test]
    fn ctrl_shift_p_falls_through() {
        assert_eq!(
            action_chord(
                &Key::Character("p".into()),
                Modifiers::CONTROL | Modifiers::SHIFT
            ),
            None
        );
    }
}

#[cfg(all(test, not(target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    fn dispatch_table_chord_bindings_round_trip() {
        for binding in platform_action_bindings()
            .iter()
            .chain(shared_action_bindings())
        {
            let key = binding.shortcut.key.to_key();
            let mut mods = Modifiers::empty();
            if binding.shortcut.modifiers.control() {
                mods |= Modifiers::CONTROL;
            }
            if binding.shortcut.modifiers.option() {
                mods |= Modifiers::ALT;
            }
            if binding.shortcut.modifiers.shift() {
                mods |= Modifiers::SHIFT;
            }
            let action =
                action_chord(&key, mods).or_else(|| tab_chord(&key, Code::Unidentified, mods));
            assert_eq!(action, Some(binding.action.clone()));
        }
    }

    #[test]
    fn canonical_chord_aliases_and_physical_digits_remain_stable() {
        assert_eq!(
            action_chord(
                &Key::Character("t".into()),
                Modifiers::CONTROL | Modifiers::SHIFT
            ),
            Some(TermAction::NewTab)
        );
        assert_eq!(
            action_chord(
                &Key::Character("t".into()),
                Modifiers::CONTROL | Modifiers::SHIFT | Modifiers::META
            ),
            Some(TermAction::NewTab)
        );
        assert_eq!(
            action_chord(
                &Key::Character("p".into()),
                Modifiers::CONTROL | Modifiers::SHIFT
            ),
            Some(TermAction::Palette)
        );
        assert_eq!(
            default_action_shortcut(&TermAction::NewTab)
                .expect("New Tab has a Linux default")
                .to_string(),
            "Ctrl+Shift+T"
        );
        assert_eq!(
            default_action_shortcut(&TermAction::Palette)
                .expect("Palette has a Linux default")
                .to_string(),
            "Ctrl+Shift+P"
        );
        assert_eq!(
            default_action_shortcut(&TermAction::Paste)
                .expect("Paste has a Linux default")
                .to_string(),
            "Ctrl+Shift+V"
        );
        assert_eq!(
            default_action_shortcut(&TermAction::PaneZoom)
                .expect("Pane zoom has a Linux default")
                .to_string(),
            "Ctrl+Shift+Z"
        );
        assert_eq!(
            default_action_shortcut(&TermAction::NextTab)
                .expect("Next Tab has a Linux default")
                .to_string(),
            "Ctrl+Tab"
        );
        assert_eq!(
            default_action_shortcut(&TermAction::PrevTab)
                .expect("Previous Tab has a Linux default")
                .to_string(),
            "Ctrl+Shift+Tab"
        );
        assert_eq!(
            action_chord(&Key::Named(NamedKey::F11), Modifiers::empty()),
            Some(TermAction::Fullscreen)
        );
        assert_eq!(
            action_chord(
                &Key::Character("=".into()),
                Modifiers::CONTROL | Modifiers::SHIFT
            ),
            Some(TermAction::IncreaseFontSize(1))
        );
        assert_eq!(
            action_chord(
                &Key::Character("K".into()),
                Modifiers::CONTROL | Modifiers::SHIFT
            ),
            Some(TermAction::ClearScrollback)
        );
        assert_eq!(
            action_chord(
                &Key::Character("K".into()),
                Modifiers::CONTROL | Modifiers::SHIFT
            ),
            None
        );
        assert_eq!(
            tab_chord(
                &Key::Character("1".into()),
                Code::Digit1,
                Modifiers::CONTROL
            ),
            Some(TermAction::SelectTab(1))
        );
        assert_eq!(
            tab_chord(&Key::Character("9".into()), Code::Digit9, Modifiers::ALT),
            Some(TermAction::SelectTab(9))
        );
    }
}
