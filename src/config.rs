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
use keyboard_types::{Code, Key, Modifiers, NamedKey};

use crate::keys::{AdjustSel, FileSink, TermAction};
use crate::theme::Theme;

/// Which theme a `theme =` value resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeRef {
    /// Follow the desktop light/dark preference.
    Auto,
    /// A named theme from the catalog (e.g. `solarized-dark`).
    Named(String),
    /// `theme = light:<name>,dark:<name>` — a named theme per mode,
    /// resolved against the same desktop preference as `Auto`
    /// (Ghostty's light/dark theme pair).
    Pair { light: String, dark: String },
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

/// `mouse-shift-capture` — under what circumstances Shift is reported to
/// the running program in mouse events (Ghostty). `False`/`Never` never
/// report it, `True` reports it only when the program has no mouse
/// capture, `Always` always reports it. When Shift is not captured, a
/// shifted press/drag bypasses the program's mouse reporting and does
/// the local selection instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseShiftCapture {
    False,
    True,
    Always,
    Never,
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

/// `quick-terminal-size` — one axis extent of the drop-down window:
/// a percentage of the screen (`50%`) or pixels (`300px`). A bare
/// number is a config error (Ghostty `quick-terminal-size`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum QuickTermSize {
    Percent(f64),
    Px(f64),
}

/// `bold-color` — bold-text color override (Ghostty 1.2, replacing
/// `bold-is-bright`): `Bright` lifts every bold cell to the bright
/// palette slot; `Color` fixes the default-fg bold color AND lifts
/// bold palette colors to bright; `None` = no bold recoloring.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BoldColor {
    None,
    Bright,
    Color(Rgb),
}

/// `grapheme-width-method` — whether a grapheme cluster occupies the
/// width of its cluster (unicode) or each scalar gets its own cells
/// (legacy). Ghostty `grapheme-width-method`, default `unicode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphemeWidthMethod {
    Unicode,
    Legacy,
}

/// `right-click-action` — what a secondary click does on a pane
/// (Ghostty `right-click-action`, default `context-menu`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RightClickAction {
    /// Show the framework context menu (and extend a live selection
    /// under the pointer, xterm-style).
    ContextMenu,
    /// Copy the active selection to the clipboard.
    Copy,
    /// Paste the clipboard at the pointer.
    Paste,
    /// Do nothing on a right click.
    Ignore,
}

/// `middle-click-action` — what a middle click does on a pane
/// (Ghostty `middle-click-action`, default `primary-paste`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MiddleClickAction {
    /// Paste the PRIMARY selection.
    PrimaryPaste,
    /// Paste the regular clipboard.
    ClipboardPaste,
    /// Do nothing on a middle click.
    Ignore,
}

/// `confirm-close-surface` — when closing a pane/tab prompts
/// (Ghostty `confirm-close-surface`, default `true`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmCloseSurface {
    /// Never prompt — close immediately.
    False,
    /// Prompt only when a program other than the shell is running.
    True,
    /// Always prompt, even at an idle shell.
    Always,
}

/// `window-new-tab-position` — where a new tab lands in the strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewTabPosition {
    /// Append after the last tab (Ghostty default).
    End,
    /// Insert immediately after the selected tab.
    Current,
}

/// `notify-on-command-finish` — when a shell-integrated command
/// completes (OSC 133 `D`), whether to raise the 🔔 notification
/// (Ghostty). `unfocused` fires only when the session isn't the
/// focused pane of the selected tab (app-level approximation — OS
/// window focus isn't observable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyWhen {
    No,
    Unfocused,
    Always,
}

/// `window-padding-color` — how the space around the cell grid is
/// colored (Ghostty `window-padding-color`, default `background`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowPaddingColor {
    /// Padding always takes the theme background.
    Background,
    /// Each padding cell extends the color of the cell next to it. On
    /// the primary screen, vertical extension is skipped when the
    /// nearest row has any default-background cells, is a prompt row,
    /// or contains a perfect-fit powerline glyph — the alternate
    /// screen extends unconditionally.
    Extend,
    /// Always extend, ignoring the primary-screen heuristics.
    ExtendAlways,
}

/// `resize-overlay` — when the cols×rows chip shows during a resize
/// (Ghostty 3-state enum; `true`/`false` parse as the deprecated
/// `always`/`never` spellings this repo accepted before the enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeOverlay {
    /// Never show the chip.
    Never,
    /// Show on every resize, including the first layout.
    Always,
    /// Skip the surface's initial-layout resize (Ghostty default).
    AfterFirst,
}

/// `resize-overlay-position` — where in the pane the chip sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeOverlayPosition {
    Center,
    TopLeft,
    TopCenter,
    TopRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

/// `osc-color-report-format` — component width in OSC 4/10/11/12
/// report replies (`none` suppresses the reply).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OscColorReportFormat {
    /// No reply.
    None,
    /// `rgb:rr/gg/bb` (8-bit, unscaled).
    Bits8,
    /// `rgb:rrrr/gggg/bbbb` (16-bit, scaled — the default).
    Bits16,
}

/// One parsed `keybind` trigger: the normalized `ctrl+alt+shift+super+key`
/// chord plus Ghostty's prefix flags (combinable in any order, e.g.
/// `keybind = global:unconsumed:ctrl+a=reload_config`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindTrigger {
    pub chord: String,
    /// `global:` — grabbed on the X11 root so it fires while unfocused.
    /// Also matches the local keybind table (the grab eats the key when
    /// live, so no double-fire). Always consumes the press.
    pub global: bool,
    /// `all:` — apply the action to every surface. Always consumes.
    pub all: bool,
    /// `unconsumed:` — fire the action AND still send the encoded key to
    /// the program. A no-op under `global:`/`all:` (they always
    /// consume — Ghostty `Binding.zig` Flags.consumed).
    pub unconsumed: bool,
    /// `performable:` — only fire while the action is performable.
    pub performable: bool,
    /// `physical:` — match the physical key position (winit `Code`),
    /// not the layout-translated character. `chord` then stores only
    /// the modifier prefix (`ctrl+shift+`) for compare.
    pub physical_code: Option<Code>,
}

/// Fully-resolved settings — defaults plus file overrides.
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub font_size: f32,
    /// Comma-joined fallback chain: the first `font-family` line is the
    /// primary, each repeat appends (Ghostty repeatable key).
    /// `TermFonts::load` resolves the list in order.
    pub font_family: String,
    /// Whether any `font-family` line has been seen this parse — drives
    /// repeat-append (parse-time only; not serialized).
    #[doc(hidden)]
    pub font_family_set: bool,
    /// `font-family-bold` / `font-family-italic` /
    /// `font-family-bold-italic` — style-specific family overrides
    /// (Ghostty); unset falls back to `font-family`.
    pub font_family_bold: Option<String>,
    pub font_family_italic: Option<String>,
    pub font_family_bold_italic: Option<String>,
    /// Named style of the primary family applied to regular text
    /// (Ghostty `font-style` — e.g. `Italic`, `Bold Italic`, `Light`).
    pub font_style: Option<String>,
    /// `font-style-bold`/`font-style-italic`/`font-style-bold-italic` —
    /// named styles that replace the fixed variant mapping for SGR
    /// bold/italic runs (e.g. `font-style-bold = Light` draws bold
    /// text in the family's Light face).
    pub font_style_bold: Option<String>,
    pub font_style_italic: Option<String>,
    pub font_style_bold_italic: Option<String>,
    /// `font-variation`/`font-variation-bold`/`font-variation-italic`/
    /// `font-variation-bold-italic` — OpenType variable-font axis settings
    /// per face, Ghostty `tag=value` comma syntax (e.g. `wght=700,wdth=85`).
    /// Each applies only to its face; unset faces get no variations.
    pub font_variation: Option<String>,
    pub font_variation_bold: Option<String>,
    pub font_variation_italic: Option<String>,
    pub font_variation_bold_italic: Option<String>,
    /// Allow fontique's synthesis for missing faces (Ghostty
    /// `font-synthetic`): `(embolden, oblique)`; `None` = both allowed
    /// (default). Each `font-synthetic = bold|italic|bold-italic` line
    /// ORs into the allow set; any line starts the set empty.
    pub font_synthetic: Option<(bool, bool)>,
    /// Per-codepoint family overrides (Ghostty `font-codepoint-map`):
    /// `U+AAAA[-U+BBBB]=Family Name` repeated; first match wins.
    pub font_codepoint_map: Vec<(u32, u32, String)>,
    /// `font-thicken` — overdraw every glyph run to darken strokes
    /// (Ghostty `font-thicken`, default false).
    pub font_thicken: bool,
    /// `font-thicken-strength` — 0-255 thicken amount (Ghostty: 0 is
    /// the lightest thickening, not off; inert unless `font-thicken`).
    pub font_thicken_strength: u8,
    pub scrollback: usize,
    /// Resolved at load; `Auto` consults the desktop once per (re)load.
    pub theme: ThemeRef,
    /// Where a finished selection is copied (Ghostty `copy-on-select`).
    pub copy_on_select: CopyOnSelect,
    /// `selection-clear-on-typing` — a keypress that produces PTY data
    /// (or an IME composition starting) drops the selection (Ghostty
    /// default true).
    pub selection_clear_on_typing: bool,
    /// `shell-integration` — which shell gets the injected hooks.
    pub shell_integration: ShellIntegration,
    /// `shell-integration-features` — the integration extras (cursor,
    /// sudo, title); each can be turned off with `no-<feature>`.
    pub shell_features: ShellFeatures,
    /// Hide the quick terminal when its window loses focus
    /// (Ghostty `quick-terminal-autohide`; default false on Linux,
    /// matching the reference's OS-dependent default).
    pub quick_terminal_autohide: bool,
    /// `quick-terminal-position` — drop-down dock edge (top default).
    pub quick_terminal_position: QuickTermPosition,
    /// `quick-terminal-size = <primary>[,<secondary>]` — primary axis
    /// (height for top/bottom, width for left/right, orientation for
    /// center); secondary axis is maximized for edge-docked positions
    /// unless a second value is given.
    pub quick_terminal_size: Option<(QuickTermSize, Option<QuickTermSize>)>,
    /// `quick-terminal-animation-duration` — seconds the drop-down's
    /// slide-in/out animation runs (Ghostty default 0.2; `= 0` disables).
    /// Edge-docked positions slide in from the dock edge; `center` mounts
    /// instantly (there is no edge to slide from).
    pub quick_terminal_animation_duration: f32,
    /// `clipboard-trim` — trim whitespace at the ends of copied text.
    pub clipboard_trim: bool,
    /// `notify-on-command-finish` — raise the 🔔 notification when a
    /// command completes (Ghostty; requires OSC 133 marks).
    pub notify_on_command_finish: NotifyWhen,
    /// `notify-on-command-finish-after` — minimum command duration
    /// before it notifies (seconds; Ghostty default 5).
    pub notify_on_command_finish_after: f64,
    /// `bell-features` `title` arm — prepend 🔔 to the window/tab
    /// title when the bell rings or a notification fires (Ghostty's
    /// `title` feature). Default on.
    pub bell_title: bool,
    pub cursor_shape: CursorShape,
    pub cursor_blink: bool,
    /// Shell program override; `None` = `$SHELL` with integration.
    pub shell: Option<String>,
    /// `command` — program for every new surface (Ghostty semantics:
    /// `direct:` splits verbatim argv, `shell:`/bare go through
    /// `sh -c`). `-e` maps to `initial-command`, not this.
    pub command: Option<Vec<String>>,
    /// `initial-command` — program for the first surface only
    /// (Ghostty `initial-command`); `-e`/`--` lands here.
    pub initial_command: Option<Vec<String>>,
    /// `osc-color-report-format` — component width in OSC 4/10/11/12
    /// report replies.
    pub osc_color_report_format: OscColorReportFormat,
    /// `keybind = <chord>=<action>` entries; `None` action = disabled.
    pub keybinds: Vec<(KeybindTrigger, Option<TermAction>)>,
    /// Set by `keybind = clear`: the built-in chord table
    /// (`action_chord`/`tab_chord`) no longer applies — only binds
    /// listed in the file (Ghostty `keybind = clear` semantics).
    pub keybinds_cleared: bool,
    /// Ring the X11 keyboard bell on `\a` (in addition to the visual flash).
    pub audible_bell: bool,
    /// `bell-features` `attention` arm — the tab's 🔔 notification badge on
    /// `TermEvent::Bell`/OSC 9 (Ghostty's request-attention feature; the
    /// badge is our attention channel). Default on; `no-attention` (or an
    /// empty `bell-features =`) turns it off.
    pub bell_attention: bool,
    /// `bell-features` `border` arm — draw a border ring around the
    /// alerted pane until it is re-focused or receives input (Ghostty's
    /// `border` feature). Default off.
    pub bell_border: bool,
    /// Window/transparency: alpha of the terminal's own background fill,
    /// 0.0 (invisible) ..= 1.0 (opaque). The winit window is created
    /// transparent when this starts below 1.0 — raising it live works;
    /// dropping it on an opaque-start window just darkens.
    pub background_opacity: f32,
    /// `background-opacity-cells` — when true the `background-opacity`
    /// alpha also applies to cells with an explicit (non-default)
    /// background color; false keeps cell backgrounds opaque over a
    /// translucent base (Ghostty `background-opacity-cells`).
    pub background_opacity_cells: bool,
    /// Guard multi-line clipboard pastes behind a confirmation overlay
    /// (Ghostty `clipboard-paste-protection`). Bracketed-paste-armed
    /// programs skip the guard — wrapped text can't execute mid-paste.
    pub paste_protection: bool,
    /// Hide the pointer while typing; it returns on the next move
    /// (Ghostty `mouse-hide-while-typing`). X11 only — XFixes HideCursor.
    pub mouse_hide_typing: bool,
    /// `mouse-shift-capture` — while a program reports the mouse
    /// (DECSET 1000/1002/1006), whether Shift reaches the program or
    /// bypasses reporting so click/drag selects terminal text
    /// (Ghostty `mouse-shift-capture`, default `false` = bypass).
    pub mouse_shift_capture: MouseShiftCapture,
    /// `clipboard-read` — policy for program clipboard reads via
    /// OSC 52 `?` requests (Ghostty `clipboard-read`, default ask).
    pub clipboard_read: ClipboardRead,
    /// `cursor-invert-fg-bg` — the block cursor swaps the cell's
    /// fg/bg so the glyph under it stays readable (Ghostty default on).
    pub cursor_invert_fg_bg: bool,
    /// `cursor-click-to-move` — a click on the cursor's row emits
    /// left/right arrow sequences repositioning the input cursor
    /// (Ghostty `cursor-click-to-move`, default off).
    pub cursor_click_to_move: bool,
    /// Blank space around the cell grid in points (Ghostty
    /// `window-padding-x` / `window-padding-y`); live-reloaded.
    pub window_padding_x: f32,
    pub window_padding_y: f32,
    /// `window-padding-balance` — center the cell grid inside the
    /// frame, splitting leftover space evenly between the two edges
    /// (Ghostty `window-padding-balance`, default false = leftover
    /// sits on the right/bottom).
    pub window_padding_balance: bool,
    /// `window-padding-color` — what the padding area is painted with
    /// (Ghostty `window-padding-color`, default `background`).
    pub window_padding_color: WindowPaddingColor,
    /// `middle-click-action` — what a middle click does (Ghostty,
    /// default `primary-paste`).
    pub middle_click_action: MiddleClickAction,
    /// `right-click-action` — secondary-click behaviour on a pane
    /// (Ghostty `right-click-action`, default `context-menu`).
    pub right_click_action: RightClickAction,
    /// `$TERM` the PTY advertises (Ghostty `term`).
    pub term: String,
    /// Whether programs may write the clipboard through OSC 52
    /// (Ghostty `clipboard-write` allow/deny).
    pub osc52_write: bool,
    /// `bold-color` — bold-text color override (Ghostty 1.2, replacing
    /// the deprecated `bold-is-bright`, which maps `true`→`bright`,
    /// `false`→unset). Default `bright`.
    pub bold_color: BoldColor,
    /// `faint-opacity` — opacity of faint (SGR 2) text, 0.0..=1.0
    /// (Ghostty default 0.5): blends the resolved fg toward the
    /// background.
    pub faint_opacity: f32,
    /// Initial working directory when no OSC 7 cwd was reported
    /// (Ghostty `working-directory`); `~` expands at load.
    pub working_directory: Option<PathBuf>,
    /// Alpha applied to a pane that is not the focused split
    /// (Ghostty `unfocused-split-opacity`), clamped to 0.0..=1.0.
    pub unfocused_split_opacity: f32,
    /// When to show the cols×rows badge while a pane resizes
    /// (Ghostty `resize-overlay`, default `after-first`).
    pub resize_overlay: ResizeOverlay,
    /// Where in the pane the badge sits
    /// (Ghostty `resize-overlay-position`, default `center`).
    pub resize_overlay_position: ResizeOverlayPosition,
    /// How long the badge stays after the last size change, in
    /// milliseconds (Ghostty `resize-overlay-duration`, default 750).
    pub resize_overlay_ms: u64,
    /// Pointer entering a pane records it as the focused split
    /// (Ghostty `focus-follows-mouse`). App-level record only —
    /// GUI key focus still needs a press until hydrolysis#126.
    pub focus_follows_mouse: bool,
    /// Theme color overrides (Ghostty `foreground` / `background` /
    /// `cursor-color` / `selection-foreground` / `palette = N=#rgb`);
    /// applied on top of the resolved theme, live-reloaded.
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    pub cursor_color: Option<Rgb>,
    /// Selection text color — `selection-foreground` sets the ink; when unset
    /// the selected cell's own background becomes the ink (inverted).
    pub selection_color: Option<Rgb>,
    /// `palette = 1=#ff0000` — indexed 0-255 slot overrides.
    pub palette_overrides: Vec<(u8, Rgb)>,
    /// `command-palette-entry` — custom rows appended to the command
    /// palette (Ghostty, repeat key).
    pub palette_entries: Vec<PaletteEntryCfg>,
    /// Wheel scroll speed multiplier (Ghostty `mouse-scroll-multiplier`).
    pub mouse_scroll_multiplier: f32,
    /// Ask before closing a pane/tab whose PTY foreground is a program
    /// other than the shell (Ghostty `confirm-close-surface`).
    pub confirm_close: ConfirmCloseSurface,
    /// Initial window content size in points; 0 = framework default.
    /// Applied once at launch (Ghostty `window-width`/`window-height`).
    pub window_width: f32,
    pub window_height: f32,
    /// Launch window position in points (Ghostty `window-position-x`/`y`);
    /// `None` = let the window manager choose. Applied once at launch.
    pub window_x: Option<f32>,
    pub window_y: Option<f32>,
    /// `link-url` — detect plain-text URLs on screen: Ctrl+hover
    /// underline + click-to-open + URL hints (Ghostty `link-url`,
    /// default true). `= false` disables all three paths.
    pub link_url: bool,
    /// `link-hover` — while the link modifier is held and the pointer
    /// is over a link, show the target URL in a status chip at the
    /// bottom-left of the pane (Ghostty `link-hover`, default true).
    /// `= false` hides the preview; the underline/click affordance
    /// still follows `link-url`.
    pub link_hover: bool,
    /// Modifier that must be held for click-to-open-link (Ghostty
    /// `open-link-modifier`-style); default Control.
    pub open_link_modifier: LinkMod,
    /// Multi-click (word/line select) timing window in milliseconds.
    pub click_interval: u64,
    /// Invert the selected cells' foreground/background (Ghostty
    /// `selection-invert-fg-bg`) — always swaps, even when
    /// `selection-foreground`/`selection-background` are configured.
    pub selection_invert: bool,
    /// Restore the last window geometry on launch and save it as the
    /// window moves/resizes (Ghostty `window-save-state`).
    pub window_save_state: bool,
    /// Launch every window fullscreen (Ghostty `fullscreen`).
    pub window_fullscreen: bool,
    /// Keep the surface open after a `command`/`-e` child exits
    /// (Ghostty `wait-after-command`, default false). Interactive
    /// shells always close their tab on exit.
    pub wait_after_command: bool,
    /// Child runtime in milliseconds below which a non-zero exit is
    /// abnormal — the surface then holds open with an error notice
    /// instead of closing silently (Ghostty
    /// `abnormal-command-exit-runtime`, default 250).
    pub abnormal_command_exit_runtime: u64,
    /// Whether programs may surface desktop notifications via OSC 9/777
    /// (Ghostty `desktop-notifications`, default true). Gates only the
    /// freedesktop notify-send hop; in-app badges still apply.
    pub desktop_notifications: bool,
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
    /// `selection-background`); `None` = the selected cell's own
    /// foreground (the reference default: swap fg/bg).
    pub selection_background: Option<Rgb>,
    /// `unfocused-split-fill` — background color painted under an
    /// unfocused split's cells (Ghostty `unfocused-split-fill`).
    pub unfocused_split_fill: Option<Rgb>,
    /// `title` — initial window/tab title; programs can still override
    /// it via OSC 0/1/2 (Ghostty `title`).
    pub title: Option<String>,
    /// `scroll-to-bottom` items (Ghostty): `keystroke` snaps to the
    /// live edge when a key writes bytes to the PTY (default on);
    /// `output` snaps on new program output while scrolled (default
    /// off — the reference documents the item but has not wired it;
    /// ours is implemented).
    pub scroll_bottom_keystroke: bool,
    pub scroll_bottom_output: bool,
    /// Alpha of the block cursor fill (Ghostty `cursor-opacity`, 0–1).
    pub cursor_opacity: f32,
    /// `env = NAME=VALUE` lines injected into spawned shells' environment.
    pub env: Vec<(String, String)>,
    /// Extra spacing per cell: `adjust-cell-width`/`adjust-cell-height`
    /// accept `N%` (of the measured cell) or `Npx` (absolute points).
    pub cell_width_adjust: CellAdjust,
    pub cell_height_adjust: CellAdjust,
    /// Cursor thickness multiplier for beam/underline shapes, percent
    /// (Ghostty `adjust-cursor-thickness`); 0 = the framework default.
    pub adjust_cursor_thickness: u16,
    /// Cursor height multiplier for beam/underline shapes, percent
    /// (Ghostty `adjust-cursor-height`); 0 = the framework default
    /// (underline 3pt at 100%, beam fills the cell).
    pub adjust_cursor_height: u16,
    /// Underline offset in points from the font's own position and a
    /// thickness multiplier percent (Ghostty `adjust-underline-position`
    /// / `adjust-underline-thickness`); 0 keeps the font values.
    pub adjust_underline_position: i16,
    pub adjust_underline_thickness: u16,
    /// Strikethrough offset/thickness, same shape as the underline pair
    /// (Ghostty `adjust-strikethrough-position` / `-thickness`).
    pub adjust_strikethrough_position: i16,
    pub adjust_strikethrough_thickness: u16,
    /// `window-subtitle` — fixed text appended to the window title after
    /// a separator; the OSC-driven part still leads (Ghostty
    /// `window-subtitle`).
    pub window_subtitle: Option<String>,
    /// `grapheme-width-method` — `legacy` skips the ZWJ-cluster join so
    /// each scalar keeps its own cells; `unicode` (default) folds the
    /// cluster into its head scalar's cells.
    pub grapheme_width_method: GraphemeWidthMethod,
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
    /// (Ghostty `window-theme`): `auto` picks light/dark from the
    /// *terminal background* luminance (the reference's `auto`),
    /// `system` follows the desktop color scheme, `light`/`dark` pin the
    /// Material scheme, `ghostty` seeds the whole M3 chrome from the
    /// terminal background at launch. The terminal palette stays on
    /// `theme =`.
    pub window_theme: WindowTheme,
    /// `window-title-font-family` — family used for the tab-strip
    /// titles (Ghostty's titlebar font; ours drives the WaterUI chips).
    pub window_title_font_family: Option<String>,
    /// `class` — the desktop identity (X11 `WM_CLASS`, Wayland `app_id`)
    /// every window groups under (Ghostty `class`; default =
    /// `com.mitchellh.ghostty`). Empty `class=` resets to the compiled
    /// `WATERUI_APP_ID`/executable name.
    pub app_class: Option<String>,
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
    /// non-key-bytes paths; `scroll-to-bottom = keystroke` covers key bytes).
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
    /// `visual-bell-color` — the bell flash overlay color (Ghostty);
    /// `None` = the theme foreground.
    pub visual_bell_color: Option<Rgb>,
    /// `open-link-with` — program run to open links. `{}` in an argument
    /// is replaced by the URL; otherwise the URL is appended as the
    /// last argument. Unset → `xdg-open`.
    pub open_link_with: Option<String>,
    /// `link` — extra clickable patterns (Ghostty's proposed
    /// `link = <regex>`); a matched span underlines on the
    /// `open-link-modifier` hover and opens through `open-link-with`.
    pub link_patterns: Vec<regex::Regex>,
    /// `enquiry-response` — reply bytes for a primary DA query
    /// (`CSI c`), C escapes decoded (`\e`, `\xNN`, `\n`). Empty/unset
    /// keeps the built-in `\x1b[?6c` (alacritty's VT102 answer).
    pub enquiry_response: Option<String>,
    /// `clipboard-paste-bracketed-safe` — when false, pasted text keeps
    /// its escape bytes verbatim (Ghostty's paranoia-off mode; default
    /// true strips them so a paste cannot escape the bracket).
    pub paste_bracketed_safe: bool,
    /// `image-storage-limit` — byte cap on the kitty graphics payload
    /// store (Ghostty, default 320 MB).
    pub image_storage_limit: usize,
    /// `config-file` — include paths recorded while expanding the config
    /// (repeat key; cycles and missing files warn instead of failing).
    pub config_files: Vec<PathBuf>,
    /// `app-notifications` — in-app toast toggles; the reference gates
    /// `clipboard-copy` and `config-reload` notifications this way
    /// (`no-<name>` disables; repeat key).
    pub app_notify_clipboard_copy: bool,
    pub app_notify_config_reload: bool,
    /// `selection-clear-on-copy` — clear the selection after an explicit
    /// `copy_to_clipboard`; `copy-on-select` never clears it.
    pub selection_clear_on_copy: bool,
    /// `undo-timeout` — ms a closed-surface undo entry stays restorable
    /// (Ghostty default 5s; `0` disables undo entirely).
    pub undo_timeout_ms: u64,
    /// `title-report` — answer `CSI 21 t` with `OSC l <title> ST`;
    /// default off (the reference treats it as an information leak).
    pub title_report: bool,
    /// `vt-kam-allowed` — let `CSI 2 h` (ANSI KAM) lock the keyboard.
    /// Ghostty default false: the request is refused and typing keeps
    /// reaching the program.
    pub vt_kam_allowed: bool,
    /// `search-foreground` / `search-background` (candidate matches).
    pub search_foreground: Option<Rgb>,
    pub search_background: Option<Rgb>,
    /// `search-selected-foreground` / `search-selected-background`
    /// (the focused match).
    pub search_selected_foreground: Option<Rgb>,
    pub search_selected_background: Option<Rgb>,
    /// `split-preserve-zoom` — `navigation` moves the zoom to the
    /// split `goto_split` focuses instead of unzooming; every layout
    /// change (split/close/resize/equalize) still unzooms (Ghostty
    /// 1.3 `navigation`; `no-navigation` disables, default).
    pub split_preserve_zoom_navigation: bool,
    /// `config-default-files` — when false, stop loading the remaining
    /// default locations (`$XDG_CONFIG_DIRS` then `$XDG_CONFIG_HOME`;
    /// a `--config` path already stands alone).
    pub config_default_files: bool,
}

/// `window-theme` values.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WindowTheme {
    /// Pick light/dark from the terminal background luminance — the
    /// reference's `auto` ("based on terminal background").
    Auto,
    Light,
    Dark,
    /// Follow the OS/desktop color-scheme preference.
    System,
    /// Chrome takes the terminal palette — the M3 style is seeded
    /// from the terminal background at launch (reference `ghostty`).
    Ghostty,
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
            font_family_set: false,
            font_family_bold: None,
            font_family_italic: None,
            font_family_bold_italic: None,
            font_style: None,
            font_style_bold: None,
            font_style_italic: None,
            font_style_bold_italic: None,
            font_variation: None,
            font_variation_bold: None,
            font_variation_italic: None,
            font_variation_bold_italic: None,
            font_synthetic: None,
            font_codepoint_map: Vec::new(),
            font_thicken: false,
            font_thicken_strength: 255,
            scrollback: 10_000,
            osc_color_report_format: OscColorReportFormat::Bits16,
            initial_command: None,
            quick_terminal_autohide: false,
            theme: ThemeRef::Named("hydroterm-dark".into()),
            copy_on_select: CopyOnSelect::Both,
            selection_clear_on_typing: true,
            shell_integration: ShellIntegration::Detect,
            shell_features: ShellFeatures { cursor: true, sudo: true, title: true },
            quick_terminal_position: QuickTermPosition::Top,
            quick_terminal_size: None,
            quick_terminal_animation_duration: 0.2,
            clipboard_trim: true,
            bell_title: true,
            grapheme_width_method: GraphemeWidthMethod::Unicode,
            cursor_shape: CursorShape::Block,
            cursor_blink: true,
            shell: None,
            command: None,
            keybinds: Vec::new(),
            keybinds_cleared: false,
            // `bell-features` defaults (Ghostty): `attention` and `title`
            // on, `system`/`audio`/`border` off.
            audible_bell: false,
            bell_attention: true,
            bell_border: false,
            background_opacity: 1.0,
            background_opacity_cells: false,
            paste_protection: true,
            mouse_hide_typing: true,
            mouse_shift_capture: MouseShiftCapture::False,
            notify_on_command_finish: NotifyWhen::No,
            notify_on_command_finish_after: 5.0,
            clipboard_read: ClipboardRead::Ask,
            cursor_invert_fg_bg: true,
            cursor_click_to_move: false,
            window_padding_x: 0.0,
            window_padding_y: 0.0,
            window_padding_balance: false,
            window_padding_color: WindowPaddingColor::Background,
            middle_click_action: MiddleClickAction::PrimaryPaste,
            right_click_action: RightClickAction::ContextMenu,
            term: "xterm-256color".to_string(),
            osc52_write: true,
            bold_color: BoldColor::Bright,
            faint_opacity: 0.5,
            working_directory: None,
            unfocused_split_opacity: 1.0,
            resize_overlay: ResizeOverlay::AfterFirst,
            resize_overlay_position: ResizeOverlayPosition::Center,
            resize_overlay_ms: 750,
            focus_follows_mouse: false,
            foreground: None,
            background: None,
            cursor_color: None,
            selection_color: None,
            palette_overrides: Vec::new(),
            palette_entries: Vec::new(),
            mouse_scroll_multiplier: 1.0,
            confirm_close: ConfirmCloseSurface::True,
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
            abnormal_command_exit_runtime: 250,
            desktop_notifications: true,
            quit_after_last_window_closed: true,
            split_divider_color: None,
            selection_background: None,
            unfocused_split_fill: None,
            title: None,
            cursor_text: None,
            scroll_bottom_keystroke: true,
            scroll_bottom_output: false,
            cursor_opacity: 1.0,
            env: Vec::new(),
            cell_width_adjust: CellAdjust::None,
            cell_height_adjust: CellAdjust::None,
            adjust_cursor_thickness: 0,
            adjust_cursor_height: 0,
            adjust_underline_position: 0,
            adjust_underline_thickness: 0,
            adjust_strikethrough_position: 0,
            adjust_strikethrough_thickness: 0,
            window_subtitle: None,
            font_baseline_adjust: CellAdjust::None,
            font_features: Vec::new(),
            minimum_contrast: 1.0,
            window_theme: WindowTheme::Auto,
            window_title_font_family: None,
            app_class: None,
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
            link_url: true,
            link_hover: true,
            word_select_chars: alacritty_terminal::term::SEMANTIC_ESCAPE_CHARS.to_string(),
            visual_bell: true,
            visual_bell_color: None,
            open_link_with: None,
            link_patterns: Vec::new(),
            enquiry_response: None,
            app_notify_clipboard_copy: true,
            app_notify_config_reload: true,
            selection_clear_on_copy: false,
            undo_timeout_ms: 5000,
            title_report: false,
            vt_kam_allowed: false,
            search_foreground: None,
            search_background: None,
            search_selected_foreground: None,
            search_selected_background: None,
            split_preserve_zoom_navigation: false,
            config_default_files: true,
            paste_bracketed_safe: true,
            image_storage_limit: 320 * 1024 * 1024,
            config_files: Vec::new(),
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

/// `background-opacity-cells` mapping: the alpha applied to cells with
/// an explicit background (Ghostty — `background-opacity` reaches cell
/// backgrounds only when the flag is on; otherwise they stay opaque).
pub fn cell_bg_alpha(cells_on: bool, opacity: f32) -> f32 {
    if cells_on { opacity } else { 1.0 }
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
window-padding-balance = false  # center the grid when it doesn't fill the frame
middle-click-action = primary-paste  # middle click pastes the PRIMARY selection
right-click-action = context-menu  # context-menu | copy | paste | ignore
font-thicken = false       # overdraw glyph runs to darken strokes
# window-title-font-family = DejaVu Sans   # tab-chip label font (the reference's titlebar font key)
# font-family-bold = DejaVu Sans Mono   # per-style family overrides
# font-family-italic = DejaVu Sans Mono
# font-family-bold-italic = DejaVu Sans Mono
term = xterm-256color      # $TERM value advertised to programs
clipboard-write = allow    # allow | deny — OSC 52 clipboard writes by programs
clipboard-read = ask       # allow | ask | deny — OSC 52 clipboard reads by programs
mouse-shift-capture = false # false | true | always | never — whether Shift reaches a mouse-reporting program
cursor-invert-fg-bg = true # block cursor swaps the cell's fg/bg
bold-color = bright        # bright | #rrggbb — bold-text color (unset = no override)
faint-opacity = 0.5        # faint (SGR 2) text opacity, 0.0-1.0
selection-clear-on-typing = true  # typing drops the selection highlight
unfocused-split-opacity = 1.0   # dim non-focused panes (0.0-1.0)
resize-overlay = after-first   # never | always | after-first — cols x rows chip while resizing
# resize-overlay-position = center   # center | top-left | top-center | top-right | bottom-left | bottom-center | bottom-right
# resize-overlay-duration = 750ms    # compound units: 1h30m, 45s, 250ms
focus-follows-mouse = false
# command = tmux attach    # program for every new surface (direct:/shell: prefixes)
# initial-command = neofetch  # program for the first surface only (`-e` lands here)
# quick-terminal-autohide = false   # hide the drop-down when its window loses focus
# osc-color-report-format = 16-bit  # 8-bit | 16-bit | none — OSC 4/10/11/12 reply width
# working-directory = ~/projects   # initial cwd when no OSC 7 report
# window-new-tab-position = end    # end | current — where new tabs insert
# window-inherit-working-directory = true  # new tab takes focused pane's OSC 7 cwd
# window-inherit-font-size = true  # new tab takes focused pane's live zoom

# Theme: auto | hydroterm-dark | hydroterm-light |
#        solarized-dark | solarized-light |
#        light:<name>,dark:<name>  — a theme per mode
# theme = light:solarized-light,dark:solarized-dark
theme = hydroterm-dark

cursor-style = block        # block | beam | underline | hollow
cursor-style-blink = true
copy-on-select = both       # both | clipboard | primary | false
shell-integration = detect  # detect | none | bash | zsh | fish
bell-features = system,audio,attention,title  # bell channels (no-X disables; `visual` = pane flash extension)
notify-on-command-finish = no   # no | unfocused | always — raise 🔔 when a command ends
notify-on-command-finish-after = 5s  # minimum command duration (500ms | 5s | 1m | 1h)
desktop-notifications = true    # OSC 9/777 may emit freedesktop notifications
abnormal-command-exit-runtime = 250  # ms — a command dying faster stays held open with a notice
grapheme-width-method = unicode # unicode | legacy — ZWJ clusters share cells vs per-scalar cells
# shell = /bin/bash

# Colors: overrides on top of the resolved theme
# (Ghostty foreground / background / palette).
# foreground = #ddeeff
# background = #101418
# cursor-color = #ffcc00
# selection-foreground = #ffffff
# selection-background = #3b4d5a   # selection highlight fill
# split-divider-color = #888888    # pane separator (default: theme Border)
# unfocused-split-fill = #2a2a2a   # bg of unfocused splits
# title = my-terminal              # initial window title (OSC can override)
# palette = 1=#e06c75   # indexed slot 0-255
# command-palette-entry = title:Foo, description:Bar, action:goto_tab:1  # extra palette row

mouse-scroll-multiplier = 1.0   # wheel scroll speed
confirm-close-surface = true  # ask before closing a running program (true|false|always)
# window-width = 800       # initial window size in points (0 = default)
# fullscreen = false         # start windows fullscreen
# window-height = 600
# window-save-state = true # remember window geometry across launches
# adjust-cell-width = 10%  # widen cells: N% or Npx
# adjust-cell-height = 2px
# adjust-font-baseline = 0px   # +Npx raises the text baseline; N% or Npx
# adjust-cursor-height = 120%  # underline thickness / beam height (0 = default)
# font-feature = -calt         # OpenType toggle: -tag off, +tag/tag/tag=N on
# font-style = Italic        # named style of font-family for regular text
# font-style-bold = Demi Bold      # named styles for the bold/italic/bold-italic variants
# font-style-italic = Light Italic
# font-style-bold-italic = Bold Italic
# font-variation = wght=400        # variable-font axes, per face: tag=value[,tag=value…]
# font-variation-bold = wght=700
# font-thicken-strength = 255      # 0-255 thicken amount when font-thicken = true
# font-synthetic-style = bold      # allow embolden synthesis; repeat for italic (or no-bold,no-italic,true,false)
# font-codepoint-map = U+2500-U+257F=DejaVu Sans Mono  # per-codepoint family
# minimum-contrast = 4.5   # 1.0-21.0 WCAG ratio floor on cell fg vs bg
# app-notifications = no-clipboard-copy   # gate in-app toasts: clipboard-copy | config-reload (no- disables)
# selection-clear-on-copy = true         # drop the selection after copy_to_clipboard
# undo-timeout = 5s                       # how long `undo` can reopen a closed surface (0 = undo off)
# split-preserve-zoom = navigation         # zoom follows goto_split instead of unzooming (default unzooms)
# config-default-files = true              # load $XDG_CONFIG_DIRS + $XDG_CONFIG_HOME configs (false stops the chain)
# title-report = false                    # let apps query the window title via CSI 21 t (OSC l <title> ST)
# search-foreground = #101418             # search match colors (selected-* = the focused match)
# search-background = #ffd75f
# search-selected-foreground = #101418
# search-selected-background = #ffaf00
# env = EDITOR=vim         # repeat to inject into spawned shells

# Keybinds: keybind = <chord>=<action>; empty action disables.
# keybind = clear                          # drop all binds incl. defaults; rebuild below
# chords: ctrl+shift+c, alt+enter, ...  actions: copy, paste,
# new_tab, close_tab, close_surface, new_window, next_tab, previous_tab,
# goto_tab:N, increase_font_size:N, decrease_font_size:N, reset_font_size,
# clear_scrollback, search, jump_to_prompt:±N, select_all, scroll_to_top,
# scroll_to_bottom, quit, new_split:right|down, goto_split:dir
# keybind = ctrl+alt+a=select_all
# keybind = global:ctrl+alt+u=toggle_quick_terminal   # X11 root grab — fires anywhere
# keybind = performable:ctrl+c=copy_to_clipboard      # fires only while copyable;
#                                                      # falls through to ^C otherwise
# keybind = unconsumed:alt+z=set_tab_title:hi          # fires AND ^[z reaches shell
# scroll-to-bottom = keystroke,output   # keystroke on by default; output off
# tab-bar-min-tabs = 2    # hide the tab strip until N tabs exist
# selection-word-chars = ,│`|:\"' ()[]{}<>\t   # double-click word separators
# open-link-with = firefox --new-window {}   # {} = the URL (default xdg-open)
# quick-terminal-position = top   # top | bottom | left | right | center
# quick-terminal-size = 45%       # N% or Npx[, second axis] — primary axis
#                                  # is height for top/bottom, width for
#                                  # left/right; edge docks maximize the rest
# shell-integration-features = cursor,sudo,title   # prefix a feature with no- to disable
# clipboard-trim-trailing-spaces = true   # trim whitespace at the ends of copied text
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
            ThemeRef::Pair { light, dark } => {
                let name = if crate::theme::system_prefers_light() {
                    light
                } else {
                    dark
                };
                Theme::by_name(name).unwrap_or_else(Theme::hydroterm_dark)
            }
        }
    }

    /// Find a configured binding for this key press. `Some((trigger,
    /// action))` where the trigger carries the Ghostty prefix flags
    /// (`global:`/`all:`/`unconsumed:`/`performable:`); the `None`
    /// action is an explicit `unbind`, `None` overall = no entry.
    /// `global:` binds match here too — while the root grab is live the
    /// grabbed key never reaches the window, so there is no double-fire.
    /// `physical:` triggers compare `code` (the key POSITION), not the
    /// layout-translated character — their `chord` field carries only
    /// the modifier prefix for compare (`<mods>physical:<name>`).
    pub fn lookup_keybind(
        &self,
        key: &Key,
        code: Code,
        mods: Modifiers,
    ) -> Option<(KeybindTrigger, Option<TermAction>)> {
        let chord = chord_of(key, mods);
        let mods_part = mods_prefix(mods);
        self.keybinds
            .iter()
            .rev() // last wins
            .find(|(t, _)| match t.physical_code {
                // Physical binds fire on key position regardless of the
                // layout-translated character (unnamed keys included).
                Some(pc) => pc == code && chord_mods(&t.chord) == mods_part,
                None => chord.as_deref() == Some(t.chord.as_str()),
            })
            .map(|(t, a)| (t.clone(), a.clone()))
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
            // A fully-quoted scalar unwraps (`shell = "/bin/zsh"`) —
            // quotes *inside* the value are content, not wrapping:
            // `keybind = c=text:"a b"` keeps its payload quotes.
            let value = value.trim();
            let value = if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
                &value[1..value.len() - 1]
            } else {
                value
            };
            match key.as_str() {
                "font-size" => match value.parse::<f32>() {
                    Ok(v) if (4.0..=96.0).contains(&v) => cfg.font_size = v,
                    _ => errors.push(format!("line {}: bad font-size {value:?}", n + 1)),
                },
                "font-family" => {
                    // Repeatable (Ghostty): each additional `font-family`
                    // line appends to the primary family's ordered
                    // fallback chain. `font_family` carries the chain
                    // comma-joined — `TermFonts::load` splits on ','.
                    if cfg.font_family_set {
                        cfg.font_family.push(',');
                        cfg.font_family.push_str(value);
                    } else {
                        cfg.font_family = value.to_string();
                        cfg.font_family_set = true;
                    }
                }
                "font-family-bold" => cfg.font_family_bold = Some(value.to_string()),
                "font-family-italic" => cfg.font_family_italic = Some(value.to_string()),
                "font-family-bold-italic" => {
                    cfg.font_family_bold_italic = Some(value.to_string());
                }
                "font-thicken" => cfg.font_thicken = bool_value(value, n, &mut errors),
                "font-thicken-strength" => match value.parse::<u8>() {
                    Ok(s) => cfg.font_thicken_strength = s,
                    Err(_) => errors.push(format!(
                        "line {}: bad font-thicken-strength {value:?} (want 0-255)",
                        n + 1
                    )),
                },
                "font-style" => cfg.font_style = Some(value.to_string()),
                "font-style-bold" => cfg.font_style_bold = Some(value.to_string()),
                "font-style-italic" => cfg.font_style_italic = Some(value.to_string()),
                "font-style-bold-italic" => {
                    cfg.font_style_bold_italic = Some(value.to_string());
                }
                "font-variation" => cfg.font_variation = Some(value.to_string()),
                "font-variation-bold" => cfg.font_variation_bold = Some(value.to_string()),
                "font-variation-italic" => {
                    cfg.font_variation_italic = Some(value.to_string());
                }
                "font-variation-bold-italic" => {
                    cfg.font_variation_bold_italic = Some(value.to_string());
                }
                "font-synthetic-style" => {
                    // Ghostty's form is `no-bold`,`no-italic`,`no-bold-italic`
                    // disables and `true`/`false` covers both; bare
                    // `bold`/`italic` are the allow-list form; an empty
                    // value allows nothing.
                    if value.trim().is_empty() {
                        cfg.font_synthetic = Some((false, false));
                    }
                    for tok in value.split(&[',', '|'][..]).map(|t| t.trim().to_ascii_lowercase()) {
                        match tok.as_str() {
                            "true" => {
                                cfg.font_synthetic = Some((true, true));
                            }
                            "false" => {
                                cfg.font_synthetic = Some((false, false));
                            }
                            "bold" => {
                                cfg.font_synthetic.get_or_insert((false, false)).0 = true;
                            }
                            "italic" => {
                                cfg.font_synthetic.get_or_insert((false, false)).1 = true;
                            }
                            "no-bold" => {
                                cfg.font_synthetic.get_or_insert((true, true)).0 = false;
                            }
                            "no-italic" => {
                                cfg.font_synthetic.get_or_insert((true, true)).1 = false;
                            }
                            "no-bold-italic" => {
                                cfg.font_synthetic = Some((false, false));
                            }
                            "" => {}
                            _ => errors.push(format!(
                                "line {}: bad font-synthetic-style token {tok:?}",
                                n + 1
                            )),
                        }
                    }
                }
                "font-codepoint-map" => {
                    match parse_codepoint_map(value) {
                        Some((lo, hi, fam)) => cfg.font_codepoint_map.push((lo, hi, fam)),
                        None => errors.push(format!(
                            "line {}: bad font-codepoint-map {value:?} (want U+AAAA[-U+BBBB]=Family)",
                            n + 1
                        )),
                    }
                }
                "middle-click-action" => {
                    cfg.middle_click_action = match value {
                        "primary-paste" => MiddleClickAction::PrimaryPaste,
                        "clipboard-paste" => MiddleClickAction::ClipboardPaste,
                        "ignore" => MiddleClickAction::Ignore,
                        _ => {
                            errors.push(format!(
                                "line {}: bad middle-click-action {value:?} (want primary-paste|clipboard-paste|ignore)",
                                n + 1
                            ));
                            cfg.middle_click_action
                        }
                    };
                }
                "right-click-action" => {
                    cfg.right_click_action = match value {
                        "context-menu" => RightClickAction::ContextMenu,
                        "copy" => RightClickAction::Copy,
                        "paste" => RightClickAction::Paste,
                        "ignore" => RightClickAction::Ignore,
                        _ => {
                            errors.push(format!(
                                "line {}: bad right-click-action {value:?}",
                                n + 1
                            ));
                            cfg.right_click_action
                        }
                    };
                }
                // `scrollback-limit-lines` is the reference's canonical
                // name (1.4 renamed `scrollback-limit` to `-bytes` and
                // split units); our line cap shares the slot.
                "scrollback" | "scrollback-limit-lines" | "scrollback-limit" => {
                    match value.parse::<usize>() {
                        Ok(v) => cfg.scrollback = v.min(1_000_000),
                        Err(_) => errors.push(format!(
                            "line {}: bad scrollback {value:?}",
                            n + 1
                        )),
                    }
                }
                "scrollback-limit-bytes" => {
                    // Byte cap is alacritty's — we carry a line cap only;
                    // parse so the key isn't an unknown-key error.
                    if value.parse::<usize>().is_err() {
                        errors.push(format!(
                            "line {}: bad scrollback-limit-bytes {value:?}",
                            n + 1
                        ));
                    }
                }
                "background-opacity" => match value.parse::<f32>() {
                    Ok(v) if (0.0..=1.0).contains(&v) => cfg.background_opacity = v,
                    _ => errors.push(format!("line {}: bad background-opacity {value:?}", n + 1)),
                },
                "clipboard-paste-protection" => match value {
                    "true" | "1" => cfg.paste_protection = true,
                    "false" | "0" => cfg.paste_protection = false,
                    _ => errors.push(format!("line {}: bad clipboard-paste-protection {value:?}", n + 1)),
                },
                "window-padding-x" => match value.parse::<f32>() {
                    Ok(v) if (0.0..=200.0).contains(&v) => cfg.window_padding_x = v,
                    _ => errors.push(format!("line {}: bad window-padding-x {value:?}", n + 1)),
                },
                "window-padding-y" => match value.parse::<f32>() {
                    Ok(v) if (0.0..=200.0).contains(&v) => cfg.window_padding_y = v,
                    _ => errors.push(format!("line {}: bad window-padding-y {value:?}", n + 1)),
                },
                "window-padding-balance" => {
                    cfg.window_padding_balance = bool_value(value, n, &mut errors);
                }
                "term" => cfg.term = value.to_string(),
                "foreground" => match parse_rgb(value) {
                    Some(c) => cfg.foreground = Some(c),
                    None => errors.push(format!("line {}: bad foreground {value:?}", n + 1)),
                },
                "background" => match parse_rgb(value) {
                    Some(c) => cfg.background = Some(c),
                    None => errors.push(format!("line {}: bad background {value:?}", n + 1)),
                },
                "cursor-color" => match parse_rgb(value) {
                    Some(c) => cfg.cursor_color = Some(c),
                    None => errors.push(format!("line {}: bad cursor-color {value:?}", n + 1)),
                },
                "selection-foreground" => {
                    match parse_rgb(value) {
                        Some(c) => cfg.selection_color = Some(c),
                        None => {
                            errors.push(format!("line {}: bad selection-foreground {value:?}", n + 1))
                        }
                    }
                }
                "selection-background" => {
                    match parse_rgb(value) {
                        Some(c) => cfg.selection_background = Some(c),
                        None => errors
                            .push(format!("line {}: bad selection-background {value:?}", n + 1)),
                    }
                }
                "split-divider-color" => {
                    match parse_rgb(value) {
                        Some(c) => cfg.split_divider_color = Some(c),
                        None => errors
                            .push(format!("line {}: bad split-divider-color {value:?}", n + 1)),
                    }
                }
                "unfocused-split-fill" => {
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
                "command-palette-entry" => match parse_palette_entry(value) {
                    Ok(entry) => cfg.palette_entries.push(entry),
                    Err(e) => errors.push(format!("line {}: {e}", n + 1)),
                },
                "mouse-scroll-multiplier" => {
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
                "confirm-close-surface" => {
                    cfg.confirm_close = match value {
                        "true" | "on" => ConfirmCloseSurface::True,
                        "false" | "off" => ConfirmCloseSurface::False,
                        "always" => ConfirmCloseSurface::Always,
                        _ => {
                            errors.push(format!(
                                "line {}: bad confirm-close-surface {value:?} (want true|false|always)",
                                n + 1
                            ));
                            cfg.confirm_close
                        }
                    };
                }
                "clipboard-write" => match value {
                    "allow" | "true" | "1" => cfg.osc52_write = true,
                    "deny" | "false" | "0" => cfg.osc52_write = false,
                    _ => errors.push(format!("line {}: bad clipboard-write {value:?}", n + 1)),
                },
                "clipboard-read" => match value {
                    "allow" | "always" => cfg.clipboard_read = ClipboardRead::Allow,
                    "ask" => cfg.clipboard_read = ClipboardRead::Ask,
                    "deny" | "never" => cfg.clipboard_read = ClipboardRead::Deny,
                    _ => errors.push(format!("line {}: bad clipboard-read {value:?}", n + 1)),
                },
                "notify-on-command-finish" => {
                    cfg.notify_on_command_finish = match value {
                        "no" => NotifyWhen::No,
                        "unfocused" => NotifyWhen::Unfocused,
                        "always" => NotifyWhen::Always,
                        _ => {
                            errors.push(format!("line {}: bad notify-on-command-finish {value:?}", n + 1));
                            cfg.notify_on_command_finish
                        }
                    };
                }
                "notify-on-command-finish-after" => {
                    match parse_notify_after(value) {
                        Some(s) => cfg.notify_on_command_finish_after = s,
                        None => errors.push(format!(
                            "line {}: bad notify-on-command-finish-after {value:?}",
                            n + 1
                        )),
                    }
                }
                "mouse-shift-capture" => {
                    cfg.mouse_shift_capture = match value {
                        "false" => MouseShiftCapture::False,
                        "true" => MouseShiftCapture::True,
                        "always" => MouseShiftCapture::Always,
                        "never" => MouseShiftCapture::Never,
                        _ => {
                            errors.push(format!("line {}: bad mouse-shift-capture {value:?}", n + 1));
                            cfg.mouse_shift_capture
                        }
                    };
                }
                "cursor-invert-fg-bg" => {
                    cfg.cursor_invert_fg_bg = bool_value(value, n, &mut errors);
                }
                "cursor-click-to-move" => {
                    cfg.cursor_click_to_move = bool_value(value, n, &mut errors);
                }
                "bold-color" => {
                    if value.eq_ignore_ascii_case("bright") {
                        cfg.bold_color = BoldColor::Bright;
                    } else if let Some(rgb) = parse_rgb(value) {
                        cfg.bold_color = BoldColor::Color(rgb);
                    } else {
                        errors.push(format!(
                            "line {}: bad bold-color {value:?} (bright or #rrggbb)",
                            n + 1
                        ));
                    }
                }
                "faint-opacity" => match value.parse::<f32>() {
                    Ok(v) => cfg.faint_opacity = v.clamp(0.0, 1.0),
                    Err(_) => {
                        errors.push(format!("line {}: bad faint-opacity {value:?}", n + 1))
                    }
                },
                "selection-clear-on-typing" => {
                    cfg.selection_clear_on_typing = bool_value(value, n, &mut errors);
                }
                "mouse-hide-while-typing" => match value {
                    "true" | "1" => cfg.mouse_hide_typing = true,
                    "false" | "0" => cfg.mouse_hide_typing = false,
                    _ => errors.push(format!("line {}: bad mouse-hide-while-typing {value:?}", n + 1)),
                },
                "theme" => {
                    if value.eq_ignore_ascii_case("auto") {
                        cfg.theme = ThemeRef::Auto;
                    } else if let Some((light, dark)) = parse_theme_pair(value) {
                        cfg.theme = ThemeRef::Pair { light, dark };
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
                    "true" | "on" => CopyOnSelect::Both,
                    "false" | "no" | "disabled" => CopyOnSelect::Disabled,
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
                "quick-terminal-size" => match parse_quick_term_size(value) {
                    Ok(s) => cfg.quick_terminal_size = s,
                    Err(msg) => errors.push(format!("line {}: {msg}", n + 1)),
                },
                "quick-terminal-animation-duration" => match value.parse::<f32>() {
                    Ok(v) if (0.0..=5.0).contains(&v) => {
                        cfg.quick_terminal_animation_duration = v;
                    }
                    _ => errors.push(format!(
                        "line {}: bad quick-terminal-animation-duration {value:?}",
                        n + 1
                    )),
                },
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
                "clipboard-trim-trailing-spaces" => match value {
                    "true" | "yes" => cfg.clipboard_trim = true,
                    "false" | "no" => cfg.clipboard_trim = false,
                    _ => errors.push(format!(
                        "line {}: bad clipboard-trim-trailing-spaces {value:?}",
                        n + 1
                    )),
                },
                // Ghostty `bell-features` — a comma list naming the enabled
                // bell channels; `no-<name>` disables and an empty value
                // turns every channel off (the reference's packed-set
                // semantics: each line applies its items to the set).
                // `system`/`audio` → the X11 bell (audio's custom sound
                // file isn't played), `attention` → the tab 🔔 badge,
                // `title` → 🔔 in the title, `border` → a border ring.
                "bell-features" => {
                    if value.trim().is_empty() {
                        cfg.audible_bell = false;
                        cfg.bell_attention = false;
                        cfg.bell_title = false;
                        cfg.bell_border = false;
                    }
                    for feature in value.split(',') {
                        let feature = feature.trim();
                        if feature.is_empty() {
                            continue;
                        }
                        let (on, name) = match feature.strip_prefix("no-") {
                            Some(rest) => (false, rest),
                            None => (true, feature),
                        };
                        match name {
                            "system" | "audio" => cfg.audible_bell = on,
                            "attention" => cfg.bell_attention = on,
                            "title" => cfg.bell_title = on,
                            "border" => cfg.bell_border = on,
                            _ => errors.push(format!(
                                "line {}: bad bell-features item {feature:?}",
                                n + 1
                            )),
                        }
                    }
                }
                "cursor-style-blink" => {
                    cfg.cursor_blink = bool_value(value, n, &mut errors)
                }
                "cursor-style" => match value {
                    "block" => cfg.cursor_shape = CursorShape::Block,
                    "beam" => cfg.cursor_shape = CursorShape::Beam,
                    "underline" => cfg.cursor_shape = CursorShape::Underline,
                    "hollow-block" => cfg.cursor_shape = CursorShape::HollowBlock,
                    _ => errors.push(format!("line {}: bad cursor-style {value:?}", n + 1)),
                },
                "shell" => cfg.shell = (!value.is_empty()).then(|| value.to_string()),
                "command" => {
                    cfg.command = parse_command(value);
                }
                "initial-command" => {
                    cfg.initial_command = parse_command(value);
                }
                "working-directory" => {
                    cfg.working_directory = (!value.is_empty()).then(|| expand_home(value));
                }
                "unfocused-split-opacity" => {
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
                "resize-overlay" => match value {
                    "never" | "false" => cfg.resize_overlay = ResizeOverlay::Never,
                    "always" | "true" => cfg.resize_overlay = ResizeOverlay::Always,
                    "after-first" => cfg.resize_overlay = ResizeOverlay::AfterFirst,
                    _ => errors.push(format!(
                        "line {}: bad resize-overlay {value:?} (want never|always|after-first)",
                        n + 1
                    )),
                },
                "resize-overlay-position" => match value {
                    "center" => {
                        cfg.resize_overlay_position = ResizeOverlayPosition::Center;
                    }
                    "top-left" => {
                        cfg.resize_overlay_position = ResizeOverlayPosition::TopLeft;
                    }
                    "top-center" => {
                        cfg.resize_overlay_position = ResizeOverlayPosition::TopCenter;
                    }
                    "top-right" => {
                        cfg.resize_overlay_position = ResizeOverlayPosition::TopRight;
                    }
                    "bottom-left" => {
                        cfg.resize_overlay_position = ResizeOverlayPosition::BottomLeft;
                    }
                    "bottom-center" => {
                        cfg.resize_overlay_position = ResizeOverlayPosition::BottomCenter;
                    }
                    "bottom-right" => {
                        cfg.resize_overlay_position = ResizeOverlayPosition::BottomRight;
                    }
                    _ => errors.push(format!(
                        "line {}: bad resize-overlay-position {value:?}",
                        n + 1
                    )),
                },
                "resize-overlay-duration" => match parse_duration_ms(value) {
                    Some(ms) => cfg.resize_overlay_ms = ms,
                    None => errors.push(format!(
                        "line {}: bad resize-overlay-duration {value:?} (want e.g. 750ms)",
                        n + 1
                    )),
                },
                "osc-color-report-format" => match value {
                    "none" => cfg.osc_color_report_format = OscColorReportFormat::None,
                    "8-bit" => cfg.osc_color_report_format = OscColorReportFormat::Bits8,
                    "16-bit" => cfg.osc_color_report_format = OscColorReportFormat::Bits16,
                    _ => errors.push(format!(
                        "line {}: bad osc-color-report-format {value:?}",
                        n + 1
                    )),
                },
                "quick-terminal-autohide" => {
                    cfg.quick_terminal_autohide = bool_value(value, n, &mut errors);
                }
                "focus-follows-mouse" => {
                    cfg.focus_follows_mouse = bool_value(value, n, &mut errors);
                }
                "window-width" => match value.parse::<f32>() {
                    Ok(v) if (0.0..=4000.0).contains(&v) => cfg.window_width = v,
                    _ => errors.push(format!("line {}: bad window-width {value:?}", n + 1)),
                },
                "window-height" => match value.parse::<f32>() {
                    Ok(v) if (0.0..=4000.0).contains(&v) => cfg.window_height = v,
                    _ => errors.push(format!("line {}: bad window-height {value:?}", n + 1)),
                },
                "window-position-x" => match value.parse::<f32>() {
                    Ok(v) if (-2000.0..=8000.0).contains(&v) => cfg.window_x = Some(v),
                    _ => errors.push(format!("line {}: bad window-position-x {value:?}", n + 1)),
                },
                "window-position-y" => match value.parse::<f32>() {
                    Ok(v) if (-2000.0..=8000.0).contains(&v) => cfg.window_y = Some(v),
                    _ => errors.push(format!("line {}: bad window-position-y {value:?}", n + 1)),
                },
                "link-url" => {
                    cfg.link_url = bool_value(value, n, &mut errors);
                }
                "link-hover" => {
                    cfg.link_hover = bool_value(value, n, &mut errors);
                }
                "open-link-modifier" => {
                    match value.parse::<LinkMod>() {
                        Ok(m) => cfg.open_link_modifier = m,
                        Err(e) => errors.push(format!("line {}: {e}", n + 1)),
                    }
                }
                "click-repeat-interval" => match value.parse::<u64>() {
                    Ok(v) if (50..=2000).contains(&v) => cfg.click_interval = v,
                    _ => errors.push(format!("line {}: bad click-repeat-interval {value:?}", n + 1)),
                },
                "selection-invert-fg-bg" => {
                    cfg.selection_invert = bool_value(value, n, &mut errors);
                }
                "window-save-state" => {
                    cfg.window_save_state = bool_value(value, n, &mut errors);
                }
                "fullscreen" => {
                    cfg.window_fullscreen = bool_value(value, n, &mut errors);
                }
                "wait-after-command" => {
                    cfg.wait_after_command = bool_value(value, n, &mut errors);
                }
                "quit-after-last-window-closed" => {
                    cfg.quit_after_last_window_closed = bool_value(value, n, &mut errors);
                }
                "window-theme" => match value {
                    "auto" => cfg.window_theme = WindowTheme::Auto,
                    "system" => cfg.window_theme = WindowTheme::System,
                    "light" => cfg.window_theme = WindowTheme::Light,
                    "dark" => cfg.window_theme = WindowTheme::Dark,
                    "ghostty" => cfg.window_theme = WindowTheme::Ghostty,
                    _ => errors.push(format!(
                        "line {}: bad window-theme {value:?} (want auto|system|light|dark|ghostty)",
                        n + 1
                    )),
                },
                "window-title-font-family" => {
                    cfg.window_title_font_family = if value.is_empty() {
                        None
                    } else {
                        Some(value.to_string())
                    };
                }
                // `class` — WM_CLASS / app_id for every window
                // (water-rs/waterui#1291 → `Window::app_id`).
                "class" => {
                    cfg.app_class = if value.is_empty() {
                        None
                    } else {
                        Some(value.to_string())
                    };
                }
                "window-decoration" => {
                    cfg.window_decoration = match value {
                        "false" => false,
                        "true" | "client" => true,
                        _ => bool_value(value, n, &mut errors),
                    };
                }
                "background-image" => {
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
                "background-image-opacity" => {
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
                "background-image-fit" => match value {
                    "contain" => cfg.background_image_fit = BgFit::Contain,
                    "cover" => cfg.background_image_fit = BgFit::Cover,
                    "stretch" => cfg.background_image_fit = BgFit::Stretch,
                    "tile" => cfg.background_image_fit = BgFit::Tile,
                    _ => errors.push(format!(
                        "line {}: bad background-image-fit {value:?}",
                        n + 1
                    )),
                },
                "background-image-repeat" => {
                    cfg.background_image_repeat = bool_value(value, n, &mut errors);
                }
                "window-new-tab-position" => {
                    match value {
                        "end" => cfg.new_tab_position = NewTabPosition::End,
                        "current" => cfg.new_tab_position = NewTabPosition::Current,
                        _ => errors.push(format!(
                            "line {}: bad window-new-tab-position {value:?} (end|current)",
                            n + 1
                        )),
                    }
                }
                "window-inherit-working-directory" => {
                    cfg.inherit_working_directory = bool_value(value, n, &mut errors);
                }
                "window-inherit-font-size" => {
                    cfg.inherit_font_size = bool_value(value, n, &mut errors);
                }
                "scroll-to-cursor" => {
                    cfg.scroll_to_cursor = bool_value(value, n, &mut errors);
                }
                "cursor-text" => match parse_rgb(value) {
                    Some(rgb) => cfg.cursor_text = Some(rgb),
                    None => errors.push(format!("line {}: bad cursor-text {value:?}", n + 1)),
                },
                "background-opacity-cells" => {
                    cfg.background_opacity_cells = bool_value(value, n, &mut errors)
                }
                "window-padding-color" => match value {
                    "background" => cfg.window_padding_color = WindowPaddingColor::Background,
                    "extend" => cfg.window_padding_color = WindowPaddingColor::Extend,
                    "extend-always" => cfg.window_padding_color = WindowPaddingColor::ExtendAlways,
                    _ => errors.push(format!("line {}: bad window-padding-color {value:?}", n + 1)),
                },
                // Ghostty `scroll-to-bottom = keystroke,output` — a
                // comma set with `no-` negations (empty = nothing
                // scrolls you).
                "scroll-to-bottom" => {
                    if value.trim().is_empty() {
                        cfg.scroll_bottom_keystroke = false;
                        cfg.scroll_bottom_output = false;
                    }
                    for item in value.split(',') {
                        let item = item.trim();
                        if item.is_empty() {
                            continue;
                        }
                        let (on, name) = match item.strip_prefix("no-") {
                            Some(rest) => (false, rest),
                            None => (true, item),
                        };
                        match name {
                            "keystroke" => cfg.scroll_bottom_keystroke = on,
                            "output" => cfg.scroll_bottom_output = on,
                            _ => errors.push(format!(
                                "line {}: bad scroll-to-bottom item {item:?}",
                                n + 1
                            )),
                        }
                    }
                }
                "cursor-opacity" => match value.parse::<f32>() {
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
                "adjust-cell-width" => match parse_cell_adjust(value) {
                    Some(pct) => cfg.cell_width_adjust = pct,
                    None => errors.push(format!(
                        "line {}: bad adjust-cell-width {value:?} (want N% or Npx)",
                        n + 1
                    )),
                },
                "adjust-cell-height" => match parse_cell_adjust(value) {
                    Some(pct) => cfg.cell_height_adjust = pct,
                    None => errors.push(format!(
                        "line {}: bad adjust-cell-height {value:?} (want N% or Npx)",
                        n + 1
                    )),
                },
                "adjust-font-baseline" => match parse_cell_adjust(value) {
                    Some(pct) => cfg.font_baseline_adjust = pct,
                    None => errors.push(format!(
                        "line {}: bad adjust-font-baseline {value:?} (want N% or Npx)",
                        n + 1
                    )),
                },
                "adjust-cursor-thickness" => {
                    match value.parse::<u16>() {
                        Ok(p) => cfg.adjust_cursor_thickness = p,
                        Err(_) => errors.push(format!(
                            "line {}: bad adjust-cursor-thickness {value:?} (want percent)",
                            n + 1
                        )),
                    }
                }
                "adjust-cursor-height" => match value
                    .strip_suffix('%')
                    .unwrap_or(value)
                    .parse::<u16>()
                {
                    Ok(p) => cfg.adjust_cursor_height = p,
                    Err(_) => errors.push(format!(
                        "line {}: bad adjust-cursor-height {value:?} (want percent)",
                        n + 1
                    )),
                },
                "desktop-notifications" => {
                    cfg.desktop_notifications = bool_value(value, n, &mut errors);
                }
                "abnormal-command-exit-runtime" => match value.parse::<u64>() {
                    Ok(ms) => cfg.abnormal_command_exit_runtime = ms,
                    Err(_) => errors.push(format!(
                        "line {}: bad abnormal-command-exit-runtime {value:?} (want ms)",
                        n + 1
                    )),
                },
                "grapheme-width-method" => match value {
                    "unicode" => cfg.grapheme_width_method = GraphemeWidthMethod::Unicode,
                    "legacy" => cfg.grapheme_width_method = GraphemeWidthMethod::Legacy,
                    _ => errors.push(format!(
                        "line {}: bad grapheme-width-method {value:?} (want unicode|legacy)",
                        n + 1
                    )),
                },
                "adjust-underline-position" => match value.parse::<i16>() {
                    Ok(p) => cfg.adjust_underline_position = p,
                    Err(_) => errors.push(format!(
                        "line {}: bad adjust-underline-position {value:?} (want points)",
                        n + 1
                    )),
                },
                "adjust-underline-thickness" => match value.parse::<u16>() {
                    Ok(p) => cfg.adjust_underline_thickness = p,
                    Err(_) => errors.push(format!(
                        "line {}: bad adjust-underline-thickness {value:?} (want percent)",
                        n + 1
                    )),
                },
                "adjust-strikethrough-position" => {
                    match value.parse::<i16>() {
                        Ok(p) => cfg.adjust_strikethrough_position = p,
                        Err(_) => errors.push(format!(
                            "line {}: bad adjust-strikethrough-position {value:?} (want points)",
                            n + 1
                        )),
                    }
                }
                "adjust-strikethrough-thickness" => {
                    match value.parse::<u16>() {
                        Ok(p) => cfg.adjust_strikethrough_thickness = p,
                        Err(_) => errors.push(format!(
                            "line {}: bad adjust-strikethrough-thickness {value:?} (want percent)",
                            n + 1
                        )),
                    }
                }
                "window-subtitle" => {
                    if value.is_empty() {
                        cfg.window_subtitle = None;
                    } else {
                        cfg.window_subtitle = Some(value.to_string());
                    }
                }
                "font-feature" => {
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
                "minimum-contrast" => match value.parse::<f32>() {
                    Ok(v) if (1.0..=21.0).contains(&v) => cfg.minimum_contrast = v,
                    _ => errors.push(format!(
                        "line {}: bad minimum-contrast {value:?} (want 1.0-21.0)",
                        n + 1
                    )),
                },
                "window-show-tab-bar" => match value {
                    "always" => cfg.tab_bar_min_tabs = 1,
                    "auto" => cfg.tab_bar_min_tabs = 2,
                    "never" => cfg.tab_bar_min_tabs = usize::MAX,
                    _ => errors.push(format!("line {}: bad window-show-tab-bar {value:?}", n + 1)),
                },
                "tab-bar-min-tabs" => match value.parse::<usize>() {
                    Ok(v) if v <= 64 => cfg.tab_bar_min_tabs = v,
                    _ => errors.push(format!("line {}: bad tab-bar-min-tabs {value:?}", n + 1)),
                },
                "selection-word-chars" => {
                    cfg.word_select_chars =
                        value.replace("\\t", "\t").replace("\\n", "\n");
                }
                "visual-bell-color" => match parse_rgb(value) {
                    Some(rgb) => cfg.visual_bell_color = Some(rgb),
                    None => errors.push(format!(
                        "line {}: bad visual-bell-color {value:?}",
                        n + 1
                    )),
                },
                "open-link-with" => {
                    cfg.open_link_with = (!value.is_empty()).then(|| value.to_string());
                }
                "link" => match regex::Regex::new(value) {
                    Ok(re) => cfg.link_patterns.push(re),
                    Err(e) => errors.push(format!("line {}: bad link regex {value:?}: {e}", n + 1)),
                },
                "enquiry-response" => {
                    cfg.enquiry_response = match c_escapes(value) {
                        Ok(s) if s.is_empty() => None,
                        Ok(s) => Some(s),
                        Err(e) => {
                            errors.push(format!("line {}: {e}", n + 1));
                            None
                        }
                    };
                }
                "clipboard-paste-bracketed-safe" => {
                    cfg.paste_bracketed_safe = bool_value(value, n, &mut errors);
                }
                "image-storage-limit" => match value.parse::<usize>() {
                    Ok(v) => cfg.image_storage_limit = v,
                    _ => errors.push(format!("line {}: bad image-storage-limit {value:?}", n + 1)),
                },
                "app-notifications" => match value {
                    "clipboard-copy" => cfg.app_notify_clipboard_copy = true,
                    "no-clipboard-copy" => cfg.app_notify_clipboard_copy = false,
                    "config-reload" => cfg.app_notify_config_reload = true,
                    "no-config-reload" => cfg.app_notify_config_reload = false,
                    _ => errors.push(format!(
                        "line {}: bad app-notifications {value:?} (want clipboard-copy|config-reload, no- prefix disables)",
                        n + 1
                    )),
                },
                "selection-clear-on-copy" => match value {
                    "true" | "yes" => cfg.selection_clear_on_copy = true,
                    "false" | "no" => cfg.selection_clear_on_copy = false,
                    _ => errors.push(format!(
                        "line {}: bad selection-clear-on-copy {value:?}",
                        n + 1
                    )),
                },
                "undo-timeout" => match parse_duration_ms(value) {
                    Some(ms) => cfg.undo_timeout_ms = ms,
                    None => errors.push(format!(
                        "line {}: bad undo-timeout {value:?} (want e.g. 5s)",
                        n + 1
                    )),
                },
                "title-report" => match value {
                    "true" | "yes" => cfg.title_report = true,
                    "false" | "no" => cfg.title_report = false,
                    _ => errors.push(format!("line {}: bad title-report {value:?}", n + 1)),
                },
                "vt-kam-allowed" => match value {
                    "true" | "yes" => cfg.vt_kam_allowed = true,
                    "false" | "no" => cfg.vt_kam_allowed = false,
                    _ => errors.push(format!("line {}: bad vt-kam-allowed {value:?}", n + 1)),
                },
                "search-foreground" => match parse_rgb(value) {
                    Some(c) => cfg.search_foreground = Some(c),
                    None => errors
                        .push(format!("line {}: bad search-foreground {value:?}", n + 1)),
                },
                "search-background" => match parse_rgb(value) {
                    Some(c) => cfg.search_background = Some(c),
                    None => errors
                        .push(format!("line {}: bad search-background {value:?}", n + 1)),
                },
                "search-selected-foreground" => match parse_rgb(value) {
                    Some(c) => cfg.search_selected_foreground = Some(c),
                    None => errors.push(format!(
                        "line {}: bad search-selected-foreground {value:?}",
                        n + 1
                    )),
                },
                "search-selected-background" => match parse_rgb(value) {
                    Some(c) => cfg.search_selected_background = Some(c),
                    None => errors.push(format!(
                        "line {}: bad search-selected-background {value:?}",
                        n + 1
                    )),
                },
                "split-preserve-zoom" => match value {
                    "navigation" => cfg.split_preserve_zoom_navigation = true,
                    "no-navigation" => cfg.split_preserve_zoom_navigation = false,
                    _ => errors.push(format!(
                        "line {}: bad split-preserve-zoom {value:?} (want navigation|no-navigation)",
                        n + 1
                    )),
                },
                "config-default-files" => match value {
                    "true" | "yes" => cfg.config_default_files = true,
                    "false" | "no" => cfg.config_default_files = false,
                    _ => errors.push(format!(
                        "line {}: bad config-default-files {value:?}",
                        n + 1
                    )),
                },
                // Expansion happens in `load` (the directive's file text is
                // spliced in before `parse` runs); the surviving line only
                // records the path for diagnostics.
                "config-file" => {
                    if !value.is_empty() {
                        cfg.config_files.push(expand_home(value));
                    }
                }
                "keybind" if value.trim().eq_ignore_ascii_case("clear") => {
                    // Ghostty `keybind = clear` — wipe every bind parsed so
                    // far AND suppress the built-in chord table; `keybind`
                    // lines after this rebuild from zero.
                    cfg.keybinds.clear();
                    cfg.keybinds_cleared = true;
                }
                "keybind" => match parse_keybind(value) {
                    // Ghostty: triggers ignore prefixes — a later
                    // `keybind` on the same chord replaces the earlier
                    // entry wholesale, flags included.
                    Ok((trig, action)) => {
                        cfg.keybinds.retain(|(t, _)| t.chord != trig.chord);
                        cfg.keybinds.push((trig, action));
                    }
                    Err(e) => errors.push(format!("line {}: {e}", n + 1)),
                },
                _ => errors.push(format!("line {}: unknown key {key:?}", n + 1)),
            }
        }
        (cfg, errors)
    }

    /// Load a config file; a missing file yields defaults and a template
    /// is written so the user can discover the format.
    /// Every default config location in load order (later wins):
    /// each `$XDG_CONFIG_DIRS` entry's `hydroterm/config`, then
    /// `$XDG_CONFIG_HOME/hydroterm/config` (highest precedence).
    /// `XDG_CONFIG_DIRS` unset falls back to `/etc/xdg` (freedesktop).
    pub fn default_paths() -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        match std::env::var_os("XDG_CONFIG_DIRS") {
            Some(dirs) => {
                for d in std::env::split_paths(&dirs) {
                    out.push(d.join("hydroterm/config"));
                }
            }
            None => out.push(PathBuf::from("/etc/xdg/hydroterm/config")),
        }
        out.push(default_path());
        out
    }

    /// Load the default locations in precedence order — the reference
    /// reads every default file, lowest precedence first. A file that
    /// sets `config-default-files = false` ends the chain early (the
    /// flag's file itself still applies). Missing files are skipped,
    /// except the XDG_CONFIG_HOME path, which gets the template like
    /// `load` does.
    pub fn load_defaults() -> (Self, Vec<String>) {
        let mut texts = String::new();
        let mut errors: Vec<String> = Vec::new();
        let home = default_path();
        let paths = Self::default_paths();
        let mut applied: Vec<PathBuf> = Vec::new();
        for path in &paths {
            let text = match std::fs::read_to_string(path) {
                Ok(t) => Some(t),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if path == &home {
                        // Last path = the home file: seed the template
                        // and take its contents so reload tests see it.
                        if let Some(dir) = path.parent() {
                            let _ = std::fs::create_dir_all(dir);
                        }
                        let _ = std::fs::write(path, TEMPLATE);
                        Some(TEMPLATE.to_string())
                    } else {
                        None
                    }
                }
                Err(e) => {
                    errors.push(format!("reading {}: {e}", path.display()));
                    None
                }
            };
            let Some(text) = text else { continue };
            applied.push(path.clone());
            // Includes splice per file so a `config-file =` path resolves
            // against its own directory, not the chain's.
            let mut visited = std::collections::HashSet::new();
            if let Ok(canon) = path.canonicalize() {
                visited.insert(canon);
            }
            let mut warnings = Vec::new();
            let expanded = expand_includes(&text, path, &mut visited, &mut warnings);
            errors.extend(warnings);
            texts.push_str(&expanded);
            // `config-default-files = false` ends the chain after this
            // file's own lines have applied.
            if !Self::parse(&expanded).0.config_default_files {
                break;
            }
        }
        let (mut cfg, errs) = Self::parse(&texts);
        errors.extend(errs);
        // `config_files` records the default locations actually applied
        // (the chain stops early on `config-default-files = false`).
        applied.extend(std::mem::take(&mut cfg.config_files));
        cfg.config_files = applied;
        (cfg, errors)
    }

    pub fn load(path: &Path) -> (Self, Vec<String>) {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let mut visited = std::collections::HashSet::new();
                if let Ok(canon) = path.canonicalize() {
                    visited.insert(canon);
                }
                let mut warnings = Vec::new();
                let expanded = expand_includes(&text, path, &mut visited, &mut warnings);
                let (cfg, mut errors) = Self::parse(&expanded);
                errors.splice(0..0, warnings);
                (cfg, errors)
            }
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

/// `config-file = <path>` include expansion: each directive's file text is
/// spliced in after the line (the directive itself stays so `parse` records
/// it in `config_files`). Paths are relative to the including file's
/// directory; `~` expands. Cycles and unreadable files warn, not fail —
/// matching Ghostty's include semantics.
fn expand_includes(
    text: &str,
    containing: &Path,
    visited: &mut std::collections::HashSet<PathBuf>,
    warnings: &mut Vec<String>,
) -> String {
    let dir = containing.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        out.push_str(line);
        out.push('\n');
        let t = line.trim();
        let Some((key, value)) = t.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("config-file") {
            let value = value.trim().trim_matches('"');
            let raw = expand_home(value);
            let path = if raw.is_absolute() {
                raw
            } else {
                dir.join(&raw)
            };
            let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
            if !visited.insert(canon) {
                warnings.push(format!("config-file cycle: {}", path.display()));
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(body) => {
                    out.push_str(&expand_includes(&body, &path, visited, warnings));
                }
                Err(e) => {
                    warnings.push(format!("config-file {}: {e}", path.display()));
                }
            }
        }
    }
    out
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

/// `theme = light:<name>,dark:<name>` — Ghostty's per-mode theme pair.
/// Either order; both keys required; each name must resolve to the
/// catalog. Returns `Some((light, dark))` only on a full pair.
fn parse_theme_pair(value: &str) -> Option<(String, String)> {
    let mut light = None;
    let mut dark = None;
    for field in value.split(',') {
        let field = field.trim();
        if let Some(v) = field.strip_prefix("light:") {
            light = Some(v.trim().to_string());
        } else if let Some(v) = field.strip_prefix("dark:") {
            dark = Some(v.trim().to_string());
        }
    }
    let (light, dark) = (light?, dark?);
    if Theme::by_name(&light).is_none() || Theme::by_name(&dark).is_none() {
        return None;
    }
    Some((light, dark))
}

/// `quick-terminal-size = <a>[,<b>]` — each extent is `N%` or `Npx`;
/// a bare number is a config error (Ghostty).
fn parse_quick_term_size(
    value: &str,
) -> Result<Option<(QuickTermSize, Option<QuickTermSize>)>, String> {
    fn one(v: &str) -> Result<QuickTermSize, String> {
        let v = v.trim();
        if let Some(p) = v.strip_suffix('%') {
            return p
                .trim()
                .parse::<f64>()
                .map(QuickTermSize::Percent)
                .map_err(|_| format!("bad quick-terminal-size {v:?}"));
        }
        if let Some(p) = v.strip_suffix("px") {
            return p
                .trim()
                .parse::<f64>()
                .map(QuickTermSize::Px)
                .map_err(|_| format!("bad quick-terminal-size {v:?}"));
        }
        Err(format!(
            "quick-terminal-size {v:?} needs a % or px suffix"
        ))
    }
    let mut parts = value.splitn(2, ',');
    let a = one(parts.next().unwrap_or_default())?;
    let b = match parts.next() {
        Some(v) => Some(one(v)?),
        None => None,
    };
    Ok(Some((a, b)))
}

/// `notify-on-command-finish-after` duration — `5s`, `500ms`, `1m`,
/// `1h`, or a bare number of seconds (Ghostty accepts its duration
/// spellings; we cover the same ground).
fn parse_notify_after(value: &str) -> Option<f64> {
    let v = value.trim();
    for (suffix, scale) in [("ms", 0.001), ("s", 1.0), ("m", 60.0), ("h", 3600.0)] {
        if let Some(n) = v.strip_suffix(suffix) {
            return n.trim().parse::<f64>().ok().filter(|x| *x >= 0.0).map(|x| x * scale);
        }
    }
    v.parse::<f64>().ok().filter(|x| *x >= 0.0)
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
/// Parse `U+AAAA[-U+BBBB]=Family Name` (Ghostty `font-codepoint-map`).
fn parse_codepoint_map(value: &str) -> Option<(u32, u32, String)> {
    let (range, family) = value.split_once('=')?;
    let family = family.trim();
    if family.is_empty() {
        return None;
    }
    let hex = |s: &str| -> Option<u32> {
        u32::from_str_radix(s.trim().strip_prefix("U+")?, 16).ok()
    };
    let mut parts = range.splitn(2, '-');
    let lo = hex(parts.next()?)?;
    let hi = match parts.next() {
        Some(h) => hex(h)?,
        None => lo,
    };
    (hi >= lo).then(|| (lo, hi, family.to_string()))
}

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
    "ignore", "copy_to_clipboard", "copy_url_to_clipboard", "copy_title_to_clipboard",
    "new_tab", "close_tab", "close_surface", "new_window", "next_tab", "previous_tab",
    "goto_tab:<n>", "move_tab:<±n>",
    "increase_font_size[:pt]", "decrease_font_size[:pt]", "set_font_size:<pt>",
    "reset_font_size",
    "clear_scrollback", "clear_screen", "reset",
    "start_search", "end_search", "search_selection", "search:<text>",
    "navigate_search:<next|previous>", "search", "jump_to_prompt:<±n>",
    "select_all", "start_selection",
    "last_tab", "close_window", "close_all_tabs", "close_other_tabs", "toggle_tab_bar",
    "scroll_to_top", "scroll_to_bottom", "scroll_page_up", "scroll_page_down",
    "scroll_page_lines:<±n>", "scroll_page_fractional:<±f>",
    "url_hints", "copy_last_output", "open_scrollback_editor", "reload_config",
    "write_screen_file[:open|copy|paste]", "write_scrollback_file[:open|copy|paste]",
    "write_selection_file[:open|copy|paste]", "write_last_output_file[:open|copy|paste]",
    "open_config", "scroll_to_selection", "clear_selection",
    "text:\"…\"", "csi:\"…\"", "esc:\"…\"",
    "scroll_to_fraction:<0-1>", "scroll_to_row:<n>",
    "paste_from_clipboard", "paste_from_selection",
    "prompt_surface_title", "prompt_tab_title",
    "set_surface_title:<text>", "set_tab_title:<text>",
    "inspector[:toggle|show|hide]",
    "toggle_mouse_visibility",
    "adjust_selection:<left|right|up|down|home|end|page_up|page_down|escape>",
    "quit", "toggle_fullscreen", "toggle_command_palette", "settings",
    "new_split:<right|down|left|up|auto>",
    "goto_split:<left|right|up|down|previous|next|top|bottom>",
    "resize_split:<left|right|up|down>[,px]",
    "goto_split:<previous|next>  (pane focus cycle)",
    "toggle_split_zoom", "equalize_splits",
    "sequence:<a,b,…>  (run every action on one chord)",
    "undo", "redo", "toggle_mark", "jump_to_mark:<previous|next>",
    "cursor_key:<up|down|left|right|home|end|page_up|page_down>",
    "hide_all_windows",
    "none | unbind  (disable a chord; unbound keys reach the pty)",
];

/// Ghostty `command`/`initial-command` value → argv: `direct:` splits
/// verbatim argv, `shell:` or a bare value goes through `sh -c`
/// (the reference's `Command` union — bare values are shell-expanded).
fn parse_command(value: &str) -> Option<Vec<String>> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    if let Some(rest) = v.strip_prefix("direct:") {
        let argv: Vec<String> = rest.split_whitespace().map(String::from).collect();
        return (!argv.is_empty()).then_some(argv);
    }
    let script = v.strip_prefix("shell:").map(str::trim).unwrap_or(v);
    Some(vec!["/bin/sh".into(), "-c".into(), script.to_string()])
}

/// `1h30m`-style compound duration → milliseconds (Ghostty `Duration`
/// — every number+unit pair adds in; sub-ms units round to 0).
fn parse_duration_ms(value: &str) -> Option<u64> {
    let mut rest = value.trim();
    // Bare `0` reads naturally for "disabled" (`undo-timeout = 0`).
    if rest == "0" {
        return Some(0);
    }
    let mut total: u64 = 0;
    let mut seen = false;
    while !rest.is_empty() {
        let dlen = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        if dlen == 0 {
            return None;
        }
        let n: u64 = rest[..dlen].parse().ok()?;
        let after_num = &rest[dlen..];
        let ulen = after_num
            .find(|c: char| c.is_ascii_digit())
            .unwrap_or(after_num.len());
        let ms_per = match after_num[..ulen].trim() {
            "y" => 365 * 86_400_000u64,
            "w" => 7 * 86_400_000u64,
            "d" => 86_400_000u64,
            "h" => 3_600_000,
            "m" => 60_000,
            "s" => 1_000,
            "ms" => 1,
            "us" | "µs" | "ns" => 0,
            _ => return None,
        };
        total = total.saturating_add(n.saturating_mul(ms_per));
        seen = true;
        rest = after_num[ulen..].trim_start();
    }
    seen.then_some(total)
}

fn bool_value(value: &str, line: usize, errors: &mut Vec<String>) -> bool {
    match value.to_ascii_lowercase().as_str() {
        "true" | "on" => true,
        "false" | "off" => false,
        _ => {
            errors.push(format!("line {}: bad boolean {value:?}", line + 1));
            false
        }
    }
}

/// `ctrl+shift+c=copy` → (trigger, Some(Copy)); an empty action or
/// `none`/`unbind` disables the chord.
fn parse_keybind(value: &str) -> Result<(KeybindTrigger, Option<TermAction>), String> {
    let (chord, action) = value
        .split_once('=')
        .ok_or_else(|| format!("keybind needs `<chord>=<action>`: {value:?}"))?;
    let raw = chord.trim();
    let lower = raw.to_ascii_lowercase();
    // Ghostty trigger prefixes — any order, each at most once:
    // `global:` (X11 root grab), `all:` (every surface), `unconsumed:`
    // (fire the action and still send the encoded key), `performable:`
    // (fire only while performable). e.g. `global:unconsumed:ctrl+a`.
    let (mut global, mut all, mut unconsumed, mut performable, mut physical) =
        (false, false, false, false, false);
    let mut rest = lower.as_str();
    while let Some((prefix, tail)) = rest.split_once(':') {
        let flag = match prefix {
            "global" => &mut global,
            "all" => &mut all,
            "unconsumed" => &mut unconsumed,
            "performable" => &mut performable,
            "physical" => &mut physical,
            _ => break,
        };
        if *flag {
            return Err(format!("keybind {raw:?}: duplicate `{prefix}:` prefix"));
        }
        *flag = true;
        rest = tail;
    }
    // `physical:` keeps the modifier prefix for compare and resolves the
    // key name to its physical `Code` (US-layout position) — `physical:e`
    // fires on the E-position key whatever the layout maps there.
    let (chord, physical_code) = if physical {
        let (mods, name, code) = parse_physical_chord(rest)?;
        (format!("{mods}physical:{name}"), Some(code))
    } else {
        (normalize_chord(rest)?, None)
    };
    let chord = KeybindTrigger {
        chord,
        global,
        all,
        unconsumed,
        performable,
        physical_code,
    };
    let action_raw = action.trim();
    let lower = action_raw.to_ascii_lowercase();
    let action = match lower.as_str() {
        "" | "none" => None,
        _ => Some(
            action_from_str(&lower, action_raw)
                .ok_or_else(|| format!("unknown action {action_raw:?}"))?,
        ),
    };
    Ok((chord, action))
}

/// A `command-palette-entry` config row (Ghostty): the palette title,
/// the description shown in the chord slot, and the action string,
/// resolved through the same action table keybinds use.
#[derive(Clone, Debug, PartialEq)]
pub struct PaletteEntryCfg {
    pub title: String,
    pub description: String,
    pub action: String,
}

/// Split an entry value on top-level commas — a double-quoted span may
/// hold a comma (`action:text:"a,b"`) and backslash escapes.
fn split_entry_fields(s: &str) -> Vec<&str> {
    let mut fields = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut esc = false;
    for (i, c) in s.char_indices() {
        if esc {
            esc = false;
            continue;
        }
        match c {
            '\\' if quoted => esc = true,
            '"' => quoted = !quoted,
            ',' if !quoted => {
                fields.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    fields.push(&s[start..]);
    fields
}

/// Parse `title:T, description:D, action:A` — Ghostty's
/// `command-palette-entry` fields; `title` and `action` are required.
/// Each value passes through `parse_payload`, so a quoted value may
/// carry escapes and commas.
fn parse_palette_entry(value: &str) -> Result<PaletteEntryCfg, String> {
    let mut title = None;
    let mut description = String::new();
    let mut action = None;
    for field in split_entry_fields(value) {
        let Some((k, v)) = field.split_once(':') else {
            return Err(format!(
                "bad command-palette-entry field {field:?} (want name:value)"
            ));
        };
        let v = parse_payload(v.trim())?;
        match k.trim() {
            "title" => title = Some(v),
            "description" => description = v,
            "action" => action = Some(v),
            other => {
                return Err(format!("unknown command-palette-entry field {other:?}"));
            }
        }
    }
    Ok(PaletteEntryCfg {
        title: title.ok_or("command-palette-entry needs title:")?,
        description,
        action: action.ok_or("command-palette-entry needs action:")?,
    })
}

/// Ghostty payload escapes inside `"…"`: `\n \r \t \e \\ \" \xNN`.
/// An unquoted payload is taken verbatim.
fn parse_payload(raw: &str) -> Result<String, String> {
    let s = raw.trim();
    let Some(inner) = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        // Unquoted payloads return the value verbatim — a `\n` an escape
        // decoded to is meaningful and must not be trimmed away.
        return Ok(raw.to_string());
    };
    c_escapes(inner)
}

/// Decode C escapes (`\n \r \t \e \\ \" \xNN`) in `inner`.
fn c_escapes(inner: &str) -> Result<String, String> {
    let mut out = String::with_capacity(inner.len());
    let mut it = inner.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('e') => out.push('\x1b'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('x') => {
                let hi = it.next().and_then(|c| c.to_digit(16));
                let lo = it.next().and_then(|c| c.to_digit(16));
                match (hi, lo) {
                    (Some(hi), Some(lo)) => out.push(char::from_u32(hi * 16 + lo).unwrap_or('\u{fffd}')),
                    _ => return Err("bad \\xNN escape".into()),
                }
            }
            other => {
                out.push('\\');
                if let Some(o) = other {
                    out.push(o);
                }
            }
        }
    }
    Ok(out)
}

/// Split a `sequence:` action list on top-level commas — a comma inside
/// a `"…"` payload (e.g. `text:"a,b"`) does not split, `\` escapes a
/// literal quote.
fn split_sequence(inner: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (i, c) in inner.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ',' if !quoted => {
                parts.push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&inner[start..]);
    parts.into_iter().map(str::trim).filter(|p| !p.is_empty()).collect()
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
            "ctrl" => mods[0] = true,
            "alt" => mods[1] = true,
            "shift" => mods[2] = true,
            "super" => mods[3] = true,
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

/// The modifier part of a normalized chord — everything before the
/// last `+` (`"ctrl+shift+e"` → `"ctrl+shift"`, `"physical:e"` → `""`).
fn chord_mods(chord: &str) -> &str {
    chord.rsplit_once('+').map(|(m, _)| m).unwrap_or("")
}

/// Canonical `ctrl+alt+shift+super` prefix for a live modifier state —
/// same segment order `chord_of` emits (no trailing `+`, empty when
/// none are held).
fn mods_prefix(mods: Modifiers) -> &'static str {
    match (
        mods.contains(Modifiers::CONTROL),
        mods.contains(Modifiers::ALT),
        mods.contains(Modifiers::SHIFT),
        mods.contains(Modifiers::META),
    ) {
        (false, false, false, false) => "",
        (true, false, false, false) => "ctrl",
        (false, true, false, false) => "alt",
        (false, false, true, false) => "shift",
        (false, false, false, true) => "super",
        (true, true, false, false) => "ctrl+alt",
        (true, false, true, false) => "ctrl+shift",
        (true, false, false, true) => "ctrl+super",
        (false, true, true, false) => "alt+shift",
        (false, true, false, true) => "alt+super",
        (false, false, true, true) => "shift+super",
        (true, true, true, false) => "ctrl+alt+shift",
        (true, true, false, true) => "ctrl+alt+super",
        (true, false, true, true) => "ctrl+shift+super",
        (false, true, true, true) => "alt+shift+super",
        (true, true, true, true) => "ctrl+alt+shift+super",
    }
}

/// Parse a `physical:` chord tail — `<mods>+<keyname>` →
/// (mods prefix, key name, winit `Code` for its US-layout position).
fn parse_physical_chord(chord: &str) -> Result<(String, String, Code), String> {
    let mut mods = [false; 4]; // ctrl, alt, shift, super
    let mut out = (String::new(), None, None);
    let mut parts = chord.split('+').peekable();
    while let Some(part) = parts.next() {
        let p = part.trim().to_ascii_lowercase();
        match p.as_str() {
            "ctrl" => mods[0] = true,
            "alt" => mods[1] = true,
            "shift" => mods[2] = true,
            "super" => mods[3] = true,
            _ => {
                if !parts.peek().is_none() || out.1.is_some() {
                    return Err(format!("bad keybind chord {chord:?}"));
                }
                let name = canonical_key_name(&p)?;
                let code = physical_code_of(&name)
                    .ok_or_else(|| format!("no physical code for key {p:?}"))?;
                out.1 = Some(name);
                out.2 = Some(code);
            }
        }
    }
    if mods[0] {
        out.0.push_str("ctrl+");
    }
    if mods[1] {
        out.0.push_str("alt+");
    }
    if mods[2] {
        out.0.push_str("shift+");
    }
    if mods[3] {
        out.0.push_str("super+");
    }
    match (out.1, out.2) {
        (Some(name), Some(code)) => Ok((out.0, name, code)),
        _ => Err(format!("keybind chord has no key: {chord:?}")),
    }
}

/// Ghostty `physical:` key name → winit/keyboard-types `Code` (the
/// US-QWERTY position the name refers to).
fn physical_code_of(name: &str) -> Option<Code> {
    let n = name.to_ascii_lowercase();
    if n.len() == 1 {
        let c = n.chars().next()?;
        return match c {
            'a'..='z' => format!("Key{}", c.to_ascii_uppercase()).parse().ok(),
            '0'..='9' => format!("Digit{c}").parse().ok(),
            ',' => Some(Code::Comma),
            '.' => Some(Code::Period),
            '/' => Some(Code::Slash),
            '-' => Some(Code::Minus),
            '=' => Some(Code::Equal),
            '[' => Some(Code::BracketLeft),
            ']' => Some(Code::BracketRight),
            ';' => Some(Code::Semicolon),
            '\'' => Some(Code::Quote),
            '`' => Some(Code::Backquote),
            '\\' => Some(Code::Backslash),
            _ => None,
        };
    }
    if let Some(digits) = n.strip_prefix('f')
        && digits.parse::<u8>().is_ok_and(|d| (1..=24).contains(&d))
    {
        return format!("F{digits}").parse().ok();
    }
    let code = match n.as_str() {
        "arrowup" => "ArrowUp",
        "arrowdown" => "ArrowDown",
        "arrowleft" => "ArrowLeft",
        "arrowright" => "ArrowRight",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        "home" => "Home",
        "end" => "End",
        "insert" => "Insert",
        "delete" => "Delete",
        "backspace" => "Backspace",
        "tab" => "Tab",
        "enter" => "Enter",
        "escape" => "Escape",
        "space" => "Space",
        _ => return None,
    };
    code.parse().ok()
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

/// Action names for `keybind =` right-hand sides — also resolves
/// `command-palette-entry` `action:` payloads (same table).
pub fn action_from_str(name: &str, raw: &str) -> Option<TermAction> {
    Some(match name {
        // Ghostty payload actions — `keybind = chord=text:"hi\n"` types
        // the literal text; `esc:`/`csi:` prepend `\e`/`\e[`. Payload
        // keeps the config line's case, so these read `raw` not `name`.
        _ if name.starts_with("text:") => {
            TermAction::TypeText(parse_payload(&raw[5..]).ok()?)
        }
        _ if name.starts_with("csi:") => TermAction::CsiSeq(parse_payload(&raw[4..]).ok()?),
        _ if name.starts_with("esc:") => TermAction::EscSeq(parse_payload(&raw[4..]).ok()?),
        // Ghostty `sequence:a,b` — every action runs in order on one
        // chord. Commas inside a `text:"…,…"` payload don't split.
        _ if name.starts_with("sequence:") => {
            let inner = &raw["sequence:".len()..];
            let parts = split_sequence(inner);
            if parts.is_empty() {
                return None;
            }
            let actions: Option<Vec<TermAction>> =
                parts.iter().map(|p| action_from_str(&p.to_lowercase(), p)).collect();
            TermAction::Sequence(actions?)
        }
        _ if name.starts_with("scroll_to_fraction:") => {
            let f: f64 = name["scroll_to_fraction:".len()..].parse().ok()?;
            TermAction::ScrollToFraction(f.clamp(0.0, 1.0))
        }
        _ if name.starts_with("scroll_to_row:") => {
            let n: usize = name["scroll_to_row:".len()..].parse().ok()?;
            TermAction::ScrollToRow(n)
        }
        "ignore" => TermAction::Ignore,
        "copy_to_clipboard" => TermAction::Copy,
        "copy_url_to_clipboard" => TermAction::CopyUrlToClipboard,
        "copy_title_to_clipboard" => TermAction::CopyTitleToClipboard,
        "paste_from_clipboard" => TermAction::Paste,
        "paste_from_selection" => TermAction::PasteFromSelection,
        "prompt_surface_title" => TermAction::PromptTitle,
        "prompt_tab_title" => TermAction::PromptTabTitle,
        // Ghostty `set_surface_title:text` / `set_tab_title:text` — the
        // raw payload keeps its case and spaces.
        _ if name.starts_with("set_surface_title:") => {
            TermAction::SetSurfaceTitle(raw["set_surface_title:".len()..].to_string())
        }
        _ if name.starts_with("set_tab_title:") => {
            TermAction::SetTabTitle(raw["set_tab_title:".len()..].to_string())
        }
        "undo" => TermAction::Undo,
        "redo" => TermAction::Redo,
        "toggle_mark" => TermAction::ToggleMark,
        // Ghostty `jump_to_mark:previous|next`.
        _ if name.starts_with("jump_to_mark:") => {
            match &name["jump_to_mark:".len()..] {
                "previous" | "prev" => TermAction::JumpToMark(-1),
                "next" => TermAction::JumpToMark(1),
                _ => return None,
            }
        }
        // Ghostty `cursor_key:<up|down|left|right|home|end|page_up|
        // page_down>` — emits the escape sequence a physical cursor
        // keypress would send, honoring DECCKM application mode.
        _ if name.starts_with("cursor_key:") => {
            use crate::keys::CursorKeyDir as D;
            TermAction::CursorKey(match &name["cursor_key:".len()..] {
                "up" => D::Up,
                "down" => D::Down,
                "right" => D::Right,
                "left" => D::Left,
                "home" => D::Home,
                "end" => D::End,
                "page_up" => D::PageUp,
                "page_down" => D::PageDown,
                _ => return None,
            })
        }
        // Ghostty `hide_all_windows` — minimize every window.
        "hide_all_windows" => TermAction::HideAllWindows,
        "inspector" => TermAction::Inspector,
        "inspector:toggle" => TermAction::Inspector,
        "inspector:show" => TermAction::InspectorSet(true),
        "inspector:hide" => TermAction::InspectorSet(false),
        "new_tab" => TermAction::NewTab,
        "close_tab" => TermAction::CloseTab,
        "close_surface" => TermAction::CloseSurface,
        "new_window" => TermAction::NewWindow,
        "toggle_quick_terminal" => TermAction::ToggleQuickTerminal,
        "last_tab" => TermAction::LastTab,
        "close_window" => TermAction::CloseWindow,
        "close_all_tabs" => TermAction::CloseAllTabs,
        "close_other_tabs" => TermAction::CloseOtherTabs,
        "toggle_tab_bar" => TermAction::ToggleTabBar,
        "next_tab" => TermAction::NextTab,
        "previous_tab" => TermAction::PrevTab,
        "reset_font_size" => TermAction::FontReset,
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
        // Ghostty `set_font_size:pt` — absolute point size.
        _ if name.starts_with("set_font_size:") => {
            let pt: f32 = name["set_font_size:".len()..].parse().ok()?;
            TermAction::SetFontSize(pt.clamp(6.0, 96.0))
        }
        "clear_scrollback" => TermAction::ClearScrollback,
        "clear_screen" => TermAction::ClearScreen,
        "reset" => TermAction::Reset,
        "search" => TermAction::Search,
        "start_search" => TermAction::StartSearch,
        "end_search" => TermAction::EndSearch,
        "search_selection" => TermAction::SearchSelection,
        // Ghostty `search:text` — sets the search bar's query.
        _ if name.starts_with("search:") => {
            TermAction::SearchFor(raw["search:".len()..].to_string())
        }
        "navigate_search:next" => TermAction::NavigateSearch(1),
        "navigate_search:previous" => TermAction::NavigateSearch(-1),
        "toggle_mouse_visibility" => TermAction::ToggleMouseVisibility,
        // Ghostty `adjust_selection:left|right|up|down|home|end|
        // page_up|page_down|escape` — keyboard selection moves.
        _ if name.starts_with("adjust_selection:") => {
            let d = &name["adjust_selection:".len()..];
            TermAction::AdjustSelection(match d {
                "left" => AdjustSel::Left,
                "right" => AdjustSel::Right,
                "up" => AdjustSel::Up,
                "down" => AdjustSel::Down,
                "home" => AdjustSel::Home,
                "end" => AdjustSel::End,
                "page_up" => AdjustSel::PageUp,
                "page_down" => AdjustSel::PageDown,
                "escape" => AdjustSel::Escape,
                _ => return None,
            })
        }
        // Ghostty `jump_to_prompt:N` — scroll N prompt marks (signed).
        _ if name.starts_with("jump_to_prompt:") => {
            let n: i32 = name["jump_to_prompt:".len()..].parse().ok()?;
            TermAction::JumpToPrompt(n)
        }
        "select_all" => TermAction::SelectAll,
        "start_selection" => TermAction::StartSelection,
        "scroll_to_top" => TermAction::ScrollToTop,
        "scroll_to_bottom" => TermAction::ScrollToBottom,
        "scroll_page_up" => TermAction::ScrollPageUp,
        "scroll_page_down" => TermAction::ScrollPageDown,
        // Ghostty `scroll_page_lines:N` — signed line count.
        _ if name.starts_with("scroll_page_lines:") => {
            let n: i32 = name["scroll_page_lines:".len()..].parse().ok()?;
            TermAction::ScrollPageLines(n)
        }
        // Ghostty `scroll_page_fractional:f` — a fraction of the page,
        // -1.0..=1.0.
        _ if name.starts_with("scroll_page_fractional:") => {
            let f: f64 = name["scroll_page_fractional:".len()..].parse().ok()?;
            TermAction::ScrollPageFractional(f.clamp(-1.0, 1.0))
        }
        // Ghostty `move_tab:N` — signed slot move.
        _ if name.starts_with("move_tab:") => {
            let n: i32 = name["move_tab:".len()..].parse().ok()?;
            TermAction::MoveTab(n)
        }
        "url_hints" => TermAction::UrlHints,
        "copy_last_output" => TermAction::CopyLastOutput,
        "open_scrollback_editor" => TermAction::OpenScrollbackEditor,
        "reload_config" => TermAction::ReloadConfig,
        "toggle_split_zoom" => TermAction::PaneZoom,
        "quit" => TermAction::Quit,
        "equalize_splits" => TermAction::EqualizeSplits,
        "toggle_fullscreen" => TermAction::Fullscreen,
        "toggle_command_palette" => TermAction::Palette,
        "settings" => TermAction::Settings,
        // Ghostty `write_*_file[:open|copy|paste]` — the suffix names
        // what to do with the written temp file.
        _ if name.starts_with("write_screen_file") => {
            TermAction::WriteScreenFile(file_sink(&name["write_screen_file".len()..])?)
        }
        _ if name.starts_with("write_scrollback_file") => {
            TermAction::WriteScrollbackFile(file_sink(&name["write_scrollback_file".len()..])?)
        }
        _ if name.starts_with("write_selection_file") => {
            TermAction::WriteSelectionFile(file_sink(&name["write_selection_file".len()..])?)
        }
        _ if name.starts_with("write_last_output_file") => {
            TermAction::WriteLastOutputFile(file_sink(&name["write_last_output_file".len()..])?)
        }
        "open_config" => TermAction::OpenConfig,
        "scroll_to_selection" => TermAction::ScrollToSelection,
        "clear_selection" => TermAction::ClearSelection,
        // Ghostty `goto_tab:N` — select the Nth tab (1-based).
        _ if name.starts_with("goto_tab:") => {
            let n: usize = name["goto_tab:".len()..].parse().ok()?;
            TermAction::SelectTab(n)
        }
        // Ghostty `keybind = ...=new_split:right` — direction arg selects
        // which side the new pane lands on (`auto` picks by pane aspect).
        _ if name.strip_prefix("new_split:").is_some() => {
            match &name["new_split:".len()..] {
                "right" => TermAction::SplitRight,
                "auto" => TermAction::SplitAuto,
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
                "previous" => TermAction::FocusPrevPane,
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

/// Ghostty's `:action` suffix on `write_*_file` — `open` is the bare
/// default; `copy`/`paste` route the file path instead.
fn file_sink(suffix: &str) -> Option<FileSink> {
    match suffix {
        "" | ":open" => Some(FileSink::Open),
        ":copy" => Some(FileSink::Copy),
        ":paste" => Some(FileSink::Paste),
        _ => None,
    }
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
            "ctrl" => Ok(Self::Ctrl),
            "shift" => Ok(Self::Shift),
            "alt" => Ok(Self::Alt),
            "super" => Ok(Self::Super),
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
    /// Load the file at `path`, or the whole default chain
    /// (`default_paths`, honouring `config-default-files`). The watched
    /// path stays the XDG_CONFIG_HOME file for hot-reload either way.
    pub fn new(path: Option<PathBuf>) -> Self {
        let path = path.unwrap_or_else(default_path);
        let (config, errors) = if path == default_path() {
            AppConfig::load_defaults()
        } else {
            AppConfig::load(&path)
        };
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
        let (config, errors) = if self.path == default_path() {
            AppConfig::load_defaults()
        } else {
            AppConfig::load(&self.path)
        };
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
        let (config, errors) = if self.path == default_path() {
            AppConfig::load_defaults()
        } else {
            AppConfig::load(&self.path)
        };
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
                    copy-on-select = true\ncursor-style = beam\ncursor-style-blink = false\n\
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
            "keybind = ctrl+shift+c=copy_to_clipboard\nkeybind = shift+ctrl+v=paste_from_clipboard\n\
             keybind = ctrl+shift+f4=close_tab\nkeybind = ctrl+shift+x=\n",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.keybinds.len(), 4);
        // Modifier order normalized — shift+ctrl+v and ctrl+shift+v collide.
        assert_eq!(cfg.keybinds[0].0.chord, "ctrl+shift+c");
        assert_eq!(cfg.keybinds[1].0.chord, "ctrl+shift+v");
        assert_eq!(cfg.keybinds[2].0.chord, "ctrl+shift+f4");
        assert_eq!(cfg.keybinds[2].1, Some(TermAction::CloseTab));
        assert_eq!(cfg.keybinds[3].1, None); // disabled
    }

    #[test]
    fn keybind_lookup_matches_and_disables() {
        let (cfg, errs) = AppConfig::parse("keybind = ctrl+shift+c=\nkeybind = alt+f4=quit");
        assert!(errs.is_empty());
        let ctrl_shift = Modifiers::CONTROL | Modifiers::SHIFT;
        let hit = cfg
            .lookup_keybind(&Key::Character("c".into()), Code::Unidentified, ctrl_shift)
            .unwrap();
        assert!(!hit.0.all && !hit.0.performable && !hit.0.unconsumed && hit.1.is_none());
        assert_eq!(
            cfg.lookup_keybind(&Key::Named(NamedKey::F4), Code::Unidentified, Modifiers::ALT)
                .unwrap()
                .1,
            Some(TermAction::Quit)
        );
        assert_eq!(
            cfg.lookup_keybind(&Key::Character("v".into()), Code::Unidentified, ctrl_shift),
            None
        );
    }

    #[test]
    fn keybind_all_scope_marks_the_hit() {
        let (cfg, errs) =
            AppConfig::parse("keybind = all:ctrl+alt+g=increase_font_size:10\nkeybind = ctrl+alt+g=quit");
        assert!(errs.is_empty(), "{errs:?}");
        let ctrl_alt = Modifiers::CONTROL | Modifiers::ALT;
        // Ghostty: triggers ignore prefixes — the later `ctrl+alt+g`
        // replaces the `all:` entry wholesale, flags included.
        let hit = cfg
            .lookup_keybind(&Key::Character("g".into()), Code::Unidentified, ctrl_alt)
            .unwrap();
        assert!(!hit.0.all && hit.1 == Some(TermAction::Quit));
        assert_eq!(cfg.keybinds.len(), 1);
        let (cfg2, errs2) =
            AppConfig::parse("keybind = all:ctrl+alt+g=increase_font_size:10");
        assert!(errs2.is_empty(), "{errs2:?}");
        let hit = cfg2
            .lookup_keybind(&Key::Character("g".into()), Code::Unidentified, ctrl_alt)
            .unwrap();
        assert!(hit.0.all && hit.1 == Some(TermAction::IncreaseFontSize(10)));
    }

    #[test]
    fn performable_prefix_parses_and_marks_the_hit() {
        // `keybind = performable:chord=action` — Ghostty's modifier that
        // fires the bind only while the action is performable.
        let (cfg, errs) = AppConfig::parse(
            "keybind = performable:ctrl+c=copy_to_clipboard\n\
             keybind = performable:all:ctrl+alt+h=clear_scrollback",
        );
        assert!(errs.is_empty(), "{errs:?}");
        let ctrl = Modifiers::CONTROL;
        let hit = cfg
            .lookup_keybind(&Key::Character("c".into()), Code::Unidentified, ctrl)
            .unwrap();
        assert!(hit.0.performable && hit.1 == Some(TermAction::Copy));
        let ctrl_alt = Modifiers::CONTROL | Modifiers::ALT;
        let hit = cfg
            .lookup_keybind(&Key::Character("h".into()), Code::Unidentified, ctrl_alt)
            .unwrap();
        assert!(
            hit.0.all && hit.0.performable && hit.1 == Some(TermAction::ClearScrollback)
        );
        // A plain bind on the same chord still wins by list order.
        let (cfg2, errs2) = AppConfig::parse(
            "keybind = performable:ctrl+c=copy_to_clipboard\nkeybind = ctrl+c=quit",
        );
        assert!(errs2.is_empty(), "{errs2:?}");
        let hit = cfg2
            .lookup_keybind(&Key::Character("c".into()), Code::Unidentified, ctrl)
            .unwrap();
        assert!(
            !hit.0.performable && !hit.0.all && hit.1 == Some(TermAction::Quit)
        );
    }

    #[test]
    fn unconsumed_prefix_parses_and_dedupes() {
        // `keybind = unconsumed:chord=action` (Ghostty) — the bind fires
        // and the press still encodes to the program. Prefixes combine
        // in any order; a duplicate prefix is an error.
        let (cfg, errs) = AppConfig::parse(
            "keybind = unconsumed:ctrl+a=reload_config\n\
             keybind = global:unconsumed:ctrl+b=quit\n\
             keybind = unconsumed:global:ctrl+c=paste_from_clipboard",
        );
        assert!(errs.is_empty(), "{errs:?}");
        let ctrl = Modifiers::CONTROL;
        let (t, a) = cfg
            .lookup_keybind(&Key::Character("a".into()), Code::Unidentified, ctrl)
            .unwrap();
        assert!(t.unconsumed && !t.global && a == Some(TermAction::ReloadConfig));
        for k in ["b", "c"] {
            let (t, _) = cfg
                .lookup_keybind(&Key::Character(k.into()), Code::Unidentified, ctrl)
                .unwrap();
            assert!(t.unconsumed && t.global);
        }
        let (cfg2, errs2) = AppConfig::parse("keybind = unconsumed:unconsumed:ctrl+a=quit");
        assert!(!errs2.is_empty());
        drop(cfg2);
        // Later same-chord bind replaces the earlier entry wholesale.
        let (cfg3, errs3) = AppConfig::parse(
            "keybind = unconsumed:ctrl+a=reload_config\nkeybind = ctrl+a=quit",
        );
        assert!(errs3.is_empty());
        assert_eq!(cfg3.keybinds.len(), 1);
        let (t, a) = cfg3
            .lookup_keybind(&Key::Character("a".into()), Code::Unidentified, ctrl)
            .unwrap();
        assert!(!t.unconsumed && a == Some(TermAction::Quit));
    }

    #[test]
    fn cursor_style_reference_names() {
        // Ghostty `cursor-style` / `cursor-style-blink` are the only
        // names — no legacy spellings.
        let (cfg, errs) = AppConfig::parse("cursor-style = beam\ncursor-style-blink = false");
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.cursor_shape, CursorShape::Beam);
        assert!(!cfg.cursor_blink);
        let (_, errs2) = AppConfig::parse("cursor-shape = underline\ncursor-blink = true");
        assert_eq!(errs2.len(), 2, "legacy names must be rejected: {errs2:?}");
    }

    #[test]
    fn window_show_tab_bar_parse() {
        for (value, want) in [("always", 1), ("auto", 2), ("never", usize::MAX)] {
            let (cfg, errs) = AppConfig::parse(&format!("window-show-tab-bar = {value}"));
            assert!(errs.is_empty(), "{value}: {errs:?}");
            assert_eq!(cfg.tab_bar_min_tabs, want, "{value}");
        }
        let (_cfg, errs) = AppConfig::parse("window-show-tab-bar = bogus");
        assert!(!errs.is_empty());
    }

    #[test]
    fn padding_color_and_opacity_cells_parse() {
        let (cfg, errs) = AppConfig::parse(
            "window-padding-color = extend\nbackground-opacity-cells = true",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.window_padding_color, WindowPaddingColor::Extend);
        assert!(cfg.background_opacity_cells);
        assert_eq!(cell_bg_alpha(true, 0.5), 0.5);
        assert_eq!(cell_bg_alpha(false, 0.5), 1.0);
        let (cfg2, _) = AppConfig::parse("window-padding-color = extend-always");
        assert_eq!(cfg2.window_padding_color, WindowPaddingColor::ExtendAlways);
        let (_c3, errs3) = AppConfig::parse("window-padding-color = bogus");
        assert!(!errs3.is_empty());
    }

    #[test]
    fn new_split_auto_parses() {
        let (cfg, errs) = AppConfig::parse("keybind = ctrl+alt+z=new_split:auto");
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.keybinds[0].1, Some(TermAction::SplitAuto));
    }

    #[test]
    fn r38_reference_names() {
        // `window-position-x`/`y` are the reference's names — `window-x`/
        // `window-y` are rejected, not aliased.
        let (cfg, errs) =
            AppConfig::parse("window-position-x = 120\nwindow-position-y = 90");
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.window_x, Some(120.0));
        assert_eq!(cfg.window_y, Some(90.0));
        let (_, errs2) = AppConfig::parse("window-x = 120\nwindow-y = 90");
        assert_eq!(errs2.len(), 2, "legacy names rejected: {errs2:?}");

        // `click-repeat-interval` is the reference's name —
        // `click-interval` is rejected.
        let (cfg, errs) = AppConfig::parse("click-repeat-interval = 250");
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.click_interval, 250);
        let (_, errs3) = AppConfig::parse("click-interval = 250");
        assert_eq!(errs3.len(), 1, "legacy name rejected: {errs3:?}");
    }

    #[test]
    fn mouse_shift_capture_parse() {
        // `mouse-shift-capture` is the reference's 4-state key —
        // the bool `mouse-shift-override` is gone.
        for (value, want) in [
            ("false", MouseShiftCapture::False),
            ("true", MouseShiftCapture::True),
            ("always", MouseShiftCapture::Always),
            ("never", MouseShiftCapture::Never),
        ] {
            let (cfg, errs) = AppConfig::parse(&format!("mouse-shift-capture = {value}"));
            assert!(errs.is_empty(), "{value}: {errs:?}");
            assert_eq!(cfg.mouse_shift_capture, want, "{value}");
        }
        let (_, errs) = AppConfig::parse("mouse-shift-override = true");
        assert_eq!(errs.len(), 1, "legacy name rejected: {errs:?}");
        let (_, errs) = AppConfig::parse("mouse-shift-capture = bogus");
        assert!(!errs.is_empty());
        // Default matches the reference: `false`.
        let (d, _) = AppConfig::parse("");
        assert_eq!(d.mouse_shift_capture, MouseShiftCapture::False);
    }

    #[test]
    fn notify_on_command_finish_parse() {
        let (cfg, errs) = AppConfig::parse(
            "notify-on-command-finish = always\nnotify-on-command-finish-after = 500ms",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.notify_on_command_finish, NotifyWhen::Always);
        assert_eq!(cfg.notify_on_command_finish_after, 0.5);
        for (value, want) in [("5s", 5.0), ("2m", 120.0), ("1h", 3600.0), ("3", 3.0)] {
            assert_eq!(parse_notify_after(value), Some(want), "{value}");
        }
        assert_eq!(parse_notify_after("bogus"), None);
        let (cfg2, _) = AppConfig::parse("notify-on-command-finish = unfocused");
        assert_eq!(cfg2.notify_on_command_finish, NotifyWhen::Unfocused);
        let (d, _) = AppConfig::parse("");
        assert_eq!(d.notify_on_command_finish, NotifyWhen::No);
        assert_eq!(d.notify_on_command_finish_after, 5.0);
    }

    #[test]
    fn canonical_close_and_toggle_actions() {
        // Reference names: `toggle_fullscreen`, `close_all_tabs`,
        // `close_other_tabs`. The old `fullscreen`/`quick_terminal`/
        // `toggle_pane_zoom` spellings are rejected, not aliased.
        let (cfg, errs) = AppConfig::parse(
            "keybind = f11=toggle_fullscreen\nkeybind = ctrl+alt+w=close_all_tabs\nkeybind = ctrl+alt+o=close_other_tabs",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.keybinds[0].1, Some(TermAction::Fullscreen));
        assert_eq!(cfg.keybinds[1].1, Some(TermAction::CloseAllTabs));
        assert_eq!(cfg.keybinds[2].1, Some(TermAction::CloseOtherTabs));
        for legacy in [
            "keybind = f11=fullscreen",
            "keybind = f12=quick_terminal",
            "keybind = ctrl+alt+z=toggle_pane_zoom",
        ] {
            let (_, errs) = AppConfig::parse(legacy);
            assert_eq!(errs.len(), 1, "{legacy} rejected: {errs:?}");
        }
    }

    #[test]
    fn selection_and_editor_actions() {
        // r39 rows: scroll_to_selection, clear_selection,
        // write_last_output_file, open_config — reference action names.
        let (cfg, errs) = AppConfig::parse(
            "keybind = ctrl+alt+s=scroll_to_selection\nkeybind = ctrl+alt+c=clear_selection\nkeybind = ctrl+alt+l=write_last_output_file\nkeybind = ctrl+alt+,=open_config",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.keybinds[0].1, Some(TermAction::ScrollToSelection));
        assert_eq!(cfg.keybinds[1].1, Some(TermAction::ClearSelection));
        assert_eq!(
            cfg.keybinds[2].1,
            Some(TermAction::WriteLastOutputFile(FileSink::Open))
        );
        assert_eq!(cfg.keybinds[3].1, Some(TermAction::OpenConfig));
    }

    #[test]
    fn cursor_key_and_hide_all_windows_parse() {
        use crate::keys::CursorKeyDir as D;
        let (cfg, errs) = AppConfig::parse(
            "keybind = ctrl+alt+u=cursor_key:up\nkeybind = ctrl+alt+h=cursor_key:home\nkeybind = ctrl+alt+n=cursor_key:page_down\nkeybind = ctrl+alt+m=hide_all_windows\nkeybind = ctrl+alt+b=cursor_key:diagonal",
        );
        assert_eq!(errs.len(), 1, "bad dir rejected: {errs:?}");
        assert_eq!(
            cfg.keybinds[0].1,
            Some(TermAction::CursorKey(D::Up))
        );
        assert_eq!(
            cfg.keybinds[1].1,
            Some(TermAction::CursorKey(D::Home))
        );
        assert_eq!(
            cfg.keybinds[2].1,
            Some(TermAction::CursorKey(D::PageDown))
        );
        assert_eq!(cfg.keybinds[3].1, Some(TermAction::HideAllWindows));
        // Works inside `sequence:` too.
        let (cfg, errs) = AppConfig::parse(
            "keybind = ctrl+alt+s=sequence:cursor_key:left,cursor_key:up",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(
            cfg.keybinds[0].1,
            Some(TermAction::Sequence(vec![
                TermAction::CursorKey(D::Left),
                TermAction::CursorKey(D::Up),
            ]))
        );
    }

    #[test]
    fn vt_kam_allowed_parse() {
        let (cfg, errs) = AppConfig::parse("vt-kam-allowed = true");
        assert!(errs.is_empty(), "{errs:?}");
        assert!(cfg.vt_kam_allowed);
        let (cfg, errs) = AppConfig::parse("vt-kam-allowed = false");
        assert!(errs.is_empty(), "{errs:?}");
        assert!(!cfg.vt_kam_allowed);
        let (_, errs) = AppConfig::parse("vt-kam-allowed = maybe");
        assert_eq!(errs.len(), 1);
    }

    #[test]
    fn font_variation_parse() {
        use crate::fonts::parse_font_variation;
        // `wght=700,wdth=85` → two axes; bad tag/number/entry → whole
        // spec rejected (all-or-nothing like the reference).
        let v = parse_font_variation("wght=700, wdth=85").unwrap();
        assert_eq!(v.len(), 2);
        assert!(parse_font_variation("wght").is_none());
        assert!(parse_font_variation("wght=seven").is_none());
        assert!(parse_font_variation("wght=700,bad").is_none());
        assert!(parse_font_variation("wght=700,,wdth=85").is_none());
        let (cfg, errs) = AppConfig::parse(
            "font-variation = wght=200\nfont-variation-bold = wght=100",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.font_variation.as_deref(), Some("wght=200"));
        assert_eq!(cfg.font_variation_bold.as_deref(), Some("wght=100"));
        assert!(cfg.font_variation_italic.is_none());
        assert!(cfg.font_variation_bold_italic.is_none());
    }

    #[test]
    fn cursor_click_to_move_parse() {
        let (cfg, errs) = AppConfig::parse("cursor-click-to-move = true");
        assert!(errs.is_empty(), "{errs:?}");
        assert!(cfg.cursor_click_to_move);
        let (d, _) = AppConfig::parse("");
        assert!(!d.cursor_click_to_move);
    }

    #[test]
    fn r41_reference_names() {
        // `adjust_selection:*` full direction set (Ghostty names).
        let (cfg, errs) = AppConfig::parse(
            "keybind = ctrl+alt+arrowleft=adjust_selection:left\n\
             keybind = ctrl+alt+arrowdown=adjust_selection:down\n\
             keybind = ctrl+alt+pageup=adjust_selection:page_up\n\
             keybind = ctrl+alt+escape=adjust_selection:escape",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(
            cfg.keybinds[0].1,
            Some(TermAction::AdjustSelection(AdjustSel::Left))
        );
        assert_eq!(
            cfg.keybinds[3].1,
            Some(TermAction::AdjustSelection(AdjustSel::Escape))
        );
        // `toggle_mouse_visibility` + canonical `toggle_command_palette`;
        // the bare `palette` action name is rejected.
        let (cfg2, errs2) = AppConfig::parse(
            "keybind = f6=toggle_mouse_visibility\nkeybind = ctrl+shift+p=toggle_command_palette",
        );
        assert!(errs2.is_empty(), "{errs2:?}");
        assert_eq!(cfg2.keybinds[0].1, Some(TermAction::ToggleMouseVisibility));
        assert_eq!(cfg2.keybinds[1].1, Some(TermAction::Palette));
        let (_, errs3) = AppConfig::parse("keybind = ctrl+shift+p=palette");
        assert_eq!(errs3.len(), 1, "palette rejected: {errs3:?}");
        // `scroll-to-bottom` item set: defaults keystroke on, output off;
        // `no-` negates, empty clears, unknown items error.
        let (d, _) = AppConfig::parse("");
        assert!(d.scroll_bottom_keystroke && !d.scroll_bottom_output);
        let (cfg4, errs4) = AppConfig::parse(
            "scroll-to-bottom = keystroke,output",
        );
        assert!(errs4.is_empty(), "{errs4:?}");
        assert!(cfg4.scroll_bottom_keystroke && cfg4.scroll_bottom_output);
        let (cfg5, _) = AppConfig::parse("scroll-to-bottom = no-keystroke");
        assert!(!cfg5.scroll_bottom_keystroke && !cfg5.scroll_bottom_output);
        let (cfg6, _) = AppConfig::parse("scroll-to-bottom = output");
        assert!(cfg6.scroll_bottom_keystroke && cfg6.scroll_bottom_output);
        let (cfg7, _) = AppConfig::parse("scroll-to-bottom = ");
        assert!(!cfg7.scroll_bottom_keystroke && !cfg7.scroll_bottom_output);
        let (_, errs8) = AppConfig::parse("scroll-to-bottom = bogus");
        assert_eq!(errs8.len(), 1, "{errs8:?}");
        // `scroll-on-input` is not a reference name — rejected.
        let (_, errs9) = AppConfig::parse("scroll-on-input = false");
        assert_eq!(errs9.len(), 1, "{errs9:?}");
    }

    #[test]
    fn goto_tab_n_and_prompt_actions() {
        // Reference names: `goto_tab:N` and `jump_to_prompt:±N`.
        let (cfg, errs) = AppConfig::parse(
            "keybind = ctrl+3=goto_tab:3\nkeybind = ctrl+shift+arrowup=jump_to_prompt:-1",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.keybinds[0].1, Some(TermAction::SelectTab(3)));
        assert_eq!(cfg.keybinds[1].1, Some(TermAction::JumpToPrompt(-1)));
    }

    #[test]
    fn keybind_payload_actions() {
        let (cfg, errs) = AppConfig::parse(
            "keybind = ctrl+alt+p=text:\"MARK A B\"\nkeybind = ctrl+alt+q=csi:\"18t\"\nkeybind = ctrl+alt+r=esc:\"[H\"\nkeybind = ctrl+alt+s=scroll_to_fraction:0.5\nkeybind = ctrl+alt+w=scroll_to_row:12",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(
            cfg.keybinds[0].1,
            Some(TermAction::TypeText("MARK A B".into()))
        );
        assert_eq!(cfg.keybinds[1].1, Some(TermAction::CsiSeq("18t".into())));
        assert_eq!(cfg.keybinds[2].1, Some(TermAction::EscSeq("[H".into())));
        assert_eq!(
            cfg.keybinds[3].1,
            Some(TermAction::ScrollToFraction(0.5))
        );
        assert_eq!(cfg.keybinds[4].1, Some(TermAction::ScrollToRow(12)));
    }

    #[test]
    fn sequence_parses_and_splits() {
        let (cfg, errs) = AppConfig::parse(
            "keybind = ctrl+alt+s=sequence:text:\"SEQ1\",copy_to_clipboard\nkeybind = ctrl+alt+t=sequence:text:\"a,b\",text:\"c\"\nkeybind = ctrl+alt+u=undo\nkeybind = ctrl+alt+m=toggle_mark\nkeybind = ctrl+alt+j=jump_to_mark:previous\nkeybind = ctrl+alt+k=jump_to_mark:next",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(
            cfg.keybinds[0].1,
            Some(TermAction::Sequence(vec![
                TermAction::TypeText("SEQ1".into()),
                TermAction::Copy,
            ]))
        );
        // A comma inside a quoted payload is not a separator.
        assert_eq!(
            cfg.keybinds[1].1,
            Some(TermAction::Sequence(vec![
                TermAction::TypeText("a,b".into()),
                TermAction::TypeText("c".into()),
            ]))
        );
        assert_eq!(cfg.keybinds[2].1, Some(TermAction::Undo));
        assert_eq!(cfg.keybinds[3].1, Some(TermAction::ToggleMark));
        assert_eq!(cfg.keybinds[4].1, Some(TermAction::JumpToMark(-1)));
        assert_eq!(cfg.keybinds[5].1, Some(TermAction::JumpToMark(1)));
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
    fn theme_pair_parses() {
        let (cfg, errs) =
            AppConfig::parse("theme = light:solarized-light,dark:solarized-dark");
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(
            cfg.theme,
            ThemeRef::Pair {
                light: "solarized-light".into(),
                dark: "solarized-dark".into()
            }
        );
        // Reversed order works; a lone half falls back to the unknown
        // theme error path (not silently a Named theme).
        let (cfg2, errs2) =
            AppConfig::parse("theme = dark:solarized-dark,light:solarized-light");
        assert!(errs2.is_empty(), "{errs2:?}");
        assert!(matches!(cfg2.theme, ThemeRef::Pair { .. }));
        let (_, errs3) = AppConfig::parse("theme = light:solarized-light");
        assert_eq!(errs3.len(), 1);
    }

    #[test]
    fn bold_color_faint_opacity_selection_clear_parse() {
        let (cfg, errs) = AppConfig::parse(
            "bold-color = #ff0088\nfaint-opacity = 0.25\nselection-clear-on-typing = false",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(
            cfg.bold_color,
            BoldColor::Color(Rgb {
                r: 0xff,
                g: 0x00,
                b: 0x88
            })
        );
        assert!((cfg.faint_opacity - 0.25).abs() < f32::EPSILON);
        assert!(!cfg.selection_clear_on_typing);

        let (cfg, errs) = AppConfig::parse("bold-color = bright\nfaint-opacity = 9.0");
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.bold_color, BoldColor::Bright);
        // Clamped into 0..=1 like Ghostty.
        assert!((cfg.faint_opacity - 1.0).abs() < f32::EPSILON);

        // The legacy alias is gone — `bold-color` is the only name.
        let (_, errs) = AppConfig::parse("bold-is-bright = false");
        assert_eq!(errs.len(), 1);
    }

    #[test]
    fn r49_keys_parse() {
        // `keybind = clear` empties the user list AND suppresses builtin
        // defaults; a bind declared after it still registers.
        let (cfg, errs) = AppConfig::parse(
            "keybind = ctrl+shift+t=new_tab\nkeybind = clear\nkeybind = ctrl+shift+z=new_tab",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert!(cfg.keybinds_cleared);
        assert_eq!(cfg.keybinds.len(), 1);
        assert_eq!(cfg.keybinds[0].1, Some(TermAction::NewTab));

        let (cfg, errs) = AppConfig::parse(
            "desktop-notifications = false\nabnormal-command-exit-runtime = 120\nadjust-cursor-height = 60%",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert!(!cfg.desktop_notifications);
        assert_eq!(cfg.abnormal_command_exit_runtime, 120);
        assert_eq!(cfg.adjust_cursor_height, 60);

        // `bold-is-bright` is a legacy key name — rejected, not aliased.
        let (_, errs) = AppConfig::parse("bold-is-bright = true");
        assert_eq!(errs.len(), 1);
        // 0 selects the framework default again; garbage errors.
        let (cfg, errs) = AppConfig::parse("adjust-cursor-height = 0");
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.adjust_cursor_height, 0);
        let (_, errs) = AppConfig::parse("adjust-cursor-height = x");
        assert_eq!(errs.len(), 1);

        let (cfg, errs) = AppConfig::parse("grapheme-width-method = legacy");
        assert!(errs.is_empty(), "{errs:?}");
        assert!(matches!(
            cfg.grapheme_width_method,
            GraphemeWidthMethod::Legacy
        ));
        let (cfg, errs) = AppConfig::parse("grapheme-width-method = unicode");
        assert!(errs.is_empty(), "{errs:?}");
        assert!(matches!(
            cfg.grapheme_width_method,
            GraphemeWidthMethod::Unicode
        ));
        let (_, errs) = AppConfig::parse("grapheme-width-method = mixed");
        assert_eq!(errs.len(), 1);
    }

    #[test]
    fn quick_terminal_size_parses() {
        let (cfg, errs) = AppConfig::parse("quick-terminal-size = 30%");
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(
            cfg.quick_terminal_size,
            Some((QuickTermSize::Percent(30.0), None))
        );
        let (cfg, errs) = AppConfig::parse("quick-terminal-size = 50%,500px");
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(
            cfg.quick_terminal_size,
            Some((QuickTermSize::Percent(50.0), Some(QuickTermSize::Px(500.0))))
        );
        // Bare numbers are a config error (Ghostty).
        let (_, errs) = AppConfig::parse("quick-terminal-size = 300");
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
             cursor-color = #fc0\nselection-foreground = 0xffffff\n\
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
        let (cfg, errs) = AppConfig::parse(
            "mouse-scroll-multiplier = 3.5\nconfirm-close-surface = false",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert!((cfg.mouse_scroll_multiplier - 3.5).abs() < f32::EPSILON);
        assert_eq!(cfg.confirm_close, ConfirmCloseSurface::False);
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
        upsert_config_key(&path, "cursor-style-blink", "true");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# comment"));
        assert!(text.contains("font-size = 13"));
        assert!(text.contains("theme = solarized-dark"));
        assert!(text.contains("cursor-style-blink = true"));
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
        // `fullscreen` is the reference's key — `window-fullscreen`
        // is rejected, not aliased.
        let (cfg, errs) = AppConfig::parse("fullscreen = true\n");
        assert!(errs.is_empty(), "{errs:?}");
        assert!(cfg.window_fullscreen);
        let (_, errs2) = AppConfig::parse("window-fullscreen = true\n");
        assert_eq!(errs2.len(), 1, "legacy name rejected: {errs2:?}");
    }

    #[test]
    fn parses_r19_keys() {
        let (cfg, errs) = AppConfig::parse(
            "mouse-shift-capture = always\nclipboard-read = deny\ncursor-invert-fg-bg = false\n",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.mouse_shift_capture, MouseShiftCapture::Always);
        assert_eq!(cfg.clipboard_read, ClipboardRead::Deny);
        assert!(!cfg.cursor_invert_fg_bg);
        // Defaults are the Ghostty ones: capture off, ask, invert on.
        let (d, _) = AppConfig::parse("");
        assert_eq!(d.mouse_shift_capture, MouseShiftCapture::False);
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

    #[test]
    fn font_synthetic_tokens_or_into_allow_set() {
        // Each line ORs tokens into the allow-set; an empty value
        // (or a line with no known tokens) allows nothing.
        let (cfg, errs) = AppConfig::parse(
            "font-synthetic-style = bold\nfont-synthetic-style = italic|bold\n",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.font_synthetic, Some((true, true)));
        let (cfg, errs) = AppConfig::parse("font-synthetic-style = \n");
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.font_synthetic, Some((false, false)));
        let (_, errs) = AppConfig::parse("font-synthetic-style = wobbly");
        assert_eq!(errs.len(), 1);
    }

    #[test]
    fn codepoint_map_parses_ranges() {
        let (cfg, errs) = AppConfig::parse(
            "font-codepoint-map = U+2500-U+257F=DejaVu Sans Mono\n\
             font-codepoint-map = U+1F600=Noto Color Emoji\n",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(
            cfg.font_codepoint_map,
            vec![
                (0x2500, 0x257F, "DejaVu Sans Mono".to_string()),
                (0x1F600, 0x1F600, "Noto Color Emoji".to_string()),
            ]
        );
        for bad in ["font-codepoint-map = U+ZZZZ=X", "font-codepoint-map = U+2-U+1=X"] {
            let (_, errs) = AppConfig::parse(bad);
            assert_eq!(errs.len(), 1, "{bad}");
        }
    }

    #[test]
    fn canonical_tab_and_surface_actions() {
        // Reference names only: goto_tab/move_tab/previous_tab,
        // ignore, close_tab vs close_surface.
        let (cfg, errs) = AppConfig::parse(
            "keybind = ctrl+alt+g=goto_tab:3\nkeybind = ctrl+alt+m=move_tab:-1\n\
             keybind = ctrl+alt+p=previous_tab\nkeybind = ctrl+alt+i=ignore\n\
             keybind = ctrl+alt+c=close_surface\nkeybind = ctrl+alt+t=close_tab",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.keybinds[0].1, Some(TermAction::SelectTab(3)));
        assert_eq!(cfg.keybinds[1].1, Some(TermAction::MoveTab(-1)));
        assert_eq!(cfg.keybinds[2].1, Some(TermAction::PrevTab));
        assert_eq!(cfg.keybinds[3].1, Some(TermAction::Ignore));
        assert_eq!(cfg.keybinds[4].1, Some(TermAction::CloseSurface));
        assert_eq!(cfg.keybinds[5].1, Some(TermAction::CloseTab));
    }

    #[test]
    fn middle_click_action_enum() {
        let (cfg, errs) =
            AppConfig::parse("middle-click-action = clipboard-paste");
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.middle_click_action, MiddleClickAction::ClipboardPaste);
        let (cfg, errs) = AppConfig::parse("middle-click-action = primary-paste");
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.middle_click_action, MiddleClickAction::PrimaryPaste);
        let (cfg, errs) = AppConfig::parse("middle-click-action = ignore");
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.middle_click_action, MiddleClickAction::Ignore);
        for bad in ["middle-click-action = wobbly", "middle-click-paste = true"] {
            let (_, errs) = AppConfig::parse(bad);
            assert_eq!(errs.len(), 1, "{bad}");
        }
    }

    #[test]
    fn confirm_close_surface_three_state() {
        let (cfg, errs) = AppConfig::parse("confirm-close-surface = always");
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.confirm_close, ConfirmCloseSurface::Always);
        let (cfg, errs) = AppConfig::parse("confirm-close-surface = true");
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.confirm_close, ConfirmCloseSurface::True);
        // The reference's names only: `confirm-close` and
        // `confirm-close-tab` are unknown keys.
        for bad in ["confirm-close = true", "confirm-close-surface = wobbly"] {
            let (_, errs) = AppConfig::parse(bad);
            assert_eq!(errs.len(), 1, "{bad}");
        }
    }

    #[test]
    fn bell_features_reference_mapping() {
        // `attention` drives only the badge channel; `title` only the
        // title prepend; `no-` forms disable one at a time; `system`
        // and `audio` both drive the XBell channel.
        let (cfg, errs) = AppConfig::parse("bell-features = title");
        assert!(errs.is_empty(), "{errs:?}");
        assert!(cfg.bell_title);
        assert!(cfg.bell_attention, "title leaves attention untouched");
        let (cfg, errs) = AppConfig::parse("bell-features = attention,no-title");
        assert!(errs.is_empty(), "{errs:?}");
        assert!(cfg.bell_attention);
        assert!(!cfg.bell_title);
        let (cfg, errs) = AppConfig::parse("bell-features = no-attention");
        assert!(errs.is_empty(), "{errs:?}");
        assert!(!cfg.bell_attention);
        assert!(cfg.bell_title);
        let (cfg, errs) = AppConfig::parse("bell-features = system,audio");
        assert!(errs.is_empty(), "{errs:?}");
        assert!(cfg.audible_bell);
        // `border` enables the pane ring; `no-border` turns it back off.
        let (cfg, errs) = AppConfig::parse("bell-features = border");
        assert!(errs.is_empty(), "{errs:?}");
        assert!(cfg.bell_border);
        let (cfg, errs) = AppConfig::parse("bell-features = border,no-border");
        assert!(errs.is_empty(), "{errs:?}");
        assert!(!cfg.bell_border);
        // Empty `bell-features =` turns every channel off.
        let (cfg, errs) = AppConfig::parse("bell-features =");
        assert!(errs.is_empty(), "{errs:?}");
        assert!(!cfg.bell_title && !cfg.bell_attention && !cfg.audible_bell);
        let (_, errs) = AppConfig::parse("bell-features = wobbly");
        assert_eq!(errs.len(), 1);
        // `visual` is not a reference item — the pane flash is our own
        // `visual-bell` option, not part of the `bell-features` set.
        let (_, errs) = AppConfig::parse("bell-features = visual");
        assert_eq!(errs.len(), 1);
    }

    #[test]
    fn tab_title_and_copy_title_actions() {
        let (cfg, errs) = AppConfig::parse(
            "keybind = ctrl+alt+1=prompt_tab_title\nkeybind = ctrl+alt+2=set_tab_title:mymark\n\
             keybind = ctrl+alt+3=set_surface_title:myssn\nkeybind = ctrl+alt+4=set_tab_title:\n\
             keybind = ctrl+alt+5=copy_title_to_clipboard\nkeybind = ctrl+alt+6=copy_url_to_clipboard",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.keybinds[0].1, Some(TermAction::PromptTabTitle));
        assert_eq!(
            cfg.keybinds[1].1,
            Some(TermAction::SetTabTitle("mymark".into()))
        );
        assert_eq!(
            cfg.keybinds[2].1,
            Some(TermAction::SetSurfaceTitle("myssn".into()))
        );
        assert_eq!(cfg.keybinds[3].1, Some(TermAction::SetTabTitle(String::new())));
        assert_eq!(cfg.keybinds[4].1, Some(TermAction::CopyTitleToClipboard));
        assert_eq!(cfg.keybinds[5].1, Some(TermAction::CopyUrlToClipboard));
    }

    #[test]
    fn legacy_action_and_key_names_rejected() {
        // The reference's names only — no legacy aliases.
        for bad in [
            "keybind = ctrl+alt+a=select_tab_3",
            "keybind = ctrl+alt+a=prompt_prev",
            "keybind = ctrl+alt+a=prompt_next",
            "keybind = ctrl+alt+a=scroll_line_up",
            "keybind = ctrl+alt+a=move_tab_left",
            "keybind = ctrl+alt+a=font_bigger",
            "keybind = ctrl+alt+a=copy",
            "keybind = ctrl+alt+a=paste",
            "keybind = ctrl+alt+a=paste_selection",
            "word-select-chars = abc",
            "clipboard-trim = true",
            "audible-bell = true",
            "visual-bell = true",
            "font-synthetic = bold",
        ] {
            let (_, errs) = AppConfig::parse(bad);
            assert!(!errs.is_empty(), "{bad} must be rejected");
        }
    }

    #[test]
    fn palette_entry_parses_reference_fields() {
        let (cfg, errs) = AppConfig::parse(
            "command-palette-entry = title:Say hi, description:Types a greeting, action:\"text:printf q\\n\"\ncommand-palette-entry = title:Tab two, action:goto_tab:2",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.palette_entries.len(), 2);
        assert_eq!(cfg.palette_entries[0].title, "Say hi");
        assert_eq!(cfg.palette_entries[0].description, "Types a greeting");
        // Entry actions resolve through the same table keybinds use.
        assert_eq!(
            action_from_str(
                &cfg.palette_entries[0].action.to_ascii_lowercase(),
                &cfg.palette_entries[0].action,
            ),
            Some(TermAction::TypeText("printf q\n".to_string()))
        );
        assert_eq!(
            action_from_str(&cfg.palette_entries[1].action, &cfg.palette_entries[1].action),
            Some(TermAction::SelectTab(2))
        );
        let (_, errs) = AppConfig::parse("command-palette-entry = title:NoAction");
        assert_eq!(errs.len(), 1);
    }

    #[test]
    fn r48_keys_parse() {
        let (cfg, errs) = AppConfig::parse(
            "link = GH-[0-9]+\nlink = [0-9a-f]{40}\nenquiry-response = \\x1b[?6;7;8c\nclipboard-paste-bracketed-safe = false\nimage-storage-limit = 1024",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.link_patterns.len(), 2);
        assert_eq!(cfg.enquiry_response.as_deref(), Some("\x1b[?6;7;8c"));
        assert!(!cfg.paste_bracketed_safe);
        assert_eq!(cfg.image_storage_limit, 1024);
        let (_, errs) = AppConfig::parse("link = ([invalid\nenquiry-response = \\xzz");
        assert_eq!(errs.len(), 2, "{errs:?}");
    }

    #[test]
    fn config_file_include_expands() {
        let dir = std::env::temp_dir().join(format!("hydroterm-inc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("extra.conf"), "font-size = 22\n").unwrap();
        std::fs::write(dir.join("loop.conf"), "config-file = loop.conf\n").unwrap();
        std::fs::write(
            dir.join("main"),
            "config-file = extra.conf\nconfig-file = loop.conf\nfullscreen = true\n",
        )
        .unwrap();
        let (cfg, errs) = AppConfig::load(&dir.join("main"));
        // Included value applied, the cycle warned (not fatal), and the
        // directive lines were recorded.
        assert_eq!(cfg.font_size, 22.0);
        assert!(cfg.window_fullscreen);
        assert!(errs.iter().any(|e| e.contains("cycle")), "{errs:?}");
        // 2 top-level directives + the `config-file` line spliced in
        // from loop.conf's own body (its expansion hit the cycle guard).
        assert_eq!(cfg.config_files.len(), 3, "{:?}", cfg.config_files);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn command_parses_reference_prefixes() {
        let (cfg, errs) = AppConfig::parse(
            "command = shell:printf hi; exec $SHELL\ninitial-command = direct:echo -n x",
        );
        assert!(errs.is_empty(), "{errs:?}");
        // `shell:` and bare values go through `sh -c`; `direct:` is argv.
        assert_eq!(
            cfg.command.unwrap(),
            ["/bin/sh", "-c", "printf hi; exec $SHELL"]
        );
        assert_eq!(cfg.initial_command.unwrap(), ["echo", "-n", "x"]);
        let (cfg, errs) = AppConfig::parse("command = tmux attach");
        assert!(errs.is_empty());
        assert_eq!(cfg.command.unwrap(), ["/bin/sh", "-c", "tmux attach"]);
        // An empty value clears the option.
        let (cfg, errs) = AppConfig::parse("command = ");
        assert!(errs.is_empty());
        assert!(cfg.command.is_none());
    }

    #[test]
    fn resize_overlay_parses_reference_enum() {
        let (cfg, errs) = AppConfig::parse(
            "resize-overlay = after-first\nresize-overlay-position = top-right\nresize-overlay-duration = 1h30m45s750ms",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.resize_overlay, ResizeOverlay::AfterFirst);
        assert_eq!(cfg.resize_overlay_position, ResizeOverlayPosition::TopRight);
        // 1h + 30m + 45s + 750ms compounds.
        assert_eq!(cfg.resize_overlay_ms, 3_600_000 + 1_800_000 + 45_000 + 750);
        // Deprecated bool spellings still parse (always/never).
        let (cfg, errs) = AppConfig::parse("resize-overlay = false\nresize-overlay = always");
        assert!(errs.is_empty());
        assert_eq!(cfg.resize_overlay, ResizeOverlay::Always);
        let (_, errs) = AppConfig::parse("resize-overlay = purple\nresize-overlay-duration = soon");
        assert_eq!(errs.len(), 2, "{errs:?}");
        for pos in [
            "center", "top-left", "top-center", "top-right",
            "bottom-left", "bottom-center", "bottom-right",
        ] {
            let (cfg, errs) = AppConfig::parse(&format!("resize-overlay-position = {pos}"));
            assert!(errs.is_empty(), "{pos}: {errs:?}");
            let _ = cfg;
        }
    }

    #[test]
    fn osc_color_report_format_parses() {
        let (cfg, errs) = AppConfig::parse("osc-color-report-format = 8-bit");
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.osc_color_report_format, OscColorReportFormat::Bits8);
        let (cfg, errs) = AppConfig::parse("osc-color-report-format = none");
        assert!(errs.is_empty());
        assert_eq!(cfg.osc_color_report_format, OscColorReportFormat::None);
        let (cfg, _) = AppConfig::parse("");
        assert_eq!(cfg.osc_color_report_format, OscColorReportFormat::Bits16);
        let (_, errs) = AppConfig::parse("osc-color-report-format = 32-bit");
        assert_eq!(errs.len(), 1);
    }

    #[test]
    fn scrollback_limit_aliases_parse() {
        // `scrollback-limit-lines` is the reference's canonical name;
        // `scrollback-limit` (1.3 name) aliases too.
        for key in ["scrollback", "scrollback-limit", "scrollback-limit-lines"] {
            let (cfg, errs) = AppConfig::parse(&format!("{key} = 5000"));
            assert!(errs.is_empty(), "{key}: {errs:?}");
            assert_eq!(cfg.scrollback, 5000);
        }
        let (_, errs) = AppConfig::parse("scrollback-limit-bytes = 1048576");
        assert!(errs.is_empty());
    }

    #[test]
    fn parses_r50_keys() {
        let (cfg, errs) = AppConfig::parse(
            "window-theme = ghostty\nfont-style-bold = Demi Bold\n\
                    font-style-italic = Light Italic\nfont-style-bold-italic = Bold Italic\n\
                    font-thicken = true\nfont-thicken-strength = 128\n\
                    window-title-font-family = DejaVu Serif\n",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.window_theme, WindowTheme::Ghostty);
        assert_eq!(cfg.font_style_bold.as_deref(), Some("Demi Bold"));
        assert_eq!(cfg.font_style_italic.as_deref(), Some("Light Italic"));
        assert_eq!(cfg.font_style_bold_italic.as_deref(), Some("Bold Italic"));
        assert_eq!(cfg.font_thicken_strength, 128);
        assert_eq!(
            cfg.window_title_font_family.as_deref(),
            Some("DejaVu Serif")
        );

        // `system` parses; a bogus value and an out-of-range strength error.
        let (cfg2, errs) = AppConfig::parse("window-theme = system\n");
        assert!(errs.is_empty());
        assert_eq!(cfg2.window_theme, WindowTheme::System);
        let (_, errs) = AppConfig::parse("window-theme = purple\n");
        assert_eq!(errs.len(), 1);
        let (_, errs) = AppConfig::parse("font-thicken-strength = 300\n");
        assert_eq!(errs.len(), 1);
        // Empty title-font clears the override.
        let (cfg3, errs) = AppConfig::parse("window-title-font-family = \n");
        assert!(errs.is_empty());
        assert!(cfg3.window_title_font_family.is_none());
    }

    #[test]
    fn parses_r51_keys() {
        let (cfg, errs) = AppConfig::parse(
            "app-notifications = no-clipboard-copy\n                    selection-clear-on-copy = true\n                    undo-timeout = 30s\n                    title-report = true\n                    search-foreground = #101418\n                    search-background = #ffd75f\n                    search-selected-foreground = #101418\n                    search-selected-background = #ffaf00\n",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert!(!cfg.app_notify_clipboard_copy);
        assert!(cfg.app_notify_config_reload);
        assert!(cfg.selection_clear_on_copy);
        assert_eq!(cfg.undo_timeout_ms, 30_000);
        assert!(cfg.title_report);
        assert_eq!(cfg.search_background, Some(Rgb { r: 0xff, g: 0xd7, b: 0x5f }));
        assert_eq!(
            cfg.search_selected_background,
            Some(Rgb { r: 0xff, g: 0xaf, b: 0x00 })
        );

        // `no-` disables one without touching the other; re-adding the
        // bare name turns it back on (repeat key).
        let (cfg2, errs) =
            AppConfig::parse("app-notifications = no-config-reload\napp-notifications = clipboard-copy\n");
        assert!(errs.is_empty());
        assert!(cfg2.app_notify_clipboard_copy);
        assert!(!cfg2.app_notify_config_reload);
        let (_, errs) = AppConfig::parse("app-notifications = no-bell\n");
        assert_eq!(errs.len(), 1);
        let (cfg3, errs) = AppConfig::parse("undo-timeout = 0\n");
        assert!(errs.is_empty());
        assert_eq!(cfg3.undo_timeout_ms, 0);
        let (_, errs) = AppConfig::parse("search-foreground = nope\n");
        assert_eq!(errs.len(), 1);
    }

    #[test]
    fn parses_r52_keys() {
        // `split-preserve-zoom` — navigation flag, `no-` clears it.
        let (cfg, errs) = AppConfig::parse("split-preserve-zoom = navigation\n");
        assert!(errs.is_empty(), "{errs:?}");
        assert!(cfg.split_preserve_zoom_navigation);
        let (cfg, errs) =
            AppConfig::parse("split-preserve-zoom = navigation\nsplit-preserve-zoom = no-navigation\n");
        assert!(errs.is_empty());
        assert!(!cfg.split_preserve_zoom_navigation);
        let (_, errs) = AppConfig::parse("split-preserve-zoom = sideways\n");
        assert_eq!(errs.len(), 1);

        // `config-default-files` is a plain bool key.
        let (cfg, errs) = AppConfig::parse("config-default-files = false\n");
        assert!(errs.is_empty(), "{errs:?}");
        assert!(!cfg.config_default_files);

        // `physical:` triggers carry the resolved Code; matching uses
        // the key position, not the logical character.
        let (cfg, errs) = AppConfig::parse("keybind = physical:ctrl+shift+e=new_tab\n");
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.keybinds.len(), 1);
        assert_eq!(cfg.keybinds[0].0.physical_code, Some(Code::KeyE));
        assert_eq!(cfg.keybinds[0].1, Some(TermAction::NewTab));
        // Matches by code + mods — the logical key is irrelevant.
        let hit = cfg.lookup_keybind(
            &Key::Character("q".into()),
            Code::KeyE,
            Modifiers::CONTROL | Modifiers::SHIFT,
        );
        assert_eq!(hit.map(|(_, a)| a), Some(Some(TermAction::NewTab)));
        // Wrong position → no match.
        assert!(
            cfg.lookup_keybind(
                &Key::Character("e".into()),
                Code::KeyQ,
                Modifiers::CONTROL | Modifiers::SHIFT,
            )
            .is_none()
        );
        // Mods must still match.
        assert!(
            cfg.lookup_keybind(&Key::Character("e".into()), Code::KeyE, Modifiers::CONTROL)
                .is_none()
        );
        // `redo` action parses.
        let (cfg, errs) = AppConfig::parse("keybind = ctrl+shift+r=redo\n");
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.keybinds[0].1, Some(TermAction::Redo));
    }

}
