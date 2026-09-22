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
    /// consulting the terminal's runtime color overrides.
    pub fn resolve(&self, colors: &Colors, fg: Color, bg: Color, flags: Flags) -> CellColors {
        let mut fg = self.resolve_fg(colors, fg, flags.contains(Flags::BOLD), false);
        let mut bg = self.lookup(colors, bg);

        if flags.contains(Flags::DIM) {
            fg = self.dim(fg);
        }
        if flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
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
