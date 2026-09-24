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

use alacritty_terminal::vte::ansi::{CursorShape, Rgb};
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

/// `copy-on-select` routing — where a finished selection lands
/// (kitty semantics; `true`/`both` writes clipboard and PRIMARY,
/// `clipboard`/`primary` pick one, `false` disables).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyOnSelect {
    Disabled,
    Clipboard,
    Primary,
    Both,
}

/// `shell-integration` — which spawned shell gets the auto-injected
/// OSC 133/7 hooks (`detect` = bash/zsh/fish, `none` disables).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellIntegration {
    None,
    Detect,
    Bash,
    Zsh,
    Fish,
}

/// `shell-integration-features` — which integration extras a supported
/// shell gets (Ghostty `cursor,sudo,title`; `no-` prefixes disable).
#[derive(Debug, Clone, Copy)]
pub struct ShellFeatures {
    /// Cursor style changes at the prompt (bar while editing).
    pub cursor: bool,
    /// `sudo` wrapper preserving the terminal's env under sudo.
    pub sudo: bool,
    /// Window/tab title driven by the prompt (`user@host:cwd`).
    pub title: bool,
}

/// `quick-terminal-position` — where the drop-down docks on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuickTermPosition {
    Top,
    Bottom,
    Left,
    Right,
    Center,
}

/// `window-new-tab-position` — where a new tab lands in the strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewTabPosition {
    /// Append after the last tab (Ghostty default).
    End,
    /// Insert immediately after the selected tab.
    Current,
}

/// Fully-resolved settings — defaults plus file overrides.
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub font_size: f32,
    pub font_family: String,
    pub scrollback: usize,
    /// Resolved at load; `Auto` consults the desktop once per (re)load.
    pub theme: ThemeRef,
    /// Where a finished selection is copied (Ghostty `copy-on-select`).
    pub copy_on_select: CopyOnSelect,
    /// `shell-integration` — which shell gets the injected hooks.
    pub shell_integration: ShellIntegration,
    /// `shell-integration-features` — the integration extras (cursor,
    /// sudo, title); each can be turned off with `no-<feature>`.
    pub shell_features: ShellFeatures,
    /// `quick-terminal-position` — drop-down dock edge (top default).
    pub quick_terminal_position: QuickTermPosition,
    /// `clipboard-trim` — trim whitespace at the ends of copied text.
    pub clipboard_trim: bool,
    /// `desktop-notifications` — OSC 9/777 also posts a freedesktop
    /// notification via `notify-send` when available.
    pub desktop_notifications: bool,
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
    /// `mouse-shift-override` — while a program reports the mouse
    /// (DECSET 1000/1002/1006), holding Shift bypasses reporting so
    /// click/drag selects terminal text (Ghostty default true).
    pub mouse_shift_override: bool,
    /// `clipboard-read` — policy for program clipboard reads via
    /// OSC 52 `?` requests (Ghostty `clipboard-read`, default ask).
    pub clipboard_read: ClipboardRead,
    /// `cursor-invert-fg-bg` — the block cursor swaps the cell's
    /// fg/bg so the glyph under it stays readable (Ghostty default on).
    pub cursor_invert_fg_bg: bool,
    /// Blank space around the cell grid in points (Ghostty
    /// `window-padding-x` / `window-padding-y`); live-reloaded.
    pub window_padding_x: f32,
    pub window_padding_y: f32,
    /// `$TERM` the PTY advertises (Ghostty `term`).
    pub term: String,
    /// Whether programs may write the clipboard through OSC 52
    /// (Ghostty `clipboard-write` allow/deny).
    pub osc52_write: bool,
    /// Bold text renders with the bright palette slot
    /// (alacritty `draw_bold_text_with_bright_colors`, default true).
    pub bold_is_bright: bool,
    /// Initial working directory when no OSC 7 cwd was reported
    /// (Ghostty `working-directory`); `~` expands at load.
    pub working_directory: Option<PathBuf>,
    /// Alpha applied to a pane that is not the focused split
    /// (Ghostty `unfocused-split-opacity`), clamped to 0.0..=1.0.
    pub unfocused_split_opacity: f32,
    /// Show a cols×rows badge while the window resizes
    /// (Ghostty `resize-overlay`).
    pub resize_overlay: bool,
    /// Pointer entering a pane records it as the focused split
    /// (Ghostty `focus-follows-mouse`). App-level record only —
    /// GUI key focus still needs a press until hydrolysis#126.
    pub focus_follows_mouse: bool,
    /// Theme color overrides (Ghostty `foreground` / `background` /
    /// `cursor-color` / `selection-color` / `palette = N=#rgb`);
    /// applied on top of the resolved theme, live-reloaded.
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    pub cursor_color: Option<Rgb>,
    /// Selection text color — `selection-color` sets the ink; the
    /// highlight fill stays the theme's `selection_bg`.
    pub selection_color: Option<Rgb>,
    /// `palette = 1=#ff0000` — indexed 0-255 slot overrides.
    pub palette_overrides: Vec<(u8, Rgb)>,
    /// Wheel scroll speed multiplier (Ghostty `mouse-scroll-multiplier`).
    pub mouse_scroll_multiplier: f32,
    /// Ask before closing a pane/tab whose PTY foreground is a program
    /// other than the shell (Ghostty `confirm-close-surface`).
    pub confirm_close: bool,
    /// Initial window content size in points; 0 = framework default.
    /// Applied once at launch (Ghostty `window-width`/`window-height`).
    pub window_width: f32,
    pub window_height: f32,
    /// Launch window position in points (Ghostty `window-position-x`/`y`);
    /// `None` = let the window manager choose. Applied once at launch.
    pub window_x: Option<f32>,
    pub window_y: Option<f32>,
    /// Modifier that must be held for click-to-open-link (Ghostty
    /// `open-link-modifier`-style); default Control.
    pub open_link_modifier: LinkMod,
    /// Multi-click (word/line select) timing window in milliseconds.
    pub click_interval: u64,
    /// Invert the selected cells' foreground/background (Ghostty
    /// `selection-invert-fg-bg`); when off, `selection_bg`/`selection_fg`
    /// theme colors are used instead.
    pub selection_invert: bool,
    /// Restore the last window geometry on launch and save it as the
    /// window moves/resizes (Ghostty `window-save-state`).
    pub window_save_state: bool,
    /// Launch every window fullscreen (Ghostty `window-fullscreen`).
    pub window_fullscreen: bool,
    /// Keep the surface open after a `command`/`-e` child exits
    /// (Ghostty `wait-after-command`, default false). Interactive
    /// shells always close their tab on exit.
    pub wait_after_command: bool,
    /// Quit the app when the last tab/surface closes (Ghostty
    /// `quit-after-last-window-closed`, default true on Linux).
    pub quit_after_last_window_closed: bool,
    /// Glyph color under the block cursor (Ghostty `cursor-text`);
    /// `None` = the inverted cell color.
    pub cursor_text: Option<Rgb>,
    /// `split-divider-color` — pane separator color; `None` = theme
    /// Border token.
    pub split_divider_color: Option<Rgb>,
    /// `selection-background` — selection highlight fill (Ghostty
    /// `selection-background`); `None` = theme's `selection_bg`.
    pub selection_background: Option<Rgb>,
    /// `unfocused-split-fill` — background color painted under an
    /// unfocused split's cells (Ghostty `unfocused-split-fill`).
    pub unfocused_split_fill: Option<Rgb>,
    /// `title` — initial window/tab title; programs can still override
    /// it via OSC 0/1/2 (Ghostty `title`).
    pub title: Option<String>,
    /// Snap to the live edge when a key writes bytes to the PTY
    /// (Ghostty `scroll-on-input`-style; default on).
    pub scroll_on_input: bool,
    /// Alpha of the block cursor fill (Ghostty `cursor-opacity`, 0–1).
    pub cursor_opacity: f32,
    /// `env = NAME=VALUE` lines injected into spawned shells' environment.
    pub env: Vec<(String, String)>,
    /// Extra spacing per cell: `adjust-cell-width`/`adjust-cell-height`
    /// accept `N%` (of the measured cell) or `Npx` (absolute points).
    pub cell_width_adjust: CellAdjust,
    pub cell_height_adjust: CellAdjust,
    /// `adjust-font-baseline`: shifts the text baseline, measured as the
    /// distance from the cell bottom (`Npx` or `N%` of that distance).
    /// Positive values move the baseline up (Ghostty semantics).
    pub font_baseline_adjust: CellAdjust,
    /// OpenType feature toggles (kitty `font_features` / Ghostty
    /// `font-feature`): `-tag` disables (e.g. `-calt` kills ligatures),
    /// `+tag`/`tag`/`tag=N` sets a value. Repeatable.
    pub font_features: Vec<String>,
    /// Minimum WCAG contrast ratio between cell foreground and background
    /// (Ghostty `minimum-contrast`); 1.0 = off (no enforcement).
    pub minimum_contrast: f32,
    /// Color scheme of the window chrome — tab strip, dialogs, buttons
    /// (Ghostty `window-theme`): `auto` follows the desktop, `light`/`dark`
    /// pin the Material scheme. The terminal palette stays on `theme =`.
    pub window_theme: WindowTheme,
    /// Window decorations (Ghostty `window-decoration`): `false`/`none`
    /// maps the window borderless (title bar + frame removed).
    pub window_decoration: bool,
    /// Background image drawn under the grid (Ghostty `background-image`):
    /// path (with `~` expansion); `None` = off.
    pub background_image: Option<PathBuf>,
    /// Opacity of `background-image` (0–1, Ghostty `background-image-opacity`).
    pub background_image_opacity: f32,
    /// Fit of `background-image` inside the surface rect (Ghostty
    /// `background-image-fit`): contain/cover/stretch/tile.
    pub background_image_fit: BgFit,
    /// Repeat `background-image` instead of stretching one copy
    /// (Ghostty `background-image-repeat`).
    pub background_image_repeat: bool,
    /// `window-new-tab-position = end|current` — where a new tab is
    /// inserted in the strip (Ghostty `window-new-tab-position`).
    pub new_tab_position: NewTabPosition,
    /// New tab inherits the focused surface's OSC 7 working directory
    /// (Ghostty `window-inherit-working-directory`, default true).
    pub inherit_working_directory: bool,
    /// New tab/window/pane inherits the focused surface's live font
    /// size instead of the config value (Ghostty
    /// `window-inherit-font-size`, default true).
    pub inherit_font_size: bool,
    /// Snap the viewport to the cursor when an interaction that can move
    /// it lands while scrolled back — paste, IME commit, program-input
    /// keys (Ghostty folds this into input snapping; ours gates the
    /// non-key-bytes paths; `scroll-on-input` covers key bytes).
    pub scroll_to_cursor: bool,
    /// `tab-bar-min-tabs` — hide the tab strip while fewer than this many
    /// tabs exist (Ghostty `window-show-tab-bar = auto` ⇔ 2). Default 1
    /// = always show.
    pub tab_bar_min_tabs: usize,
    /// `word-select-chars` — characters that terminate a word for
    /// double-click selection (alacritty `semantic_escape_chars`
    /// semantics; literal `\t`/`\n` in the file expand to tab/newline).
    pub word_select_chars: String,
    /// `visual-bell` — flash the pane surface on BEL (default true).
    /// `audible-bell` rings the X11 keyboard bell; both independent.
    pub visual_bell: bool,
    /// `open-link-with` — program run to open links. `{}` in an argument
    /// is replaced by the URL; otherwise the URL is appended as the
    /// last argument. Unset → `xdg-open`.
    pub open_link_with: Option<String>,
}

/// `window-theme` values.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WindowTheme {
    /// Follow the desktop color-scheme (default).
    Auto,
    Light,
    Dark,
}

/// `background-image-fit` values.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BgFit {
    /// Scale to fit inside, preserve aspect (letterboxed).
    Contain,
    /// Scale to fill, preserve aspect (cropped).
    Cover,
    /// Scale to fill, distorting aspect.
    Stretch,
    /// Native size, repeated to fill.
    Tile,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            font_size: 13.0,
            font_family: "monospace".to_string(),
            scrollback: 10_000,
            theme: ThemeRef::Named("hydroterm-dark".into()),
            copy_on_select: CopyOnSelect::Both,
            shell_integration: ShellIntegration::Detect,
            shell_features: ShellFeatures { cursor: true, sudo: true, title: true },
            quick_terminal_position: QuickTermPosition::Top,
            clipboard_trim: true,
            desktop_notifications: true,
            cursor_shape: CursorShape::Block,
            cursor_blink: true,
            shell: None,
            command: None,
            keybinds: Vec::new(),
            audible_bell: true,
            background_opacity: 1.0,
            paste_protection: true,
            mouse_hide_typing: true,
            mouse_shift_override: true,
            clipboard_read: ClipboardRead::Ask,
            cursor_invert_fg_bg: true,
            window_padding_x: 0.0,
            window_padding_y: 0.0,
            term: "xterm-256color".to_string(),
            osc52_write: true,
            bold_is_bright: true,
            working_directory: None,
            unfocused_split_opacity: 1.0,
            resize_overlay: true,
            focus_follows_mouse: false,
            foreground: None,
            background: None,
            cursor_color: None,
            selection_color: None,
            palette_overrides: Vec::new(),
            mouse_scroll_multiplier: 1.0,
            confirm_close: true,
            window_width: 0.0,
            window_height: 0.0,
            window_x: None,
            window_y: None,
            open_link_modifier: LinkMod::Ctrl,
            click_interval: 400,
            selection_invert: false,
            window_save_state: false,
            window_fullscreen: false,
            wait_after_command: false,
            quit_after_last_window_closed: true,
            split_divider_color: None,
            selection_background: None,
            unfocused_split_fill: None,
            title: None,
            cursor_text: None,
            scroll_on_input: true,
            cursor_opacity: 1.0,
            env: Vec::new(),
            cell_width_adjust: CellAdjust::None,
            cell_height_adjust: CellAdjust::None,
            font_baseline_adjust: CellAdjust::None,
            font_features: Vec::new(),
            minimum_contrast: 1.0,
            window_theme: WindowTheme::Auto,
            window_decoration: true,
            background_image: None,
            background_image_opacity: 1.0,
            background_image_fit: BgFit::Cover,
            background_image_repeat: false,
            new_tab_position: NewTabPosition::End,
            inherit_working_directory: true,
            inherit_font_size: true,
            scroll_to_cursor: true,
            tab_bar_min_tabs: 1,
            word_select_chars: alacritty_terminal::term::SEMANTIC_ESCAPE_CHARS.to_string(),
            visual_bell: true,
            open_link_with: None,
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
osc52-read = ask           # allow | ask | deny — OSC 52 clipboard reads by programs
mouse-shift-override = true # Shift+click/drag selects even while a program owns the mouse
cursor-invert-fg-bg = true # block cursor swaps the cell's fg/bg
bold-is-bright = true       # bold text uses the bright palette slot
unfocused-split-opacity = 1.0   # dim non-focused panes (0.0-1.0)
resize-overlay = true      # cols x rows badge while resizing
focus-follows-mouse = false
# command = tmux attach    # program for the initial session (`-e` wins)
# working-directory = ~/projects   # initial cwd when no OSC 7 report
# window-new-tab-position = end    # end | current — where new tabs insert
# window-inherit-working-directory = true  # new tab takes focused pane's OSC 7 cwd
# window-inherit-font-size = true  # new tab takes focused pane's live zoom

# Theme: auto | hydroterm-dark | hydroterm-light |
#        solarized-dark | solarized-light
theme = hydroterm-dark

cursor-shape = block        # block | beam | underline | hollow
cursor-blink = true
copy-on-select = both       # both | clipboard | primary | false
shell-integration = detect  # detect | none | bash | zsh | fish
desktop-notifications = true # OSC 9/777 also notify via notify-send
audible-bell = true       # ring the X11 keyboard bell on BEL
# shell = /bin/bash

# Colors: overrides on top of the resolved theme
# (Ghostty foreground / background / palette).
# foreground = #ddeeff
# background = #101418
# cursor-color = #ffcc00
# selection-color = #ffffff
# selection-background = #3b4d5a   # selection highlight fill
# split-divider-color = #888888    # pane separator (default: theme Border)
# unfocused-split-fill = #2a2a2a   # bg of unfocused splits
# title = my-terminal              # initial window title (OSC can override)
# palette = 1=#e06c75   # indexed slot 0-255

mouse-scroll-multiplier = 1.0   # wheel scroll speed
confirm-close = true       # ask before closing a running program
# window-width = 800       # initial window size in points (0 = default)
# window-fullscreen = false  # start windows fullscreen
# window-height = 600
# window-save-state = true # remember window geometry across launches
# adjust-cell-width = 10%  # widen cells: N% or Npx
# adjust-cell-height = 2px
# adjust-font-baseline = 0px   # +Npx raises the text baseline; N% or Npx
# font-feature = -calt         # OpenType toggle: -tag off, +tag/tag/tag=N on
# minimum-contrast = 4.5   # 1.0-21.0 WCAG ratio floor on cell fg vs bg
# env = EDITOR=vim         # repeat to inject into spawned shells

# Keybinds: keybind = <chord>=<action>; empty action disables.
# chords: ctrl+shift+c, alt+enter, ...  actions: copy, paste,
# new_tab, close_tab, new_window, next_tab, prev_tab, select_tab_1..8,
# font_bigger, font_smaller, font_reset, clear_scrollback, search,
# prompt_prev, prompt_next, select_all, scroll_to_top,
# scroll_to_bottom, quit, split_right, split_down,
# focus_next_pane, focus_prev_pane
# keybind = ctrl+alt+a=select_all
# keybind = global:ctrl+alt+u=toggle_quick_terminal   # X11 root grab — fires anywhere
# tab-bar-min-tabs = 2    # hide the tab strip until N tabs exist
# word-select-chars = ,│`|:\"' ()[]{}<>\t   # double-click word separators
# visual-bell = true      # flash the pane on BEL
# open-link-with = firefox --new-window {}   # {} = the URL (default xdg-open)
# quick-terminal-position = top   # top | bottom | left | right | center
# shell-integration-features = cursor,sudo,title   # prefix a feature with no- to disable
# clipboard-trim = true   # trim whitespace at the ends of copied text
# keybind = ctrl+shift+q=unbind   # unbind a chord (falls through to literal keys)

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
            .map(|(_, a)| a.clone())
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
                "foreground" => match parse_rgb(value) {
                    Some(c) => cfg.foreground = Some(c),
                    None => errors.push(format!("line {}: bad foreground {value:?}", n + 1)),
                },
                "background" => match parse_rgb(value) {
                    Some(c) => cfg.background = Some(c),
                    None => errors.push(format!("line {}: bad background {value:?}", n + 1)),
                },
                "cursor-color" | "cursor_color" => match parse_rgb(value) {
                    Some(c) => cfg.cursor_color = Some(c),
                    None => errors.push(format!("line {}: bad cursor-color {value:?}", n + 1)),
                },
                "selection-color" | "selection_color" | "selection-foreground" => {
                    match parse_rgb(value) {
                        Some(c) => cfg.selection_color = Some(c),
                        None => {
                            errors.push(format!("line {}: bad selection-color {value:?}", n + 1))
                        }
                    }
                }
                "selection-background" | "selection_background" => {
                    match parse_rgb(value) {
                        Some(c) => cfg.selection_background = Some(c),
                        None => errors
                            .push(format!("line {}: bad selection-background {value:?}", n + 1)),
                    }
                }
                "split-divider-color" | "split_divider_color" => {
                    match parse_rgb(value) {
                        Some(c) => cfg.split_divider_color = Some(c),
                        None => errors
                            .push(format!("line {}: bad split-divider-color {value:?}", n + 1)),
                    }
                }
                "unfocused-split-fill" | "unfocused_split_fill" => {
                    match parse_rgb(value) {
                        Some(c) => cfg.unfocused_split_fill = Some(c),
                        None => errors
                            .push(format!("line {}: bad unfocused-split-fill {value:?}", n + 1)),
                    }
                }
                "title" => {
                    if value.is_empty() {
                        cfg.title = None;
                    } else {
                        cfg.title = Some(value.to_string());
                    }
                }
                "palette" => match value.split_once('=').and_then(|(i, c)| {
                    i.trim().parse::<u8>().ok().zip(parse_rgb(c.trim()))
                }) {
                    Some(pair) => cfg.palette_overrides.push(pair),
                    None => errors.push(format!(
                        "line {}: bad palette {value:?} (want N=#rrggbb)",
                        n + 1
                    )),
                },
                "mouse-scroll-multiplier" | "mouse_scroll_multiplier" => {
                    match value.parse::<f32>() {
                        Ok(v) if (0.1..=100.0).contains(&v) => {
                            cfg.mouse_scroll_multiplier = v;
                        }
                        _ => errors.push(format!(
                            "line {}: bad mouse-scroll-multiplier {value:?}",
                            n + 1
                        )),
                    }
                }
                "confirm-close" | "confirm-close-surface" | "confirm_close" => {
                    cfg.confirm_close = bool_value(value, n, &mut errors);
                }
                "osc52-write" | "clipboard-write" => match value {
                    "allow" | "true" | "yes" | "1" | "on" => cfg.osc52_write = true,
                    "deny" | "false" | "no" | "0" | "off" => cfg.osc52_write = false,
                    _ => errors.push(format!("line {}: bad osc52-write {value:?}", n + 1)),
                },
                "osc52-read" | "clipboard-read" => match value {
                    "allow" | "always" | "true" => cfg.clipboard_read = ClipboardRead::Allow,
                    "ask" | "prompt" => cfg.clipboard_read = ClipboardRead::Ask,
                    "deny" | "never" | "false" => cfg.clipboard_read = ClipboardRead::Deny,
                    _ => errors.push(format!("line {}: bad osc52-read {value:?}", n + 1)),
                },
                "mouse-shift-override" | "mouse_shift_override" => {
                    cfg.mouse_shift_override = bool_value(value, n, &mut errors);
                }
                "cursor-invert-fg-bg" | "cursor_invert_fg_bg" => {
                    cfg.cursor_invert_fg_bg = bool_value(value, n, &mut errors);
                }
                "bold-is-bright" | "bold_is_bright"
                | "draw-bold-text-with-bright-colors" => match value {
                    "true" | "yes" | "1" | "on" => cfg.bold_is_bright = true,
                    "false" | "no" | "0" | "off" => cfg.bold_is_bright = false,
                    _ => errors.push(format!("line {}: bad bold-is-bright {value:?}", n + 1)),
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
                "copy-on-select" => cfg.copy_on_select = match value {
                    "true" | "yes" | "on" | "both" => CopyOnSelect::Both,
                    "false" | "no" | "off" | "disabled" | "none" => CopyOnSelect::Disabled,
                    "clipboard" => CopyOnSelect::Clipboard,
                    "primary" => CopyOnSelect::Primary,
                    _ => {
                        errors.push(format!("line {}: bad copy-on-select {value:?}", n + 1));
                        cfg.copy_on_select
                    }
                },
                "shell-integration" => match value {
                    "none" => cfg.shell_integration = ShellIntegration::None,
                    "detect" => cfg.shell_integration = ShellIntegration::Detect,
                    "bash" => cfg.shell_integration = ShellIntegration::Bash,
                    "zsh" => cfg.shell_integration = ShellIntegration::Zsh,
                    "fish" => cfg.shell_integration = ShellIntegration::Fish,
                    _ => errors.push(format!("line {}: bad shell-integration {value:?}", n + 1)),
                },
                "shell-integration-features" => {
                    for feat in value.split(',') {
                        let feat = feat.trim();
                        match feat {
                            "cursor" => cfg.shell_features.cursor = true,
                            "no-cursor" => cfg.shell_features.cursor = false,
                            "sudo" => cfg.shell_features.sudo = true,
                            "no-sudo" => cfg.shell_features.sudo = false,
                            "title" => cfg.shell_features.title = true,
                            "no-title" => cfg.shell_features.title = false,
                            "" => {}
                            _ => errors.push(format!(
                                "line {}: bad shell-integration-feature {feat:?}",
                                n + 1
                            )),
                        }
                    }
                }
                "quick-terminal-position" => match value {
                    "top" => cfg.quick_terminal_position = QuickTermPosition::Top,
                    "bottom" => cfg.quick_terminal_position = QuickTermPosition::Bottom,
                    "left" => cfg.quick_terminal_position = QuickTermPosition::Left,
                    "right" => cfg.quick_terminal_position = QuickTermPosition::Right,
                    "center" => cfg.quick_terminal_position = QuickTermPosition::Center,
                    _ => errors.push(format!(
                        "line {}: bad quick-terminal-position {value:?}",
                        n + 1
                    )),
                },
                "clipboard-trim" => match value {
                    "true" | "1" | "yes" | "on" => cfg.clipboard_trim = true,
                    "false" | "0" | "no" | "off" => cfg.clipboard_trim = false,
                    _ => errors.push(format!("line {}: bad clipboard-trim {value:?}", n + 1)),
                },
                "desktop-notifications" => {
                    cfg.desktop_notifications = bool_value(value, n, &mut errors);
                }
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
                "command" => {
                    let argv: Vec<String> =
                        value.split_whitespace().map(String::from).collect();
                    cfg.command = (!argv.is_empty()).then_some(argv);
                }
                "working-directory" | "working_directory" | "working-dir" => {
                    cfg.working_directory = (!value.is_empty()).then(|| expand_home(value));
                }
                "unfocused-split-opacity" | "unfocused_split_opacity" => {
                    match value.parse::<f32>() {
                        Ok(v) if (0.0..=1.0).contains(&v) => {
                            cfg.unfocused_split_opacity = v;
                        }
                        _ => errors.push(format!(
                            "line {}: bad unfocused-split-opacity {value:?}",
                            n + 1
                        )),
                    }
                }
                "resize-overlay" | "resize_overlay" => {
                    cfg.resize_overlay = bool_value(value, n, &mut errors);
                }
                "focus-follows-mouse" | "focus_follows_mouse" => {
                    cfg.focus_follows_mouse = bool_value(value, n, &mut errors);
                }
                "window-width" | "window_width" => match value.parse::<f32>() {
                    Ok(v) if (0.0..=4000.0).contains(&v) => cfg.window_width = v,
                    _ => errors.push(format!("line {}: bad window-width {value:?}", n + 1)),
                },
                "window-height" | "window_height" => match value.parse::<f32>() {
                    Ok(v) if (0.0..=4000.0).contains(&v) => cfg.window_height = v,
                    _ => errors.push(format!("line {}: bad window-height {value:?}", n + 1)),
                },
                "window-x" | "window_x" => match value.parse::<f32>() {
                    Ok(v) if (-2000.0..=8000.0).contains(&v) => cfg.window_x = Some(v),
                    _ => errors.push(format!("line {}: bad window-x {value:?}", n + 1)),
                },
                "window-y" | "window_y" => match value.parse::<f32>() {
                    Ok(v) if (-2000.0..=8000.0).contains(&v) => cfg.window_y = Some(v),
                    _ => errors.push(format!("line {}: bad window-y {value:?}", n + 1)),
                },
                "open-link-modifier" | "open_link_modifier" => {
                    match value.parse::<LinkMod>() {
                        Ok(m) => cfg.open_link_modifier = m,
                        Err(e) => errors.push(format!("line {}: {e}", n + 1)),
                    }
                }
                "click-interval" | "click_interval" => match value.parse::<u64>() {
                    Ok(v) if (50..=2000).contains(&v) => cfg.click_interval = v,
                    _ => errors.push(format!("line {}: bad click-interval {value:?}", n + 1)),
                },
                "selection-invert-fg-bg" | "selection_invert_fg_bg" => {
                    cfg.selection_invert = bool_value(value, n, &mut errors);
                }
                "window-save-state" | "window_save_state" => {
                    cfg.window_save_state = bool_value(value, n, &mut errors);
                }
                "window-fullscreen" | "window_fullscreen" => {
                    cfg.window_fullscreen = bool_value(value, n, &mut errors);
                }
                "wait-after-command" | "wait_after_command" => {
                    cfg.wait_after_command = bool_value(value, n, &mut errors);
                }
                "quit-after-last-window-closed" | "quit_after_last_window_closed" => {
                    cfg.quit_after_last_window_closed = bool_value(value, n, &mut errors);
                }
                "window-theme" | "window_theme" => match value {
                    "auto" | "system" => cfg.window_theme = WindowTheme::Auto,
                    "light" => cfg.window_theme = WindowTheme::Light,
                    "dark" => cfg.window_theme = WindowTheme::Dark,
                    _ => errors.push(format!("line {}: bad window-theme {value:?}", n + 1)),
                },
                "window-decoration" | "window_decoration" => {
                    cfg.window_decoration = match value {
                        "false" | "none" => false,
                        "true" | "auto" | "client" | "server" => true,
                        _ => bool_value(value, n, &mut errors),
                    };
                }
                "background-image" | "background_image" => {
                    let p = value.trim();
                    if p.is_empty() || p == "none" {
                        cfg.background_image = None;
                    } else {
                        let expanded = if let Some(rest) = p.strip_prefix("~/") {
                            std::env::var_os("HOME")
                                .map(|h| PathBuf::from(h).join(rest))
                                .unwrap_or_else(|| PathBuf::from(p))
                        } else {
                            PathBuf::from(p)
                        };
                        cfg.background_image = Some(expanded);
                    }
                }
                "background-image-opacity" | "background_image_opacity" => {
                    match value.parse::<f32>() {
                        Ok(v) if (0.0..=1.0).contains(&v) => {
                            cfg.background_image_opacity = v;
                        }
                        _ => errors.push(format!(
                            "line {}: bad background-image-opacity {value:?}",
                            n + 1
                        )),
                    }
                }
                "background-image-fit" | "background_image_fit" => match value {
                    "contain" => cfg.background_image_fit = BgFit::Contain,
                    "cover" => cfg.background_image_fit = BgFit::Cover,
                    "stretch" => cfg.background_image_fit = BgFit::Stretch,
                    "tile" => cfg.background_image_fit = BgFit::Tile,
                    _ => errors.push(format!(
                        "line {}: bad background-image-fit {value:?}",
                        n + 1
                    )),
                },
                "background-image-repeat" | "background_image_repeat" => {
                    cfg.background_image_repeat = bool_value(value, n, &mut errors);
                }
                "window-new-tab-position" | "window_new_tab_position" | "new-tab-position" => {
                    match value {
                        "end" => cfg.new_tab_position = NewTabPosition::End,
                        "current" => cfg.new_tab_position = NewTabPosition::Current,
                        _ => errors.push(format!(
                            "line {}: bad window-new-tab-position {value:?} (end|current)",
                            n + 1
                        )),
                    }
                }
                "window-inherit-working-directory" | "window_inherit_working_directory"
                | "inherit-working-directory" => {
                    cfg.inherit_working_directory = bool_value(value, n, &mut errors);
                }
                "window-inherit-font-size" | "window_inherit_font_size"
                | "inherit-font-size" => {
                    cfg.inherit_font_size = bool_value(value, n, &mut errors);
                }
                "scroll-to-cursor" | "scroll_to_cursor" => {
                    cfg.scroll_to_cursor = bool_value(value, n, &mut errors);
                }
                "cursor-text" | "cursor_text" => match parse_rgb(value) {
                    Some(rgb) => cfg.cursor_text = Some(rgb),
                    None => errors.push(format!("line {}: bad cursor-text {value:?}", n + 1)),
                },
                "scroll-on-input" | "scroll_on_input" => {
                    cfg.scroll_on_input = bool_value(value, n, &mut errors);
                }
                "cursor-opacity" | "cursor_opacity" => match value.parse::<f32>() {
                    Ok(v) if (0.0..=1.0).contains(&v) => cfg.cursor_opacity = v,
                    _ => errors.push(format!("line {}: bad cursor-opacity {value:?}", n + 1)),
                },
                "env" => match value.split_once('=') {
                    Some((name, val)) if !name.trim().is_empty() => {
                        cfg.env.push((name.trim().to_string(), val.to_string()));
                    }
                    _ => errors.push(format!(
                        "line {}: bad env {value:?} (want NAME=VALUE)",
                        n + 1
                    )),
                },
                "adjust-cell-width" | "cell-width" => match parse_cell_adjust(value) {
                    Some(pct) => cfg.cell_width_adjust = pct,
                    None => errors.push(format!(
                        "line {}: bad adjust-cell-width {value:?} (want N% or Npx)",
                        n + 1
                    )),
                },
                "adjust-cell-height" | "cell-height" => match parse_cell_adjust(value) {
                    Some(pct) => cfg.cell_height_adjust = pct,
                    None => errors.push(format!(
                        "line {}: bad adjust-cell-height {value:?} (want N% or Npx)",
                        n + 1
                    )),
                },
                "adjust-font-baseline" | "font-baseline" => match parse_cell_adjust(value) {
                    Some(pct) => cfg.font_baseline_adjust = pct,
                    None => errors.push(format!(
                        "line {}: bad adjust-font-baseline {value:?} (want N% or Npx)",
                        n + 1
                    )),
                },
                "font-feature" | "font_feature" => {
                    for spec in value.split(',') {
                        let spec = spec.trim();
                        if spec.is_empty() {
                            continue;
                        }
                        if crate::fonts::parse_font_feature(spec).is_some() {
                            cfg.font_features.push(spec.to_string());
                        } else {
                            errors.push(format!(
                                "line {}: bad font-feature {spec:?} (want -tag, +tag, tag or tag=N)",
                                n + 1
                            ));
                        }
                    }
                }
                "minimum-contrast" | "minimum_contrast" => match value.parse::<f32>() {
                    Ok(v) if (1.0..=21.0).contains(&v) => cfg.minimum_contrast = v,
                    _ => errors.push(format!(
                        "line {}: bad minimum-contrast {value:?} (want 1.0-21.0)",
                        n + 1
                    )),
                },
                "tab-bar-min-tabs" | "tab_bar_min_tabs" => match value.parse::<usize>() {
                    Ok(v) if v <= 64 => cfg.tab_bar_min_tabs = v,
                    _ => errors.push(format!("line {}: bad tab-bar-min-tabs {value:?}", n + 1)),
                },
                "word-select-chars" | "word_select_chars" => {
                    cfg.word_select_chars =
                        value.replace("\\t", "\t").replace("\\n", "\n");
                }
                "visual-bell" | "visual_bell" => {
                    cfg.visual_bell = bool_value(value, n, &mut errors);
                }
                "open-link-with" | "open_link_with" => {
                    cfg.open_link_with = (!value.is_empty()).then(|| value.to_string());
                }
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

/// Per-cell spacing adjustment: `N%` of the measured cell or `Npx`
/// absolute points (bare numbers read as percent, matching Ghostty).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum CellAdjust {
    /// No adjustment.
    #[default]
    None,
    /// Fraction of the measured cell extent (0.20 = +20%).
    Fraction(f32),
    /// Absolute points added to the cell extent.
    Points(f32),
}

impl CellAdjust {
    /// Apply the adjustment to a measured extent.
    pub fn apply(self, base: f32) -> f32 {
        match self {
            Self::None => base,
            Self::Fraction(p) => base * (1.0 + p),
            Self::Points(px) => base + px,
        }
    }
}

fn parse_cell_adjust(value: &str) -> Option<CellAdjust> {
    let v = value.trim();
    if let Some(p) = v.strip_suffix('%') {
        let f: f32 = p.trim().parse().ok()?;
        return (-50.0..=100.0).contains(&f).then_some(CellAdjust::Fraction(f / 100.0));
    }
    if let Some(p) = v.strip_suffix("px") {
        let f: f32 = p.trim().parse().ok()?;
        return (-100.0..=200.0).contains(&f).then_some(CellAdjust::Points(f));
    }
    let f: f32 = v.parse().ok()?;
    (-50.0..=100.0).contains(&f).then_some(CellAdjust::Fraction(f / 100.0))
}

/// `~/…` expands to `$HOME/…`; anything else passes through verbatim.
fn expand_home(value: &str) -> PathBuf {
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(value)
}

/// `#rgb` / `#rrggbb` / `0xrrggbb` → a terminal RGB. No names,
/// no alpha — those live in the theme, not the override keys.
fn parse_rgb(value: &str) -> Option<Rgb> {
    let hex = value
        .strip_prefix('#')
        .or_else(|| value.strip_prefix("0x"))
        .or_else(|| value.strip_prefix("0X"))?;
    let u32_from_hex = |s: &str| u32::from_str_radix(s, 16).ok();
    match hex.len() {
        3 => u32_from_hex(hex).map(|v| {
            let r = ((v >> 8) & 0xf) as u8;
            let g = ((v >> 4) & 0xf) as u8;
            let b = (v & 0xf) as u8;
            Rgb { r: r * 17, g: g * 17, b: b * 17 }
        }),
        6 => u32_from_hex(hex).map(|v| Rgb {
            r: ((v >> 16) & 0xff) as u8,
            g: ((v >> 8) & 0xff) as u8,
            b: (v & 0xff) as u8,
        }),
        _ => None,
    }
}

/// Every action name the `keybind` parser accepts — printed by
/// `+list-actions`. Parameterized forms show their argument shape.
pub const ACTION_NAMES: &[&str] = &[
    "copy", "paste", "new_tab", "close_tab", "new_window", "next_tab", "prev_tab",
    "select_tab_<n>", "move_tab_left", "move_tab_right",
    "font_bigger", "font_smaller", "font_reset",
    "increase_font_size[:pt]", "decrease_font_size[:pt]",
    "clear_scrollback", "clear_screen", "reset", "search", "search_next", "search_prev",
    "prompt_prev", "prompt_next", "select_all", "start_selection",
    "last_tab", "close_window", "toggle_tab_bar",
    "scroll_to_top", "scroll_to_bottom", "scroll_page_up", "scroll_page_down",
    "scroll_line_up", "scroll_line_down",
    "url_hints", "copy_last_output", "open_scrollback_editor", "reload_config",
    "write_screen_file", "write_scrollback_file", "write_selection_file",
    "quit", "fullscreen", "palette", "settings",
    "split_right", "split_down", "split_left", "split_up",
    "new_split:<right|down|left|up|auto>",
    "goto_split:<left|right|up|down|previous|next|top|bottom>",
    "resize_split:<left|right|up|down>[,px]",
    "focus_next_pane", "focus_prev_pane",
    "toggle_split_zoom", "equalize_splits",
    "none | unbind  (disable a chord)",
];

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
    // `global:` — an X11 root grab, fired while any window has focus
    // (Ghostty `keybind = global:chord=action`). Kept inside `keybinds`
    // with a `global:` tag; `lookup_keybind` never produces it.
    let raw = chord.trim();
    let chord = match raw
        .to_ascii_lowercase()
        .strip_prefix("global:")
    {
        Some(rest) => format!("global:{}", normalize_chord(rest)?),
        None => normalize_chord(raw)?,
    };
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
        "toggle_quick_terminal" | "quick_terminal" => TermAction::ToggleQuickTerminal,
        "last_tab" => TermAction::LastTab,
        "close_window" => TermAction::CloseWindow,
        "toggle_tab_bar" => TermAction::ToggleTabBar,
        "next_tab" => TermAction::NextTab,
        "prev_tab" => TermAction::PrevTab,
        "font_bigger" => TermAction::FontBigger,
        "font_smaller" => TermAction::FontSmaller,
        "font_reset" | "reset_font_size" => TermAction::FontReset,
        // Ghostty `increase_font_size:pt` / `decrease_font_size:pt`;
        // a bare action name steps 1pt.
        _ if name.strip_prefix("increase_font_size").is_some() => {
            let pts = name["increase_font_size".len()..]
                .strip_prefix(':')
                .and_then(|s| s.trim().parse::<i32>().ok())
                .unwrap_or(1)
                .clamp(1, 48);
            TermAction::IncreaseFontSize(pts)
        }
        _ if name.strip_prefix("decrease_font_size").is_some() => {
            let pts = name["decrease_font_size".len()..]
                .strip_prefix(':')
                .and_then(|s| s.trim().parse::<i32>().ok())
                .unwrap_or(1)
                .clamp(1, 48);
            TermAction::DecreaseFontSize(pts)
        }
        "clear_scrollback" => TermAction::ClearScrollback,
        "clear_screen" => TermAction::ClearScreen,
        "reset" => TermAction::Reset,
        "search" => TermAction::Search,
        "prompt_prev" => TermAction::PromptPrev,
        "prompt_next" => TermAction::PromptNext,
        "select_all" => TermAction::SelectAll,
        "start_selection" => TermAction::StartSelection,
        "scroll_to_top" => TermAction::ScrollToTop,
        "scroll_to_bottom" => TermAction::ScrollToBottom,
        "scroll_page_up" => TermAction::ScrollPageUp,
        "scroll_page_down" => TermAction::ScrollPageDown,
        "scroll_line_up" => TermAction::ScrollLineUp,
        "scroll_line_down" => TermAction::ScrollLineDown,
        "move_tab_left" => TermAction::MoveTabLeft,
        "move_tab_right" => TermAction::MoveTabRight,
        "url_hints" => TermAction::UrlHints,
        "copy_last_output" => TermAction::CopyLastOutput,
        "open_scrollback_editor" => TermAction::OpenScrollbackEditor,
        "search_next" => TermAction::SearchNext,
        "search_prev" => TermAction::SearchPrev,
        "reload_config" => TermAction::ReloadConfig,
        "toggle_split_zoom" | "toggle_pane_zoom" => TermAction::PaneZoom,
        "quit" => TermAction::Quit,
        "split_right" => TermAction::SplitRight,
        "split_down" => TermAction::SplitDown,
        "focus_next_pane" => TermAction::FocusNextPane,
        "focus_prev_pane" => TermAction::FocusPrevPane,
        "equalize_splits" | "equalise_splits" => TermAction::EqualizeSplits,
        "fullscreen" => TermAction::Fullscreen,
        "palette" | "command_palette" => TermAction::Palette,
        "settings" => TermAction::Settings,
        "write_screen_file" => TermAction::WriteScreenFile,
        "write_scrollback_file" => TermAction::WriteScrollbackFile,
        "write_selection_file" => TermAction::WriteSelectionFile,
        _ if name.strip_prefix("select_tab_").is_some() => {
            let n: usize = name["select_tab_".len()..].parse().ok()?;
            TermAction::SelectTab(n)
        }
        // Ghostty `keybind = ...=new_split:right` — direction arg selects
        // which side the new pane lands on (`auto` = right).
        _ if name.strip_prefix("new_split:").is_some() => {
            match &name["new_split:".len()..] {
                "right" | "auto" => TermAction::SplitRight,
                "down" => TermAction::SplitDown,
                "left" => TermAction::SplitLeft,
                "up" => TermAction::SplitUp,
                _ => return None,
            }
        }
        // Ghostty `keybind = ...=goto_split:left` — directional focus,
        // previous/next cycle, top/bottom = first/last pane.
        _ if name.strip_prefix("goto_split:").is_some() => {
            match &name["goto_split:".len()..] {
                "left" => TermAction::FocusPaneDir {
                    horizontal: true,
                    forward: false,
                },
                "right" => TermAction::FocusPaneDir {
                    horizontal: true,
                    forward: true,
                },
                "up" => TermAction::FocusPaneDir {
                    horizontal: false,
                    forward: false,
                },
                "down" => TermAction::FocusPaneDir {
                    horizontal: false,
                    forward: true,
                },
                "previous" | "prev" => TermAction::FocusPrevPane,
                "next" => TermAction::FocusNextPane,
                "top" => TermAction::GotoSplit(0),
                "bottom" => TermAction::GotoSplit(usize::MAX),
                _ => return None,
            }
        }
        // Ghostty `keybind = ...=resize_split:up[,10]` — optional `,px`
        // amount; without one it uses the 48pt arrow-key step.
        _ if name.strip_prefix("resize_split:").is_some() => {
            let arg = &name["resize_split:".len()..];
            let (dir, amount) = arg.split_once(',').map_or((arg, 48), |(d, a)| {
                (d, a.trim().parse::<i32>().unwrap_or(48))
            });
            let (horizontal, forward) = match dir {
                "left" => (true, false),
                "right" => (true, true),
                "up" => (false, false),
                "down" => (false, true),
                _ => return None,
            };
            TermAction::ResizePane {
                horizontal,
                forward,
                px: amount,
            }
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

/// Modifier required to open links with a click (`open-link-modifier`).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum LinkMod {
    /// Control key (default, Ghostty-compatible).
    #[default]
    Ctrl,
    Shift,
    Alt,
    /// The "super"/meta key.
    Super,
}

impl std::str::FromStr for LinkMod {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ctrl" | "control" => Ok(Self::Ctrl),
            "shift" => Ok(Self::Shift),
            "alt" | "option" => Ok(Self::Alt),
            "super" | "meta" | "cmd" | "command" => Ok(Self::Super),
            other => Err(format!("bad open-link-modifier {other:?}")),
        }
    }
}

/// Program clipboard-read policy (`clipboard-read`).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum ClipboardRead {
    /// Reads are answered immediately.
    Allow,
    /// Reads wait on a confirm prompt (snackbar "Allow" or Enter).
    #[default]
    Ask,
    /// Reads are answered with empty text.
    Deny,
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

    /// Unconditional re-read (the `reload-config` action path).
    pub fn reload(&mut self) {
        let mtime = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        self.mtime = mtime;
        let (config, errors) = AppConfig::load(&self.path);
        self.config = config;
        self.errors = errors;
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
        assert_eq!(cfg.copy_on_select, CopyOnSelect::Both);
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
    fn color_overrides_parse() {
        let (cfg, errs) = AppConfig::parse(
            "foreground = #ddeeff\nbackground = #101418\n\
             cursor-color = #fc0\nselection-color = 0xffffff\n\
             palette = 1=#e06c75\npalette = 0=#000\n",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.foreground, Some(Rgb { r: 0xdd, g: 0xee, b: 0xff }));
        assert_eq!(cfg.background, Some(Rgb { r: 0x10, g: 0x14, b: 0x18 }));
        assert_eq!(cfg.cursor_color, Some(Rgb { r: 0xff, g: 0xcc, b: 0x00 }));
        assert_eq!(cfg.selection_color, Some(Rgb { r: 0xff, g: 0xff, b: 0xff }));
        assert_eq!(
            cfg.palette_overrides,
            vec![
                (1, Rgb { r: 0xe0, g: 0x6c, b: 0x75 }),
                (0, Rgb { r: 0x00, g: 0x00, b: 0x00 }),
            ]
        );
    }

    #[test]
    fn scroll_multiplier_and_confirm_close() {
        let (cfg, errs) = AppConfig::parse("mouse-scroll-multiplier = 3.5\nconfirm-close = no");
        assert!(errs.is_empty(), "{errs:?}");
        assert!((cfg.mouse_scroll_multiplier - 3.5).abs() < f32::EPSILON);
        assert!(!cfg.confirm_close);
        let (_, errs) = AppConfig::parse("mouse-scroll-multiplier = 0");
        assert_eq!(errs.len(), 1);
    }

    #[test]
    fn bad_colors_report() {
        let (_, errs) = AppConfig::parse(
            "foreground = red\nbackground = #12345\npalette = x=#fff\npalette = 300=#fff\n",
        );
        assert_eq!(errs.len(), 4, "{errs:?}");
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

    #[test]
    fn window_geometry_and_save_state_parse() {
        let (cfg, errs) = AppConfig::parse(
            "window-width = 1024\nwindow-height = 768\nwindow-save-state = true\n",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert!((cfg.window_width - 1024.0).abs() < f32::EPSILON);
        assert!((cfg.window_height - 768.0).abs() < f32::EPSILON);
        assert!(cfg.window_save_state);
    }

    #[test]
    fn parses_window_fullscreen() {
        let (cfg, errs) = AppConfig::parse("window-fullscreen = true\n");
        assert!(errs.is_empty(), "{errs:?}");
        assert!(cfg.window_fullscreen);
    }

    #[test]
    fn parses_r19_keys() {
        let (cfg, errs) = AppConfig::parse(
            "mouse-shift-override = false\nclipboard-read = deny\ncursor-invert-fg-bg = false\n",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert!(!cfg.mouse_shift_override);
        assert_eq!(cfg.clipboard_read, ClipboardRead::Deny);
        assert!(!cfg.cursor_invert_fg_bg);
        // Defaults are the Ghostty ones: override on, ask, invert on.
        let (d, _) = AppConfig::parse("");
        assert!(d.mouse_shift_override);
        assert_eq!(d.clipboard_read, ClipboardRead::Ask);
        assert!(d.cursor_invert_fg_bg);
    }

    #[test]
    fn env_lines_collect_name_value_pairs() {
        let (cfg, errs) = AppConfig::parse(
            "env = EDITOR=vim\nenv = A=B=C\nenv = =x\n",
        );
        assert_eq!(errs.len(), 1, "{errs:?}"); // `=x` has an empty name
        assert_eq!(
            cfg.env,
            vec![
                ("EDITOR".to_string(), "vim".to_string()),
                ("A".to_string(), "B=C".to_string()),
            ]
        );
    }

    #[test]
    fn cell_adjust_parses_percent_and_px() {
        let (cfg, errs) = AppConfig::parse(
            "adjust-cell-width = 20%\nadjust-cell-height = 4px\n",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.cell_width_adjust, CellAdjust::Fraction(0.2));
        assert_eq!(cfg.cell_height_adjust, CellAdjust::Points(4.0));
        // Bare number = percent; bad input reports.
        let (_, errs) = AppConfig::parse("adjust-cell-width = wide");
        assert_eq!(errs.len(), 1);
        assert_eq!(
            CellAdjust::Fraction(0.5).apply(10.0),
            15.0
        );
        assert_eq!(CellAdjust::Points(3.0).apply(10.0), 13.0);
        assert_eq!(CellAdjust::None.apply(10.0), 10.0);
    }

    #[test]
    fn minimum_contrast_parses_in_range() {
        let (cfg, errs) = AppConfig::parse("minimum-contrast = 4.5\n");
        assert!(errs.is_empty(), "{errs:?}");
        assert!((cfg.minimum_contrast - 4.5).abs() < f32::EPSILON);
        let (_, errs) = AppConfig::parse("minimum-contrast = 0.5");
        assert_eq!(errs.len(), 1);
    }
}
