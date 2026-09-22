//! Terminal typography on the host's shared parley stack: cell metrics from a
//! probe layout, per-run shaping with fontique's automatic fallback, and the
//! faux-bold / faux-italic synthesis parley reports back per run.
//!
//! The family's `parley::FontContext` is the host's own — installed into the
//! environment at startup and shared through `FontCollection` — so a pane
//! never enumerates system fonts for itself, and the shaped glyph runs hand
//! back `peniko::FontData` the scene's `draw_glyph_run` consumes directly.

use parley::fontique::Synthesis;
use parley::{Alignment, AlignmentOptions, FontFamily, FontFamilyName, FontStyle,
             FontWeight, Layout, LayoutContext, StyleProperty, style::GenericFamily};
use waterui_text::FontCollection;

/// Families asked for first, in preference order. Everything they cannot
/// cover — box drawing, CJK, emoji — falls back through fontique's own
/// cascade rather than a hand-rolled face list.
const PRIMARY_FAMILIES: &[&str] = &[
    "JetBrains Mono",
    "Fira Code",
    "Cascadia Code",
    "Source Code Pro",
    "DejaVu Sans Mono",
    "Liberation Mono",
    "monospace",
];

/// Logical-cell geometry of the terminal grid, derived from a probe layout
/// of the primary family at the configured size.
#[derive(Debug, Clone, Copy)]
pub struct CellMetrics {
    /// Grid cell width in logical units.
    pub cell_w: f32,
    /// Grid cell height in logical units.
    pub cell_h: f32,
    /// Distance from a cell's top edge to the text baseline.
    pub baseline: f32,
    /// Underline offset below the baseline.
    pub underline_pos: f32,
    /// Underline / strikethrough stroke thickness.
    pub stroke: f32,
    /// Strikethrough offset below the baseline.
    pub strikeout_pos: f32,
    /// Em size the runs shape at, in logical units.
    pub size_px: f32,
}

/// Shaping state shared by every draw pass of one pane: the host's font
/// collection handle, a reusable parley `LayoutContext`, and the resolved
/// primary family.
pub struct TermFonts {
    collection: FontCollection,
    layout_cx: LayoutContext<[u8; 4]>,
    /// The primary monospace family as a parley `FontFamily` — a named family
    /// when one of the preferences is installed, generic monospace otherwise.
    family: FontFamily<'static>,
    size_px: f32,
    pub metrics: CellMetrics,
}

impl TermFonts {
    /// Resolve the primary family in `collection` and measure the cell.
    pub fn load(collection: FontCollection, size_pt: f32) -> Self {
        let (family, family_name) = collection.use_fonts(|fonts| {
            for name in PRIMARY_FAMILIES {
                if fonts.collection.family_by_name(name).is_some() {
                    return (
                        FontFamily::Single(FontFamilyName::Named(std::borrow::Cow::Owned(
                            (*name).to_string(),
                        ))),
                        (*name).to_string(),
                    );
                }
            }
            (
                FontFamily::from(GenericFamily::Monospace),
                "monospace".to_string(),
            )
        });
        tracing::info!(family = %family_name, "terminal primary font");

        let mut fonts = Self {
            collection,
            layout_cx: LayoutContext::new(),
            family,
            size_px: size_pt,
            metrics: CellMetrics::fallback(size_pt),
        };
        fonts.metrics = fonts.probe_metrics();
        fonts
    }

    /// Re-measure after a font-size change.
    pub fn resize(&mut self, size_pt: f32) {
        self.size_px = size_pt;
        self.metrics = self.probe_metrics();
    }

    /// Shape `text` as one terminal line: single line, left aligned, with the
    /// cell's style applied as the default run style. Fontique splits the
    /// result into one run per face the text actually needs, which is where
    /// the terminal's font fallback now comes from.
    pub fn shape_run(&mut self, text: &str, bold: bool, italic: bool) -> Layout<[u8; 4]> {
        let size = self.size_px;
        let family = self.family.clone();
        self.collection.use_fonts(|fonts| {
            let mut builder = self
                .layout_cx
                .ranged_builder(fonts, text, 1.0, false);
            builder.push_default(StyleProperty::Brush([255, 255, 255, 255]));
            builder.push_default(StyleProperty::FontSize(size));
            builder.push_default(StyleProperty::FontFamily(family));
            builder.push_default(StyleProperty::FontWeight(FontWeight::new(
                if bold { 700.0 } else { 400.0 },
            )));
            builder.push_default(StyleProperty::FontStyle(if italic {
                FontStyle::Italic
            } else {
                FontStyle::Normal
            }));
            let mut layout = builder.build(text);
            layout.break_all_lines(None);
            layout.align(Alignment::Start, AlignmentOptions::default());
            layout
        })
    }

    /// Measure the cell grid off a probe layout: digit advance for the cell
    /// width, typographic line height for the cell height.
    fn probe_metrics(&mut self) -> CellMetrics {
        const PROBE: &str = "0000000000";
        let layout = self.shape_run(PROBE, false, false);
        let mut metrics = CellMetrics::fallback(self.size_px);
        if let Some(line) = layout.lines().next() {
            let m = line.metrics();
            if m.advance > 0.0 {
                metrics.cell_w = (m.advance / PROBE.len() as f32).ceil().max(1.0);
            }
            let height = m.ascent + m.descent + m.leading.max(0.0);
            if height > 0.0 {
                metrics.cell_h = height.ceil().max(1.0);
                metrics.baseline = m.baseline.max(0.0);
                metrics.strikeout_pos = (m.ascent * 0.32).round();
            }
        }
        metrics.finish();
        metrics
    }
}

impl CellMetrics {
    /// Geometry fallback before the first probe layout completes.
    fn fallback(size_px: f32) -> Self {
        Self {
            cell_w: (size_px * 0.6).ceil().max(1.0),
            cell_h: (size_px * 1.25).ceil().max(1.0),
            baseline: (size_px * 0.85).round(),
            underline_pos: (size_px / 11.0).max(1.0).round(),
            stroke: (size_px / 18.0).max(1.0),
            strikeout_pos: (size_px * 0.35).round(),
            size_px,
        }
    }

    /// Recompute the heuristic decoration geometry once ascent/descent are
    /// known (called after the probe fills the real values in).
    fn finish(&mut self) {
        self.underline_pos = (self.size_px / 11.0).max(1.0).round();
        self.stroke = (self.size_px / 18.0).max(1.0);
    }
}

/// Faux-bold / faux-italic flags for one shaped run, straight from the
/// synthesis fontique computed for it.
pub struct RunStyle {
    /// Draw the run a second time offset right (no designed bold face).
    pub embolden: bool,
    /// Shear to apply for a faux oblique, in degrees (no italic face).
    pub skew: Option<f32>,
}

impl From<Synthesis> for RunStyle {
    fn from(s: Synthesis) -> Self {
        Self {
            embolden: s.embolden(),
            skew: s.skew(),
        }
    }
}
