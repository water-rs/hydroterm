//! Terminal color scheme: the 256-color indexed palette plus named-color
//! resolution (foreground/background/cursor/selection, ANSI 16, dim/bright
//! variants).

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

/// A resolved color pair for one cell.
#[derive(Debug, Clone, Copy)]
pub struct CellColors {
    pub fg: Rgb,
    pub bg: Rgb,
}

const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    Rgb { r, g, b }
}

/// Default theme — a dark scheme close to Ghostty's defaults.
pub struct Palette {
    /// Indexed colors 0..256 (ANSI 0-15, cube 16-231, grays 232-255).
    indexed: [Rgb; 256],
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    /// Dim foreground used when no dim override exists.
    pub dim_foreground: Rgb,
    pub selection_bg: Rgb,
    /// Selection text color; `None` keeps the cell's own fg.
    pub selection_fg: Option<Rgb>,
    /// Badge/chip background (URL hints) — the theme's accent slot.
    pub accent: Rgb,
    /// Text drawn on `accent`, chosen for contrast against it.
    pub accent_fg: Rgb,
}

impl Default for Palette {
    fn default() -> Self {
        let mut indexed = [rgb(0, 0, 0); 256];
        // ANSI 0-7
        indexed[0] = rgb(0x1d, 0x1f, 0x21);
        indexed[1] = rgb(0xcc, 0x66, 0x66);
        indexed[2] = rgb(0xb5, 0xbd, 0x68);
        indexed[3] = rgb(0xf0, 0xc6, 0x74);
        indexed[4] = rgb(0x81, 0xa2, 0xbe);
        indexed[5] = rgb(0xb2, 0x94, 0xbb);
        indexed[6] = rgb(0x8a, 0xbe, 0xb7);
        indexed[7] = rgb(0xc5, 0xc8, 0xc6);
        // Bright 8-15
        indexed[8] = rgb(0x66, 0x66, 0x66);
        indexed[9] = rgb(0xd5, 0x4e, 0x53);
        indexed[10] = rgb(0xb9, 0xca, 0x4a);
        indexed[11] = rgb(0xe7, 0xc5, 0x47);
        indexed[12] = rgb(0x7a, 0xa6, 0xda);
        indexed[13] = rgb(0xc3, 0x97, 0xd8);
        indexed[14] = rgb(0x70, 0xc0, 0xb1);
        indexed[15] = rgb(0xea, 0xea, 0xea);
        // 216-color cube
        let levels = [0u8, 95, 135, 175, 215, 255];
        for i in 0..216usize {
            indexed[16 + i] = rgb(levels[i / 36], levels[(i / 6) % 6], levels[i % 6]);
        }
        // Grayscale ramp
        for i in 0..24usize {
            let v = (8 + i * 10) as u8;
            indexed[232 + i] = rgb(v, v, v);
        }

        Self {
            indexed,
            foreground: rgb(0xc5, 0xc8, 0xc6),
            background: rgb(0x1d, 0x1f, 0x21),
            cursor: rgb(0xc5, 0xc8, 0xc6),
            dim_foreground: rgb(0x62, 0x64, 0x63),
            selection_bg: rgb(0x37, 0x3b, 0x41),
            selection_fg: None,
            accent: rgb(0x81, 0xa2, 0xbe),
            accent_fg: rgb(0x1d, 0x1f, 0x21),
        }
    }
}

impl Palette {
    /// Build a palette from a named theme, keeping the standard
    /// cube/grayscale ramps for indexed colors 16-255.
    pub fn from_theme(theme: &crate::theme::Theme) -> Self {
        let mut palette = Self::default();
        palette.indexed[..16].copy_from_slice(&theme.ansi);
        palette.foreground = theme.foreground;
        palette.background = theme.background;
        palette.cursor = theme.cursor;
        palette.dim_foreground = theme.dim_foreground;
        palette.selection_bg = theme.selection_bg;
        palette.selection_fg = theme.selection_fg;
        palette.accent = theme.accent;
        palette.accent_fg = theme.accent_fg;
        palette
    }

    /// Theme plus the config's explicit overrides — `foreground` /
    /// `background` / `cursor-color` / `selection-color` / `palette = N=#rgb`
    /// (Ghostty naming). The theme gives every slot a default; these keys
    /// replace exactly what they name and nothing else.
    pub fn for_config(cfg: &crate::config::AppConfig) -> Self {
        let mut palette = Self::from_theme(&cfg.resolve_theme());
        if let Some(c) = cfg.foreground {
            palette.foreground = c;
        }
        if let Some(c) = cfg.background {
            palette.background = c;
        }
        if let Some(c) = cfg.cursor_color {
            palette.cursor = c;
        }
        if let Some(c) = cfg.selection_color {
            palette.selection_fg = Some(c);
        }
        for (i, c) in &cfg.palette_overrides {
            palette.indexed[*i as usize] = *c;
        }
        palette
    }

    /// Static lookup by index (for `ColorRequest` replies): 0-255 indexed,
    /// 256 foreground, 257 background, 258 cursor, else foreground.
    pub fn at(&self, index: usize) -> Rgb {
        match index {
            0..=255 => self.indexed[index],
            256 => self.foreground,
            257 => self.background,
            258 => self.cursor,
            _ => self.foreground,
        }
    }

    /// Resolve a cell's fg/bg honoring INVERSE, BOLD-brightening and DIM,
    /// consulting the terminal's runtime color overrides. `bold_bright`
    /// is the `bold-is-bright` config — off keeps bold in the normal slot.
    pub fn resolve(
        &self,
        colors: &Colors,
        fg: Color,
        bg: Color,
        flags: Flags,
        bold_bright: bool,
        min_contrast: f32,
    ) -> CellColors {
        let mut fg = self.resolve_fg(
            colors,
            fg,
            flags.contains(Flags::BOLD) && bold_bright,
            false,
        );
        let mut bg = self.lookup(colors, bg);

        if flags.contains(Flags::DIM) {
            fg = self.dim(fg);
        }
        if flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if min_contrast > 1.0 {
            fg = enforce_contrast(fg, bg, min_contrast);
        }
        CellColors { fg, bg }
    }

    /// Foreground resolution: BOLD lifts named colors to their bright slot.
    pub fn resolve_fg(&self, colors: &Colors, color: Color, bold: bool, dim: bool) -> Rgb {
        let mut color = color;
        if bold && let Color::Named(named) = color {
            color = Color::Named(named.to_bright());
        }
        let rgb = self.lookup(colors, color);
        if dim { self.dim(rgb) } else { rgb }
    }

    /// Blend a color halfway toward the background — the "dim" look.
    pub fn dim(&self, color: Rgb) -> Rgb {
        let blend = |a: u8, b: u8| ((a as u16 + b as u16) / 2) as u8;
        Rgb {
            r: blend(color.r, self.background.r),
            g: blend(color.g, self.background.g),
            b: blend(color.b, self.background.b),
        }
    }

    /// Look up a `Color` through the runtime overrides table first, then the
    /// built-in palette.
    pub fn lookup(&self, colors: &Colors, color: Color) -> Rgb {
        match color {
            Color::Spec(rgb) => rgb,
            Color::Indexed(i) => colors[i as usize].unwrap_or(self.indexed[i as usize]),
            Color::Named(named) => self.named(colors, named),
        }
    }

    /// Resolve a `NamedColor` via overrides then defaults.
    pub fn named(&self, colors: &Colors, named: NamedColor) -> Rgb {
        use NamedColor::*;
        if let Some(rgb) = colors[named] {
            return rgb;
        }
        match named {
            Foreground => self.foreground,
            Background => self.background,
            Cursor => self.cursor,
            DimForeground => self.dim_foreground,
            BrightForeground => self.foreground,
            Black | Red | Green | Yellow | Blue | Magenta | Cyan | White | BrightBlack
            | BrightRed | BrightGreen | BrightYellow | BrightBlue | BrightMagenta
            | BrightCyan | BrightWhite => self.indexed[named as usize],
            DimBlack | DimRed | DimGreen | DimYellow | DimBlue | DimMagenta | DimCyan
            | DimWhite => self.dim(self.indexed[named.to_bright() as usize]),
        }
    }
}

/// Convert a terminal `Rgb` to a peniko color.
pub fn peniko(rgb: Rgb) -> peniko::Color {
    peniko::Color::new([
        rgb.r as f32 / 255.0,
        rgb.g as f32 / 255.0,
        rgb.b as f32 / 255.0,
        1.0,
    ])
}

/// Convert with alpha.
pub fn peniko_alpha(rgb: Rgb, a: f32) -> peniko::Color {
    peniko::Color::new([
        rgb.r as f32 / 255.0,
        rgb.g as f32 / 255.0,
        rgb.b as f32 / 255.0,
        a,
    ])
}


// ---------------------------------------------------------------------------
// `minimum-contrast`: WCAG-ratio floor on cell foreground vs background
// ---------------------------------------------------------------------------

/// Relative luminance of one sRGB channel (IEC 61966-2-1 linearization).
fn channel_luminance(c: u8) -> f64 {
    let v = f64::from(c) / 255.0;
    if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
}

/// WCAG 2.x relative luminance of an sRGB triple.
fn luminance(c: Rgb) -> f64 {
    0.2126 * channel_luminance(c.r)
        + 0.7152 * channel_luminance(c.g)
        + 0.0722 * channel_luminance(c.b)
}

fn contrast_ratio(a: Rgb, b: Rgb) -> f64 {
    let (l1, l2) = (luminance(a), luminance(b));
    let (hi, lo) = if l1 >= l2 { (l1, l2) } else { (l2, l1) };
    (hi + 0.05) / (lo + 0.05)
}

/// Lift `fg` toward the extreme (black or white) that has more contrast
/// against `bg` until the WCAG ratio reaches `min`, binary-searching the
/// blend factor. Mirrors Ghostty's `minimum-contrast` adjustment.
pub fn enforce_contrast(fg: Rgb, bg: Rgb, min: f32) -> Rgb {
    if contrast_ratio(fg, bg) >= min as f64 {
        return fg;
    }
    // Blend toward black or white — the WCAG `+0.05` offsets make the
    // ratio asymmetric, so pick whichever extreme actually scores higher
    // rather than comparing luminance to a midpoint.
    let white = Rgb { r: 255, g: 255, b: 255 };
    let black = Rgb { r: 0, g: 0, b: 0 };
    let target = if contrast_ratio(white, bg) >= contrast_ratio(black, bg) {
        white
    } else {
        black
    };
    let blend = |t: f32| Rgb {
        r: (f32::from(fg.r) + t * (f32::from(target.r) - f32::from(fg.r))).round() as u8,
        g: (f32::from(fg.g) + t * (f32::from(target.g) - f32::from(fg.g))).round() as u8,
        b: (f32::from(fg.b) + t * (f32::from(target.b) - f32::from(fg.b))).round() as u8,
    };
    // If even the extreme cannot reach `min`, take the extreme outright.
    if contrast_ratio(target, bg) < min as f64 {
        return target;
    }
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..12 {
        let mid = (lo + hi) / 2.0;
        if contrast_ratio(blend(mid), bg) >= min as f64 {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    blend(hi)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contrast_enforcement_reaches_the_ratio_floor() {
        // Grey-on-grey (~1.1:1) must lift to >= 4.5.
        let fg = Rgb { r: 128, g: 128, b: 128 };
        let bg = Rgb { r: 120, g: 120, b: 120 };
        let out = enforce_contrast(fg, bg, 4.5);
        assert!(contrast_ratio(out, bg) >= 4.5 - 1e-6, "{out:?}");
        // Already-sufficient colors pass through untouched.
        let fg2 = Rgb { r: 250, g: 250, b: 250 };
        let bg2 = Rgb { r: 10, g: 10, b: 10 };
        assert_eq!(enforce_contrast(fg2, bg2, 4.5), fg2);
        // White-on-white: the extreme wins and the blend stops at the
        // ratio floor — dark, and >= 4.5, not necessarily pure black.
        let out = enforce_contrast(Rgb{r:255,g:255,b:255}, Rgb{r:255,g:255,b:255}, 4.5);
        assert!(contrast_ratio(out, Rgb{r:255,g:255,b:255}) >= 4.5 - 1e-6, "{out:?}");
        // Unreachable floor: max ratio vs a grey ~120 is 4.75 (black),
        // so a 21.0 request must still return the best possible = black.
        let out = enforce_contrast(Rgb{r:128,g:128,b:128}, Rgb{r:120,g:120,b:120}, 21.0);
        assert_eq!(out, Rgb { r: 0, g: 0, b: 0 });
    }
}
