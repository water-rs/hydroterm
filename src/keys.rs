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
    // F11 alone toggles fullscreen (GNOME Terminal / VTE convention).
    if matches!(key, Key::Named(NamedKey::F11)) && mods.is_empty() {
        return Some(TermAction::Fullscreen);
    }
    // Shift-only PageUp/PageDown scroll one page — the xterm/alacritty
    // convention (Ctrl+Shift+PageUp/Down stays bound to tab cycling).
    if mods.contains(Modifiers::SHIFT)
        && !mods.contains(Modifiers::CONTROL)
        && !mods.contains(Modifiers::ALT)
        && !mods.contains(Modifiers::META)
    {
        if let Key::Named(named) = key {
            return match named {
                NamedKey::PageUp => Some(TermAction::ScrollPageUp),
                NamedKey::PageDown => Some(TermAction::ScrollPageDown),
                // Shift+Up/Down scroll one line — xterm/kitty convention.
                NamedKey::ArrowUp => Some(TermAction::ScrollLineUp),
                NamedKey::ArrowDown => Some(TermAction::ScrollLineDown),
                // Shift+Insert pastes — the xterm/VTE convention.
                NamedKey::Insert => Some(TermAction::Paste),
                _ => None,
            };
        }
        return None;
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
        if let Key::Named(named) = key {
            return match named {
                NamedKey::ArrowLeft => Some(TermAction::FocusPaneDir {
                    horizontal: true,
                    forward: false,
                }),
                NamedKey::ArrowRight => Some(TermAction::FocusPaneDir {
                    horizontal: true,
                    forward: true,
                }),
                NamedKey::ArrowUp => Some(TermAction::FocusPaneDir {
                    horizontal: false,
                    forward: false,
                }),
                NamedKey::ArrowDown => Some(TermAction::FocusPaneDir {
                    horizontal: false,
                    forward: true,
                }),
                NamedKey::PageUp => Some(TermAction::FocusPrevPane),
                NamedKey::PageDown => Some(TermAction::FocusNextPane),
                _ => None,
            };
        }
        return None;
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
        if let Key::Named(_) = key {
            return None;
        }
        if let Key::Character(text) = key {
            return match text.as_str() {
                "1" => Some(TermAction::GotoSplit(0)),
                "2" => Some(TermAction::GotoSplit(1)),
                "3" => Some(TermAction::GotoSplit(2)),
                "4" => Some(TermAction::GotoSplit(3)),
                "5" => Some(TermAction::GotoSplit(4)),
                "6" => Some(TermAction::GotoSplit(5)),
                "7" => Some(TermAction::GotoSplit(6)),
                "8" => Some(TermAction::GotoSplit(7)),
                "9" => Some(TermAction::GotoSplit(8)),
                _ => None,
            };
        }
        return None;
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
        if let Key::Named(named) = key {
            return match named {
                NamedKey::ArrowLeft => Some(TermAction::ResizePane {
                    horizontal: true,
                    forward: false,
                    px: 48,
                }),
                NamedKey::ArrowRight => Some(TermAction::ResizePane {
                    horizontal: true,
                    forward: true,
                    px: 48,
                }),
                NamedKey::ArrowUp => Some(TermAction::ResizePane {
                    horizontal: false,
                    forward: false,
                    px: 48,
                }),
                NamedKey::ArrowDown => Some(TermAction::ResizePane {
                    horizontal: false,
                    forward: true,
                    px: 48,
                }),
                _ => None,
            };
        }
        return None;
    }
    if !(mods.contains(Modifiers::CONTROL) && mods.contains(Modifiers::SHIFT)) {
        return None;
    }
    if let Key::Named(named) = key {
        return Some(match named {
            NamedKey::ArrowUp => TermAction::PromptPrev,
            NamedKey::ArrowDown => TermAction::PromptNext,
            // Ctrl+Shift+PageUp/Down reorders tabs (Chrome/Firefox
            // convention) — plain Ctrl+PageUp/Down cycles them.
            NamedKey::PageUp => TermAction::MoveTabLeft,
            NamedKey::PageDown => TermAction::MoveTabRight,
            NamedKey::Home => TermAction::ScrollToTop,
            NamedKey::End => TermAction::ScrollToBottom,
            // Ctrl+Shift+Enter zooms the focused pane (kitty/tmux
            // convention) — same action as Ctrl+Shift+Z.
            NamedKey::Enter => TermAction::PaneZoom,
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
        "z" => TermAction::PaneZoom,
        "]" | "}" => TermAction::FocusNextPane,
        "[" | "{" => TermAction::FocusPrevPane,
        "+" | "=" => TermAction::FontBigger,
        "-" | "_" => TermAction::FontSmaller,
        "0" | ")" => TermAction::FontReset,
        "k" => TermAction::ClearScrollback,
        "o" => TermAction::CopyLastOutput,
        "u" => TermAction::UrlHints,
        "g" => TermAction::OpenScrollbackEditor,
        "f" => TermAction::Search,
        "p" => TermAction::Palette,
        "," | "<" => TermAction::ReloadConfig,
        "l" => TermAction::ClearScreen,
        _ => return None,
    })
}

/// Ctrl (no shift) digits select tabs 1-8; Alt+digits select tabs 1-9;
/// Ctrl+Tab / Ctrl+Shift+Tab cycle.
pub fn tab_chord(key: &Key, code: Code, mods: Modifiers) -> Option<TermAction> {
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
    if mods.contains(Modifiers::CONTROL) && !mods.contains(Modifiers::SHIFT) {
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermAction {
    Copy,
    Paste,
    NewTab,
    CloseTab,
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
    /// Show/hide the tab strip on demand, overriding `tab-bar-min-tabs`
    /// until the next config reload (kitty `toggle_tab_bar`).
    ToggleTabBar,
    NextTab,
    PrevTab,
    SelectTab(usize),
    FontBigger,
    FontSmaller,
    FontReset,
    /// Ghostty `increase_font_size:pt` / `decrease_font_size:pt` —
    /// parameterized zoom steps (whole points).
    IncreaseFontSize(i32),
    DecreaseFontSize(i32),
    ClearScrollback,
    /// Ghostty `clear_screen` — erase the display AND the scrollback,
    /// cursor home (no mode reset).
    ClearScreen,
    /// Ghostty `reset` — RIS: reset modes + erase everything.
    Reset,
    Search,
    /// Jump viewport to the previous/next OSC 133 prompt mark.
    PromptPrev,
    PromptNext,
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
    /// Scroll one line up/down through scrollback (Shift+Up/Down).
    ScrollLineUp,
    ScrollLineDown,
    /// Move the current tab one slot left/right (Ctrl+Shift+PageUp/Down).
    MoveTabLeft,
    MoveTabRight,
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
    /// Copy the output of the last finished (or running) command — the
    /// rows between its OSC 133 `C` and `D` marks.
    CopyLastOutput,
    /// Dump the scrollback + screen to a temp file and open it in
    /// `$VISUAL`/`$EDITOR` inside a new tab (kitty/WezTerm style).
    OpenScrollbackEditor,
    /// `write_screen_file` / `write_scrollback_file` /
    /// `write_selection_file` (Ghostty actions): dump the visible
    /// viewport / full scrollback+screen / current selection to a temp
    /// file and open it in `$VISUAL`/`$EDITOR` in a new tab.
    WriteScreenFile,
    WriteScrollbackFile,
    WriteSelectionFile,
    /// Step the search match cursor forward / back.
    SearchNext,
    SearchPrev,
    /// Shut down all sessions and exit.
    Quit,
    /// Split the focused pane (new pane takes half its slot); Left/Up
    /// insert the new pane ahead of the target like Ghostty's
    /// `new_split:left`/`up`.
    SplitRight,
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
    /// Reset every split in the tab to equal shares
    /// (Ghostty `equalize_splits`, tmux `select-layout -E`).
    EqualizeSplits,
    /// Open the command palette.
    Palette,
    /// Open the settings page.
    Settings,
}

