//! Named color themes — ANSI 16 + foreground/background/cursor/selection.

use alacritty_terminal::vte::ansi::Rgb;
use waterui::theme::ColorScheme;

/// One complete color scheme.
pub struct Theme {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    /// Dim foreground used when no dim override exists.
    pub dim_foreground: Rgb,
    /// Badge/chip background (URL hints, toasts) — the accent slot.
    pub accent: Rgb,
    /// Text drawn on `accent` — chosen for contrast against it.
    pub accent_fg: Rgb,
    /// ANSI colors 0-15 (normal 0-7, bright 8-15).
    pub ansi: [Rgb; 16],
}

const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    Rgb { r, g, b }
}

/// Theme catalog — the `theme = <name>` config key resolves against it.
pub const THEMES: &[&str] = &[
    "hydroterm-dark",
    "hydroterm-light",
    "solarized-dark",
    "solarized-light",
];

impl Theme {
    /// Look a theme up by config name.
    pub fn by_name(name: &str) -> Option<Self> {
        match name {
            "hydroterm-dark" | "dark" => Some(Self::hydroterm_dark()),
            "hydroterm-light" | "light" => Some(Self::hydroterm_light()),
            "solarized-dark" => Some(Self::solarized_dark()),
            "solarized-light" => Some(Self::solarized_light()),
            _ => None,
        }
    }

    /// Default dark scheme (Tomorrow-flavored).
    pub fn hydroterm_dark() -> Self {
        Self {
            foreground: rgb(0xc5, 0xc8, 0xc6),
            background: rgb(0x1d, 0x1f, 0x21),
            cursor: rgb(0xc5, 0xc8, 0xc6),
            dim_foreground: rgb(0x62, 0x64, 0x63),
            accent: rgb(0x81, 0xa2, 0xbe),
            accent_fg: rgb(0x1d, 0x1f, 0x21),
            ansi: [
                rgb(0x1d, 0x1f, 0x21),
                rgb(0xcc, 0x66, 0x66),
                rgb(0xb5, 0xbd, 0x68),
                rgb(0xf0, 0xc6, 0x74),
                rgb(0x81, 0xa2, 0xbe),
                rgb(0xb2, 0x94, 0xbb),
                rgb(0x8a, 0xbe, 0xb7),
                rgb(0xc5, 0xc8, 0xc6),
                rgb(0x66, 0x66, 0x66),
                rgb(0xd5, 0x4e, 0x53),
                rgb(0xb9, 0xca, 0x4a),
                rgb(0xe7, 0xc5, 0x47),
                rgb(0x7a, 0xa6, 0xda),
                rgb(0xc3, 0x97, 0xd8),
                rgb(0x70, 0xc0, 0xb1),
                rgb(0xea, 0xea, 0xea),
            ],
        }
    }

    /// Default light scheme.
    pub fn hydroterm_light() -> Self {
        Self {
            foreground: rgb(0x4d, 0x4d, 0x4c),
            background: rgb(0xff, 0xff, 0xff),
            cursor: rgb(0x4d, 0x4d, 0x4c),
            dim_foreground: rgb(0x8e, 0x90, 0x8c),
            accent: rgb(0x42, 0x71, 0xae),
            accent_fg: rgb(0xff, 0xff, 0xff),
            ansi: [
                rgb(0x00, 0x00, 0x00),
                rgb(0xc8, 0x28, 0x29),
                rgb(0x71, 0x8c, 0x00),
                rgb(0xea, 0xb7, 0x00),
                rgb(0x42, 0x71, 0xae),
                rgb(0x89, 0x59, 0xa8),
                rgb(0x3e, 0x99, 0x9f),
                rgb(0xff, 0xff, 0xff),
                rgb(0x00, 0x00, 0x00),
                rgb(0xc8, 0x28, 0x29),
                rgb(0x71, 0x8c, 0x00),
                rgb(0xea, 0xb7, 0x00),
                rgb(0x42, 0x71, 0xae),
                rgb(0x89, 0x59, 0xa8),
                rgb(0x3e, 0x99, 0x9f),
                rgb(0xff, 0xff, 0xff),
            ],
        }
    }

    /// Canonical Solarized dark.
    pub fn solarized_dark() -> Self {
        Self {
            foreground: rgb(0x83, 0x94, 0x96),
            background: rgb(0x00, 0x2b, 0x36),
            cursor: rgb(0x83, 0x94, 0x96),
            dim_foreground: rgb(0x58, 0x6e, 0x75),
            accent: rgb(0x26, 0x8b, 0xd2),
            accent_fg: rgb(0xfd, 0xf6, 0xe3),
            ansi: [
                rgb(0x07, 0x36, 0x42),
                rgb(0xdc, 0x32, 0x2f),
                rgb(0x85, 0x99, 0x00),
                rgb(0xb5, 0x89, 0x00),
                rgb(0x26, 0x8b, 0xd2),
                rgb(0xd3, 0x36, 0x82),
                rgb(0x2a, 0xa1, 0x98),
                rgb(0xee, 0xe8, 0xd5),
                rgb(0x00, 0x2b, 0x36),
                rgb(0xcb, 0x4b, 0x16),
                rgb(0x58, 0x6e, 0x75),
                rgb(0x65, 0x7b, 0x83),
                rgb(0x83, 0x94, 0x96),
                rgb(0x6c, 0x71, 0xc4),
                rgb(0x93, 0xa1, 0xa1),
                rgb(0xfd, 0xf6, 0xe3),
            ],
        }
    }

    /// Canonical Solarized light.
    pub fn solarized_light() -> Self {
        Self {
            foreground: rgb(0x65, 0x7b, 0x83),
            background: rgb(0xfd, 0xf6, 0xe3),
            cursor: rgb(0x65, 0x7b, 0x83),
            dim_foreground: rgb(0x93, 0xa1, 0xa1),
            accent: rgb(0x26, 0x8b, 0xd2),
            accent_fg: rgb(0xfd, 0xf6, 0xe3),
            ansi: [
                rgb(0x07, 0x36, 0x42),
                rgb(0xdc, 0x32, 0x2f),
                rgb(0x85, 0x99, 0x00),
                rgb(0xb5, 0x89, 0x00),
                rgb(0x26, 0x8b, 0xd2),
                rgb(0xd3, 0x36, 0x82),
                rgb(0x2a, 0xa1, 0x98),
                rgb(0xee, 0xe8, 0xd5),
                rgb(0x00, 0x2b, 0x36),
                rgb(0xcb, 0x4b, 0x16),
                rgb(0x58, 0x6e, 0x75),
                rgb(0x65, 0x7b, 0x83),
                rgb(0x83, 0x94, 0x96),
                rgb(0x6c, 0x71, 0xc4),
                rgb(0x93, 0xa1, 0xa1),
                rgb(0xfd, 0xf6, 0xe3),
            ],
        }
    }

    /// `theme = auto`: ask the desktop (freedesktop color-scheme via
    /// gsettings when present); fall back to the default dark theme.
    pub fn auto() -> Self {
        if system_prefers_light() {
            Self::hydroterm_light()
        } else {
            Self::hydroterm_dark()
        }
    }
}

/// Freedesktop color-scheme probe (gsettings); false when absent — KDE
/// and most minimal WMs have no `color-scheme` key, matching `Theme::auto`.
pub fn system_prefers_light() -> bool {
    std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "color-scheme"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("prefer-light"))
        .unwrap_or(false)
}

/// Freedesktop dark-scheme probe (gsettings); false when gsettings or
/// the key is absent — resolving `auto` to light unless the desktop
/// explicitly asks for dark. (Probing `prefer-light` instead falls back
/// to dark on schema-less desktops, which put dark M3 chrome on a light
/// `theme =` palette — the r44 contrast defect.)
pub fn system_prefers_dark() -> bool {
    std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "color-scheme"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("prefer-dark"))
        .unwrap_or(false)
}

/// `window-theme` → the WaterUI color scheme installed into the environment
/// as `Theme::color_scheme` (water-rs/cli#188).
///
/// Reference semantics (`window-theme = auto|system|light|dark|ghostty`):
/// `auto` picks light/dark from the *terminal background* luminance, not
/// the desktop — a light `theme =` palette gets light chrome on a schema-
/// less desktop (the r44 fix was exactly this, on the wrong trigger);
/// `system` probes the desktop preference at call time so a config reload
/// re-reads it; `ghostty` additionally seeds the M3 style from the
/// terminal background at launch (`lib.rs::material_style`) — the scheme
/// here keeps the env signal consistent for anything not token-driven.
pub fn scheme_for(window_theme: &crate::config::WindowTheme, background: &Rgb) -> ColorScheme {
    match window_theme {
        crate::config::WindowTheme::Light => ColorScheme::Light,
        crate::config::WindowTheme::Dark => ColorScheme::Dark,
        crate::config::WindowTheme::Auto | crate::config::WindowTheme::Ghostty => {
            if rgb_is_dark(background) {
                ColorScheme::Dark
            } else {
                ColorScheme::Light
            }
        }
        crate::config::WindowTheme::System => {
            if system_prefers_dark() {
                ColorScheme::Dark
            } else {
                ColorScheme::Light
            }
        }
    }
}

/// Rec.709-ish luminance of the terminal background → dark scheme?
fn rgb_is_dark(rgb: &Rgb) -> bool {
    (0.2126 * f64::from(rgb.r) + 0.7152 * f64::from(rgb.g) + 0.0722 * f64::from(rgb.b)) < 127.5
}
