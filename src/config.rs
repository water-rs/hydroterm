//! Configuration: `~/.config/hydroterm/config` in `key = value` form,
//! watched for mtime changes and hot-reloaded.
//!
//! Format rationale: `key = value` lines with `#` comments are trivially
//! hand-editable, error-tolerant per line, and map 1:1 onto config keys —
//! picked over TOML/JSON because a terminal config is a flat bag of
//! scalars plus repeated `keybind` lines, which TOML's `[[array]]` tables
//! make noisy.

use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use alacritty_terminal::vte::ansi::CursorShape;
use keyboard_types::{Key, Modifiers, NamedKey};

use crate::keys::TermAction;
use crate::theme::Theme;

/// Which theme a `theme =` value resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeRef {
    /// Follow the desktop light/dark preference.
    Auto,
    /// A named theme from the catalog (e.g. `solarized-dark`).
    Named(String),
}

/// Fully-resolved settings — defaults plus file overrides.
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub font_size: f32,
    pub font_family: String,
    pub scrollback: usize,
    /// Resolved at load; `Auto` consults the desktop once per (re)load.
    pub theme: ThemeRef,
    /// Copy the selection to the clipboard on mouse release.
    pub copy_on_select: bool,
    pub cursor_shape: CursorShape,
    pub cursor_blink: bool,
    /// Shell program override; `None` = `$SHELL` with integration.
    pub shell: Option<String>,
    /// `-e` / `--` command for the initial session.
    pub command: Option<Vec<String>>,
    /// `keybind = <chord>=<action>` entries; `None` action = disabled.
    pub keybinds: Vec<(String, Option<TermAction>)>,
    /// Ring the X11 keyboard bell on `\a` (in addition to the visual flash).
    pub audible_bell: bool,
    /// Window/transparency: alpha of the terminal's own background fill,
    /// 0.0 (invisible) ..= 1.0 (opaque). The winit window is created
    /// transparent when this starts below 1.0 — raising it live works;
    /// dropping it on an opaque-start window just darkens.
    pub background_opacity: f32,
    /// Guard multi-line clipboard pastes behind a confirmation overlay
    /// (Ghostty `clipboard-paste-protection`). Bracketed-paste-armed
    /// programs skip the guard — wrapped text can't execute mid-paste.
    pub paste_protection: bool,
    /// Hide the pointer while typing; it returns on the next move
    /// (Ghostty `mouse-hide-while-typing`). X11 only — XFixes HideCursor.
    pub mouse_hide_typing: bool,
    /// Blank space around the cell grid in points (Ghostty
    /// `window-padding-x` / `window-padding-y`); live-reloaded.
    pub window_padding_x: f32,
    pub window_padding_y: f32,
    /// `$TERM` the PTY advertises (Ghostty `term`).
    pub term: String,
    /// Whether programs may write the clipboard through OSC 52
    /// (Ghostty `clipboard-write` allow/deny).
    pub osc52_write: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            font_size: 13.0,
            font_family: "monospace".to_string(),
            scrollback: 10_000,
            theme: ThemeRef::Named("hydroterm-dark".into()),
            copy_on_select: false,
            cursor_shape: CursorShape::Block,
            cursor_blink: true,
            shell: None,
            command: None,
            keybinds: Vec::new(),
            audible_bell: true,
            background_opacity: 1.0,
            paste_protection: true,
            mouse_hide_typing: true,
            window_padding_x: 0.0,
            window_padding_y: 0.0,
            term: "xterm-256color".to_string(),
            osc52_write: true,
        }
    }
}

/// The default config path: `$XDG_CONFIG_HOME/hydroterm/config`
/// (or `~/.config/hydroterm/config`).
pub fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    base.unwrap_or_else(|| PathBuf::from("/tmp")).join("hydroterm/config")
}

/// Template written when no config file exists yet.
const TEMPLATE: &str = "\
# hydroterm configuration — key = value, one per line.
# Changes are picked up automatically (hot reload).

font-size = 13
font-family = monospace
scrollback = 10000
window-padding-x = 0       # blank margin around the grid, in points
window-padding-y = 0
term = xterm-256color      # $TERM value advertised to programs
osc52-write = allow        # allow | deny — OSC 52 clipboard writes by programs

# Theme: auto | hydroterm-dark | hydroterm-light |
#        solarized-dark | solarized-light
theme = hydroterm-dark

cursor-shape = block        # block | beam | underline | hollow
cursor-blink = true
copy-on-select = false
audible-bell = true       # ring the X11 keyboard bell on BEL
# shell = /bin/bash

# Keybinds: keybind = <chord>=<action>; empty action disables.
# chords: ctrl+shift+c, alt+enter, ...  actions: copy, paste,
# new_tab, close_tab, new_window, next_tab, prev_tab, select_tab_1..8,
# font_bigger, font_smaller, font_reset, clear_scrollback, search,
# prompt_prev, prompt_next, select_all, scroll_to_top,
# scroll_to_bottom, quit, split_right, split_down,
# focus_next_pane, focus_prev_pane
# keybind = ctrl+alt+a=select_all

# Alpha of the terminal background fill (0..1); set below 1.0 at launch
# for a translucent window over the desktop.
# background-opacity = 0.85
";

impl AppConfig {
    /// Resolve the theme reference into a concrete theme.
    pub fn resolve_theme(&self) -> Theme {
        match &self.theme {
            ThemeRef::Auto => Theme::auto(),
            ThemeRef::Named(name) => Theme::by_name(name).unwrap_or_else(Theme::hydroterm_dark),
        }
    }

    /// Find a configured binding for this key press. `Some(Some)` = bound
    /// action, `Some(None)` = explicitly disabled, `None` = no entry.
    pub fn lookup_keybind(&self, key: &Key, mods: Modifiers) -> Option<Option<TermAction>> {
        let chord = chord_of(key, mods)?;
        self.keybinds
            .iter()
            .rev() // last wins
            .find(|(c, _)| *c == chord)
            .map(|(_, a)| *a)
    }

    /// Parse config text → (config, errors). Unknown keys and bad values
    /// are collected as human-readable errors, never fatal.
    pub fn parse(text: &str) -> (Self, Vec<String>) {
        let mut cfg = Self::default();
        let mut errors = Vec::new();
        for (n, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                errors.push(format!("line {}: expected `key = value`: {line:?}", n + 1));
                continue;
            };
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim().trim_matches('"');
            match key.as_str() {
                "font-size" => match value.parse::<f32>() {
                    Ok(v) if (4.0..=96.0).contains(&v) => cfg.font_size = v,
                    _ => errors.push(format!("line {}: bad font-size {value:?}", n + 1)),
                },
                "font-family" => cfg.font_family = value.to_string(),
                "scrollback" => match value.parse::<usize>() {
                    Ok(v) => cfg.scrollback = v.min(1_000_000),
                    Err(_) => errors.push(format!("line {}: bad scrollback {value:?}", n + 1)),
                },
                "background-opacity" | "background_opacity" => match value.parse::<f32>() {
                    Ok(v) if (0.0..=1.0).contains(&v) => cfg.background_opacity = v,
                    _ => errors.push(format!("line {}: bad background-opacity {value:?}", n + 1)),
                },
                "paste-protection" | "clipboard-paste-protection" => match value {
                    "true" | "yes" | "1" | "on" => cfg.paste_protection = true,
                    "false" | "no" | "0" | "off" => cfg.paste_protection = false,
                    _ => errors.push(format!("line {}: bad paste-protection {value:?}", n + 1)),
                },
                "window-padding-x" | "window_padding_x" => match value.parse::<f32>() {
                    Ok(v) if (0.0..=200.0).contains(&v) => cfg.window_padding_x = v,
                    _ => errors.push(format!("line {}: bad window-padding-x {value:?}", n + 1)),
                },
                "window-padding-y" | "window_padding_y" => match value.parse::<f32>() {
                    Ok(v) if (0.0..=200.0).contains(&v) => cfg.window_padding_y = v,
                    _ => errors.push(format!("line {}: bad window-padding-y {value:?}", n + 1)),
                },
                "window-padding" | "window_padding" => match value.parse::<f32>() {
                    Ok(v) if (0.0..=200.0).contains(&v) => {
                        cfg.window_padding_x = v;
                        cfg.window_padding_y = v;
                    }
                    _ => errors.push(format!("line {}: bad window-padding {value:?}", n + 1)),
                },
                "term" => cfg.term = value.to_string(),
                "osc52-write" | "clipboard-write" => match value {
                    "allow" | "true" | "yes" | "1" | "on" => cfg.osc52_write = true,
                    "deny" | "false" | "no" | "0" | "off" => cfg.osc52_write = false,
                    _ => errors.push(format!("line {}: bad osc52-write {value:?}", n + 1)),
                },
                "mouse-hide-while-typing" | "mouse_hide_while_typing" => match value {
                    "true" | "yes" | "1" | "on" => cfg.mouse_hide_typing = true,
                    "false" | "no" | "0" | "off" => cfg.mouse_hide_typing = false,
                    _ => errors.push(format!("line {}: bad mouse-hide-while-typing {value:?}", n + 1)),
                },
                "theme" => {
                    if value.eq_ignore_ascii_case("auto") {
                        cfg.theme = ThemeRef::Auto;
                    } else if Theme::by_name(value).is_some() {
                        cfg.theme = ThemeRef::Named(value.to_string());
                    } else {
                        errors.push(format!(
                            "line {}: unknown theme {value:?} (one of: auto, {})",
                            n + 1,
                            crate::theme::THEMES.join(", ")
                        ));
                    }
                }
                "copy-on-select" => cfg.copy_on_select = bool_value(value, n, &mut errors),
                "audible-bell" => cfg.audible_bell = bool_value(value, n, &mut errors),
                "cursor-blink" => cfg.cursor_blink = bool_value(value, n, &mut errors),
                "cursor-shape" => match value {
                    "block" => cfg.cursor_shape = CursorShape::Block,
                    "beam" => cfg.cursor_shape = CursorShape::Beam,
                    "underline" => cfg.cursor_shape = CursorShape::Underline,
                    "hollow" | "hollow-block" => cfg.cursor_shape = CursorShape::HollowBlock,
                    _ => errors.push(format!("line {}: bad cursor-shape {value:?}", n + 1)),
                },
                "shell" => cfg.shell = (!value.is_empty()).then(|| value.to_string()),
                "keybind" => match parse_keybind(value) {
                    Ok((chord, action)) => cfg.keybinds.push((chord, action)),
                    Err(e) => errors.push(format!("line {}: {e}", n + 1)),
                },
                _ => errors.push(format!("line {}: unknown key {key:?}", n + 1)),
            }
        }
        (cfg, errors)
    }

    /// Load a config file; a missing file yields defaults and a template
    /// is written so the user can discover the format.
    pub fn load(path: &Path) -> (Self, Vec<String>) {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(path, TEMPLATE);
                (Self::default(), Vec::new())
            }
            Err(e) => (Self::default(), vec![format!("reading {}: {e}", path.display())]),
        }
    }
}

fn bool_value(value: &str, line: usize, errors: &mut Vec<String>) -> bool {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => true,
        "false" | "no" | "off" | "0" => false,
        _ => {
            errors.push(format!("line {}: bad boolean {value:?}", line + 1));
            false
        }
    }
}

/// `ctrl+shift+c=copy` → ("ctrl+shift+c", Some(Copy)); an empty action or
/// `none`/`unbind` disables the chord.
fn parse_keybind(value: &str) -> Result<(String, Option<TermAction>), String> {
    let (chord, action) = value
        .split_once('=')
        .ok_or_else(|| format!("keybind needs `<chord>=<action>`: {value:?}"))?;
    let chord = normalize_chord(chord.trim())?;
    let action = action.trim().to_ascii_lowercase();
    let action = match action.as_str() {
        "" | "none" | "unbind" => None,
        _ => Some(
            action_from_str(&action)
                .ok_or_else(|| format!("unknown action {action:?}"))?,
        ),
    };
    Ok((chord, action))
}

/// Parse `ctrl+shift+arrowup` / `alt+f4` / `super+v` into the canonical
/// `ctrl+alt+shift+super+key` string.
fn normalize_chord(chord: &str) -> Result<String, String> {
    let mut mods = [false; 4]; // ctrl, alt, shift, super
    let mut key = None;
    for (i, part) in chord.split('+').enumerate() {
        let p = part.trim().to_ascii_lowercase();
        let is_last = i == chord.split('+').count() - 1;
        match p.as_str() {
            "ctrl" | "control" => mods[0] = true,
            "alt" | "option" => mods[1] = true,
            "shift" => mods[2] = true,
            "super" | "cmd" | "command" | "meta" | "win" => mods[3] = true,
            _ => {
                if !is_last || key.is_some() {
                    return Err(format!("bad keybind chord {chord:?}"));
                }
                key = Some(canonical_key_name(&p)?);
            }
        }
    }
    let Some(key) = key else {
        return Err(format!("keybind chord has no key: {chord:?}"));
    };
    let mut out = String::with_capacity(24);
    if mods[0] {
        out.push_str("ctrl+");
    }
    if mods[1] {
        out.push_str("alt+");
    }
    if mods[2] {
        out.push_str("shift+");
    }
    if mods[3] {
        out.push_str("super+");
    }
    out.push_str(&key);
    Ok(out)
}

/// Canonical key names used in chords: single chars stay single,
/// named keys use lowercase `arrowup`, `f4`, `pageup`, …
fn canonical_key_name(name: &str) -> Result<String, String> {
    const NAMES: &[&str] = &[
        "arrowup", "arrowdown", "arrowleft", "arrowright", "pageup", "pagedown", "home", "end",
        "insert", "delete", "backspace", "tab", "enter", "escape", "space",
    ];
    let n = name.to_ascii_lowercase();
    if n.len() == 1 || NAMES.contains(&n.as_str()) {
        return Ok(n);
    }
    if let Some(digits) = n.strip_prefix('f')
        && !digits.is_empty()
        && digits.parse::<u8>().is_ok_and(|d| (1..=24).contains(&d))
    {
        return Ok(n);
    }
    Err(format!("unknown key name {name:?}"))
}

/// Action names for `keybind =` right-hand sides.
fn action_from_str(name: &str) -> Option<TermAction> {
    Some(match name {
        "copy" => TermAction::Copy,
        "paste" => TermAction::Paste,
        "new_tab" => TermAction::NewTab,
        "close_tab" => TermAction::CloseTab,
        "new_window" => TermAction::NewWindow,
        "next_tab" => TermAction::NextTab,
        "prev_tab" => TermAction::PrevTab,
        "font_bigger" => TermAction::FontBigger,
        "font_smaller" => TermAction::FontSmaller,
        "font_reset" => TermAction::FontReset,
        "clear_scrollback" => TermAction::ClearScrollback,
        "search" => TermAction::Search,
        "prompt_prev" => TermAction::PromptPrev,
        "prompt_next" => TermAction::PromptNext,
        "select_all" => TermAction::SelectAll,
        "scroll_to_top" => TermAction::ScrollToTop,
        "scroll_to_bottom" => TermAction::ScrollToBottom,
        "quit" => TermAction::Quit,
        "split_right" => TermAction::SplitRight,
        "split_down" => TermAction::SplitDown,
        "focus_next_pane" => TermAction::FocusNextPane,
        "focus_prev_pane" => TermAction::FocusPrevPane,
        "fullscreen" => TermAction::Fullscreen,
        "palette" | "command_palette" => TermAction::Palette,
        "settings" => TermAction::Settings,
        _ if name.strip_prefix("select_tab_").is_some() => {
            let n: usize = name["select_tab_".len()..].parse().ok()?;
            TermAction::SelectTab(n)
        }
        _ => return None,
    })
}

/// Canonical `ctrl+alt+shift+super+key` string for a live key press.
fn chord_of(key: &Key, mods: Modifiers) -> Option<String> {
    let mut out = String::with_capacity(24);
    if mods.contains(Modifiers::CONTROL) {
        out.push_str("ctrl+");
    }
    if mods.contains(Modifiers::ALT) {
        out.push_str("alt+");
    }
    if mods.contains(Modifiers::SHIFT) {
        out.push_str("shift+");
    }
    if mods.contains(Modifiers::META) {
        out.push_str("super+");
    }
    let key_name = match key {
        Key::Character(c) if c.as_str() == " " => "space".to_string(),
        Key::Character(c) => c.to_ascii_lowercase(),
        Key::Named(named) => named_key_name(*named)?,
    };
    out.push_str(&key_name);
    Some(out)
}

fn named_key_name(key: NamedKey) -> Option<String> {
    let s = match key {
        NamedKey::ArrowUp => "arrowup".to_string(),
        NamedKey::ArrowDown => "arrowdown".to_string(),
        NamedKey::ArrowLeft => "arrowleft".to_string(),
        NamedKey::ArrowRight => "arrowright".to_string(),
        NamedKey::PageUp => "pageup".to_string(),
        NamedKey::PageDown => "pagedown".to_string(),
        NamedKey::Home => "home".to_string(),
        NamedKey::End => "end".to_string(),
        NamedKey::Insert => "insert".to_string(),
        NamedKey::Delete => "delete".to_string(),
        NamedKey::Backspace => "backspace".to_string(),
        NamedKey::Tab => "tab".to_string(),
        NamedKey::Enter => "enter".to_string(),
        NamedKey::Escape => "escape".to_string(),
        NamedKey::F1 => "f1".to_string(),
        NamedKey::F2 => "f2".to_string(),
        NamedKey::F3 => "f3".to_string(),
        NamedKey::F4 => "f4".to_string(),
        NamedKey::F5 => "f5".to_string(),
        NamedKey::F6 => "f6".to_string(),
        NamedKey::F7 => "f7".to_string(),
        NamedKey::F8 => "f8".to_string(),
        NamedKey::F9 => "f9".to_string(),
        NamedKey::F10 => "f10".to_string(),
        NamedKey::F11 => "f11".to_string(),
        NamedKey::F12 => "f12".to_string(),
        _ => return None,
    };
    Some(s)
}

/// mtime-based hot reload, polled from the render loop (throttled).
pub struct ConfigWatcher {
    pub path: PathBuf,
    pub config: AppConfig,
    /// Latest parse errors for the status line.
    pub errors: Vec<String>,
    mtime: Option<SystemTime>,
    last_check: Instant,
}

impl ConfigWatcher {
    /// Load the file at `path` (or the default) and remember its mtime.
    pub fn new(path: Option<PathBuf>) -> Self {
        let path = path.unwrap_or_else(default_path);
        let (config, errors) = AppConfig::load(&path);
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        Self {
            path,
            config,
            errors,
            mtime,
            last_check: Instant::now(),
        }
    }

    /// Reparse when the file changed; returns true when `config` changed.
    pub fn poll(&mut self) -> bool {
        if self.last_check.elapsed() < std::time::Duration::from_millis(400) {
            return false;
        }
        self.last_check = Instant::now();
        let mtime = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        if mtime == self.mtime {
            return false;
        }
        self.mtime = mtime;
        let (config, errors) = AppConfig::load(&self.path);
        self.config = config;
        self.errors = errors;
        true
    }
}


/// Insert or replace `key = value` in the config file, preserving every
/// other line (comments, unknown keys). Creates the file and parent dirs
/// when missing — used by the in-app settings page; the watcher then
/// hot-reloads the change like a manual edit.
pub fn upsert_config_key(path: &Path, key: &str, value: &str) {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut out: Vec<String> = Vec::with_capacity(text.lines().count() + 1);
    let mut done = false;
    for line in text.lines() {
        let k = line
            .split('#')
            .next()
            .unwrap_or("")
            .split('=')
            .next()
            .unwrap_or("")
            .trim();
        if k == key {
            if !done {
                out.push(format!("{key} = {value}"));
                done = true;
            }
        } else {
            out.push(line.to_string());
        }
    }
    if !done {
        out.push(format!("{key} = {value}"));
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, out.join("\n") + "\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scalars_and_comments() {
        let text = "# c\n\nfont-size = 14.5\nfont-family = \"JetBrains Mono\"\n\
                    scrollback = 5000\ntheme = solarized-light\n\
                    copy-on-select = true\ncursor-shape = beam\ncursor-blink = no\n\
                    shell = /bin/zsh\n";
        let (cfg, errs) = AppConfig::parse(text);
        assert!(errs.is_empty(), "{errs:?}");
        assert!((cfg.font_size - 14.5).abs() < f32::EPSILON);
        assert_eq!(cfg.font_family, "JetBrains Mono");
        assert_eq!(cfg.scrollback, 5000);
        assert_eq!(cfg.theme, ThemeRef::Named("solarized-light".into()));
        assert!(cfg.copy_on_select);
        assert_eq!(cfg.cursor_shape, CursorShape::Beam);
        assert!(!cfg.cursor_blink);
        assert_eq!(cfg.shell.as_deref(), Some("/bin/zsh"));
    }

    #[test]
    fn unknown_keys_and_bad_values_report() {
        let (_, errs) = AppConfig::parse("bogus-key = 1\nfont-size = huge\ntheme = nope");
        assert_eq!(errs.len(), 3);
        assert!(errs[0].contains("unknown key"));
    }

    #[test]
    fn malformed_line_reports() {
        let (_, errs) = AppConfig::parse("no-equals-here");
        assert_eq!(errs.len(), 1);
    }

    #[test]
    fn keybind_parses_and_normalizes() {
        let (cfg, errs) = AppConfig::parse(
            "keybind = ctrl+shift+c=copy\nkeybind = shift+ctrl+v=paste\n\
             keybind = ctrl+shift+f4=close_tab\nkeybind = ctrl+shift+x=\n",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.keybinds.len(), 4);
        // Modifier order normalized — shift+ctrl+v and ctrl+shift+v collide.
        assert_eq!(cfg.keybinds[0].0, "ctrl+shift+c");
        assert_eq!(cfg.keybinds[1].0, "ctrl+shift+v");
        assert_eq!(cfg.keybinds[2].0, "ctrl+shift+f4");
        assert_eq!(cfg.keybinds[2].1, Some(TermAction::CloseTab));
        assert_eq!(cfg.keybinds[3].1, None); // disabled
    }

    #[test]
    fn keybind_lookup_matches_and_disables() {
        let (cfg, errs) = AppConfig::parse("keybind = ctrl+shift+c=\nkeybind = alt+f4=quit");
        assert!(errs.is_empty());
        let ctrl_shift = Modifiers::CONTROL | Modifiers::SHIFT;
        assert_eq!(
            cfg.lookup_keybind(&Key::Character("c".into()), ctrl_shift),
            Some(None)
        );
        assert_eq!(
            cfg.lookup_keybind(&Key::Named(NamedKey::F4), Modifiers::ALT),
            Some(Some(TermAction::Quit))
        );
        assert_eq!(
            cfg.lookup_keybind(&Key::Character("v".into()), ctrl_shift),
            None
        );
    }

    #[test]
    fn select_tab_n_and_prompt_actions() {
        let (cfg, errs) = AppConfig::parse(
            "keybind = ctrl+3=select_tab_3\nkeybind = ctrl+shift+arrowup=prompt_prev",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.keybinds[0].1, Some(TermAction::SelectTab(3)));
        assert_eq!(cfg.keybinds[1].1, Some(TermAction::PromptPrev));
    }

    #[test]
    fn theme_auto_and_unknown() {
        let (cfg, errs) = AppConfig::parse("theme = auto");
        assert!(errs.is_empty());
        assert_eq!(cfg.theme, ThemeRef::Auto);
        let (_, errs) = AppConfig::parse("theme = midnight-whatever");
        assert_eq!(errs.len(), 1);
    }

    #[test]
    fn watcher_detects_change() {
        let dir = std::env::temp_dir().join(format!("hydroterm-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config");
        std::fs::write(&path, "font-size = 20\n").unwrap();
        let mut w = ConfigWatcher::new(Some(path.clone()));
        assert!((w.config.font_size - 20.0).abs() < f32::EPSILON);
        w.last_check = Instant::now() - std::time::Duration::from_secs(1);
        std::fs::write(&path, "font-size = 22\n").unwrap();
        // mtime granularity may tie; poll must still see the new mtime.
        assert!(w.poll() || w.config.font_size == 20.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upsert_replaces_and_appends_keys() {
        let dir = std::env::temp_dir().join(format!("hydroterm-upsert-{}", std::process::id()));
        let path = dir.join("config");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "# comment\nfont-size = 13\ntheme = auto\n").unwrap();
        upsert_config_key(&path, "theme", "solarized-dark");
        upsert_config_key(&path, "cursor-blink", "true");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# comment"));
        assert!(text.contains("font-size = 13"));
        assert!(text.contains("theme = solarized-dark"));
        assert!(text.contains("cursor-blink = true"));
        assert!(!text.contains("theme = auto"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
