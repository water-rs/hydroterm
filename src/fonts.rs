//! Terminal typography on the host's shared parley stack: cell metrics from a
//! probe layout, per-run shaping with fontique's automatic fallback, and the
//! faux-bold / faux-italic synthesis parley reports back per run.
//!
//! The family's `parley::FontContext` is the host's own — installed into the
//! environment at startup and shared through `FontCollection` — so a pane
//! never enumerates system fonts for itself, and the shaped glyph runs hand
//! back `peniko::FontData` the scene's `draw_glyph_run` consumes directly.

use parley::fontique::Synthesis;
use parley::style::{FontFeature, FontFeatures};
use parley::setting::Tag;
use parley::{Alignment, AlignmentOptions, FontFamily, FontFamilyName, FontStyle,
             FontWeight, Layout, LayoutContext, StyleProperty, style::GenericFamily};
use waterui_text::FontCollection;

/// Parse one `font-feature` entry: `-tag` disables, `+tag`/`tag`/`tag=N`
/// sets the value; tags are 4-byte OpenType feature tags.
pub fn parse_font_feature(spec: &str) -> Option<FontFeature> {
    let spec = spec.trim();
    let (value, body) = if let Some(rest) = spec.strip_prefix('-') {
        (0, rest)
    } else {
        (1, spec.strip_prefix('+').unwrap_or(spec))
    };
    let (tag_s, value) = match body.split_once('=') {
        Some((t, v)) => (t.trim(), v.trim().parse::<u16>().ok()?),
        None => (body.trim(), value),
    };
    Some(FontFeature::new(Tag::parse(tag_s)?, value))
}

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
    /// `font-family-bold` / `font-family-italic` /
    /// `font-family-bold-italic` per-style overrides (Ghostty);
    /// `None` falls back to `family`.
    family_bold: Option<FontFamily<'static>>,
    family_italic: Option<FontFamily<'static>>,
    family_bold_italic: Option<FontFamily<'static>>,
    size_px: f32,
    /// `adjust-cell-width`/`adjust-cell-height` — spacing added to the
    /// measured cell (fraction of the cell or absolute points).
    cell_adjust: (crate::config::CellAdjust, crate::config::CellAdjust),
    /// `adjust-font-baseline` — offset applied to the baseline measured
    /// from the cell bottom (positive moves text up).
    baseline_adjust: crate::config::CellAdjust,
    /// `font-feature` entries applied to every shaped run.
    features: Vec<FontFeature>,
    pub metrics: CellMetrics,
}

impl TermFonts {
    /// Resolve the primary family in `collection` and measure the cell.
    /// `pref` is the configured `font-family`: when it names an installed
    /// family it wins over the built-in preference list (a comma list of
    /// names works — the first installed one is taken). Generic aliases
    /// like `monospace`/`serif` map to their generic family.
    pub fn load(collection: FontCollection, size_pt: f32, pref: &str) -> Self {
        let (family, family_name) = collection.use_fonts(|fonts| {
            let prefer = pref
                .split(',')
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .find_map(|name| {
                    let generic = match name.to_ascii_lowercase().as_str() {
                        "monospace" => Some(GenericFamily::Monospace),
                        "sans-serif" | "sans" => Some(GenericFamily::SansSerif),
                        "serif" => Some(GenericFamily::Serif),
                        "cursive" => Some(GenericFamily::Cursive),
                        "fantasy" => Some(GenericFamily::Fantasy),
                        "system-ui" | "ui" => Some(GenericFamily::SystemUi),
                        "emoji" => Some(GenericFamily::Emoji),
                        "math" => Some(GenericFamily::Math),
                        _ => None,
                    };
                    if let generic @ Some(_) = generic {
                        return generic.map(|g| {
                            (FontFamilyName::Generic(g), name.to_string())
                        });
                    }
                    fonts
                        .collection
                        .family_by_name(name)
                        .map(|_| (FontFamilyName::Named(std::borrow::Cow::Owned(name.to_string())), name.to_string()))
                });
            let (primary, family_name) = prefer.unwrap_or_else(|| {
                // Ordered stack: the preferred monospace first, then the
                // generic emoji family so clustered emoji (ZWJ sequences,
                // keycaps, flags) resolve to the color emoji font instead
                // of a monochrome symbols fallback.
                let name = PRIMARY_FAMILIES.iter().copied().find(|name| {
                    *name != "monospace" && fonts.collection.family_by_name(name).is_some()
                });
                match name {
                    Some(name) => (
                        FontFamilyName::Named(std::borrow::Cow::Owned(name.to_string())),
                        name.to_string(),
                    ),
                    None => (
                        FontFamilyName::Generic(GenericFamily::Monospace),
                        "monospace".to_string(),
                    ),
                }
            });
            let list: Vec<FontFamilyName> =
                vec![primary, FontFamilyName::Generic(GenericFamily::Emoji)];
            (
                FontFamily::List(std::borrow::Cow::Owned(list)),
                family_name,
            )
        });
        tracing::info!(family = %family_name, "terminal primary font");

        let mut fonts = Self {
            collection,
            layout_cx: LayoutContext::new(),
            family,
            family_bold: None,
            family_italic: None,
            family_bold_italic: None,
            size_px: size_pt,
            cell_adjust: (crate::config::CellAdjust::None, crate::config::CellAdjust::None),
            baseline_adjust: crate::config::CellAdjust::None,
            features: Vec::new(),
            metrics: CellMetrics::fallback(size_pt),
        };
        fonts.metrics = fonts.probe_metrics();
        fonts
    }

    /// Set the per-style family overrides (Ghostty `font-family-bold`
    /// / `font-family-italic` / `font-family-bold-italic`). Each name is
    /// resolved like `font-family` (generic aliases allowed); an
    /// uninstalled or empty name clears the override, leaving the run on
    /// the primary family.
    pub fn set_style_families(
        &mut self,
        bold: &Option<String>,
        italic: &Option<String>,
        bold_italic: &Option<String>,
    ) {
        self.collection.use_fonts(|fonts| {
            let mut resolve = |pref: &Option<String>| {
                pref.as_deref().and_then(|pref| {
                    resolve_family_name(fonts, pref).map(|name| {
                        FontFamily::List(std::borrow::Cow::Owned(vec![
                            name,
                            FontFamilyName::Generic(GenericFamily::Emoji),
                        ]))
                    })
                })
            };
            self.family_bold = resolve(bold);
            self.family_italic = resolve(italic);
            self.family_bold_italic = resolve(bold_italic);
        });
    }

    /// Re-measure after a font-size change.
    pub fn resize(&mut self, size_pt: f32) {
        self.size_px = size_pt;
        self.metrics = self.probe_metrics();
    }

    /// Re-resolve the primary family after `font-family` changed (hot
    /// reload): re-run the preference cascade and re-measure the cell.
    pub fn reload_family(&mut self, pref: &str) {
        let fresh = Self::load(self.collection.clone(), self.size_px, pref);
        self.family = fresh.family;
        self.metrics = self.probe_metrics();
    }

    /// Hot-reload `adjust-cell-width`/`adjust-cell-height` and re-measure.
    pub fn set_cell_adjust(
        &mut self,
        w: crate::config::CellAdjust,
        h: crate::config::CellAdjust,
    ) {
        if self.cell_adjust != (w, h) {
            self.cell_adjust = (w, h);
            self.metrics = self.probe_metrics();
        }
    }

    /// Hot-reload `adjust-font-baseline` and re-measure.
    pub fn set_baseline_adjust(&mut self, a: crate::config::CellAdjust) {
        if self.baseline_adjust != a {
            self.baseline_adjust = a;
            self.metrics = self.probe_metrics();
        }
    }

    /// Hot-reload `font-feature` entries and re-measure (a feature can
    /// change advances — e.g. `-calt` rejoins ligatures into cells).
    pub fn set_features(&mut self, specs: &[String]) {
        let parsed: Vec<FontFeature> =
            specs.iter().filter_map(|s| parse_font_feature(s)).collect();
        if self.features != parsed {
            self.features = parsed;
            self.metrics = self.probe_metrics();
        }
    }

    /// Shape `text` as one terminal line: single line, left aligned, with the
    /// cell's style applied as the default run style. Fontique splits the
    /// result into one run per face the text actually needs, which is where
    /// the terminal's font fallback now comes from.
    pub fn shape_run(&mut self, text: &str, bold: bool, italic: bool) -> Layout<[u8; 4]> {
        let size = self.size_px;
        // `font-family-bold-italic` wins, then the per-style override,
        // then the primary family (Ghostty precedence).
        let family = match (bold, italic) {
            (true, true) => self
                .family_bold_italic
                .as_ref()
                .or(self.family_bold.as_ref())
                .or(self.family_italic.as_ref())
                .unwrap_or(&self.family),
            (true, false) => self.family_bold.as_ref().unwrap_or(&self.family),
            (false, true) => self.family_italic.as_ref().unwrap_or(&self.family),
            (false, false) => &self.family,
        }
        .clone();
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
            if !self.features.is_empty() {
                builder.push_default(StyleProperty::FontFeatures(
                    FontFeatures::List(std::borrow::Cow::Owned(self.features.clone())),
                ));
            }
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
        let (aw, ah) = self.cell_adjust;
        let dw = aw.apply(metrics.cell_w) - metrics.cell_w;
        let dh = ah.apply(metrics.cell_h) - metrics.cell_h;
        metrics.cell_w = (metrics.cell_w + dw).max(1.0);
        metrics.cell_h = (metrics.cell_h + dh).max(1.0);
        // Extra height centres the glyph in the taller cell.
        metrics.baseline += dh / 2.0;
        // `adjust-font-baseline`: Ghostty measures the baseline as the
        // distance up from the cell bottom — positive moves text up.
        let dist = (metrics.cell_h - metrics.baseline).max(0.0);
        metrics.baseline = (metrics.cell_h - self.baseline_adjust.apply(dist)).max(0.0);
        metrics.finish();
        metrics
    }
}

/// Resolve one family name — generic aliases map to their generic
/// family, anything else must name an installed family.
fn resolve_family_name(
    fonts: &mut parley::FontContext,
    name: &str,
) -> Option<FontFamilyName<'static>> {
    let generic = match name.to_ascii_lowercase().as_str() {
        "monospace" => Some(GenericFamily::Monospace),
        "sans-serif" | "sans" => Some(GenericFamily::SansSerif),
        "serif" => Some(GenericFamily::Serif),
        "cursive" => Some(GenericFamily::Cursive),
        "fantasy" => Some(GenericFamily::Fantasy),
        "system-ui" | "ui" => Some(GenericFamily::SystemUi),
        "emoji" => Some(GenericFamily::Emoji),
        "math" => Some(GenericFamily::Math),
        _ => None,
    };
    if let Some(g) = generic {
        return Some(FontFamilyName::Generic(g));
    }
    fonts
        .collection
        .family_by_name(name)
        .map(|_| FontFamilyName::Named(std::borrow::Cow::Owned(name.to_string())))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The resolved family is a stack ending in the generic emoji family, so
    /// clustered emoji (ZWJ sequences, flags, keycaps) prefer the color
    /// emoji font over a monochrome symbols fallback.
    #[test]
    fn family_stack_prefers_color_emoji() {
        let fonts = TermFonts::load(FontCollection::new(parley::FontContext::new()), 13.0, "");
        let FontFamily::List(list) = &fonts.family else {
            panic!("family is not a fallback stack");
        };
        assert!(
            list.iter()
                .any(|f| matches!(f, FontFamilyName::Generic(GenericFamily::Emoji))),
            "emoji generic missing from fallback stack: {list:?}"
        );
    }

    /// A ZWJ emoji cluster shapes into a ligature: at least one visual
    /// cluster carries glyph(s) while its neighbors share the run — i.e.
    /// fontique resolved the whole sequence with one face, not split into
    /// per-scalar fallbacks.
    #[test]
    fn zwj_cluster_shapes_as_one_ligature() {
        let mut fonts =
            TermFonts::load(FontCollection::new(parley::FontContext::new()), 13.0, "");
        let layout = fonts.shape_run(
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}",
            false,
            false,
        );
        let mut clusters = 0usize;
        let mut glyph_clusters = 0usize;
        // Identify the font the cluster actually resolved to: FontData
        // carries the font file's raw bytes, so compare with the font files
        // on this system to name the resolved face.
        const NOTO_COLOR: &str = "/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf";
        let mut resolved_noto = false;
        for line in layout.lines() {
            for item in line.items() {
                let parley::PositionedLayoutItem::GlyphRun(gr) = item else {
                    continue;
                };
                for cluster in gr.run().visual_clusters() {
                    clusters += 1;
                    if cluster.glyphs().next().is_some() {
                        glyph_clusters += 1;
                    }
                }
                let run_font = gr.run().font().data.clone();
                if let Ok(noto) = std::fs::read(NOTO_COLOR)
                    && run_font.as_ref() == noto.as_slice()
                {
                    resolved_noto = true;
                }
            }
        }
        assert!(glyph_clusters >= 1, "no glyphs shaped for the ZWJ cluster");
        assert!(clusters >= glyph_clusters);
        if std::path::Path::new(NOTO_COLOR).exists() {
            assert!(
                resolved_noto,
                "family ZWJ cluster did not resolve to Noto Color Emoji"
            );
        }
    }
}
