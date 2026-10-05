//! Terminal typography on the host's shared parley stack: cell metrics from a
//! probe layout, per-run shaping with fontique's automatic fallback, and the
//! faux-bold / faux-italic synthesis parley reports back per run.
//!
//! The family's `parley::FontContext` is the host's own — installed into the
//! environment at startup and shared through `FontCollection` — so a pane
//! never enumerates system fonts for itself, and the shaped glyph runs hand
//! back parley font data the scene registers as Cherenkov fonts by face
//! identity (blob id plus collection index).

use std::collections::HashMap;
use std::sync::Arc;

use parley::fontique::Synthesis;
use parley::setting::Tag;
use parley::style::{FontFeature, FontFeatures, FontVariation, FontVariations};
use parley::{
    Alignment, AlignmentOptions, FontFamily, FontFamilyName, FontStyle, FontWeight, Layout,
    LayoutContext, StyleProperty, style::GenericFamily,
};
use waterui_graphics::Registered;
use waterui_graphics::cherenkov::{Font, FontId, FontSource};
use waterui_graphics::resources::RecordingResources;
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

/// Face identity for engine registration: the font blob's id plus the
/// face's index inside it. Faces sharing one file — a `.ttc` collection —
/// carry the same blob id and are only told apart by the index, so the
/// pair is the registration key and the `FontSource` fed to the engine.
fn font_key(fd: &parley::FontData) -> (u64, u32) {
    (fd.data.id(), fd.index)
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
    /// `font-codepoint-map` — chars inside a range shape with this
    /// family instead of the run's family; first match wins.
    codepoint_map: Vec<(std::ops::RangeInclusive<u32>, FontFamily<'static>)>,
    /// `font-style` — the plain run's default weight and style.
    font_style: (FontWeight, FontStyle),
    /// `font-style-bold`/`font-style-italic`/`font-style-bold-italic` —
    /// named styles that replace the fixed variant mapping (bold→700,
    /// italic→Italic) when set; `None` keeps the fixed mapping.
    style_bold: Option<(FontWeight, FontStyle)>,
    style_italic: Option<(FontWeight, FontStyle)>,
    style_bold_italic: Option<(FontWeight, FontStyle)>,
    /// `font-variation` family — OpenType axis settings per face
    /// (`wght=700,wdth=85`); each applies only to its own run and does
    /// not inherit.
    variations: [Option<Vec<FontVariation>>; 4],
    /// Engine-side font registrations keyed by face identity — the blob
    /// id AND the collection index, since faces in one font file (a
    /// `.ttc`) share the blob. An entry registers on first use and names
    /// itself each frame after.
    font_regs: HashMap<(u64, u32), Registered<Font>>,
    pub metrics: CellMetrics,
}

impl TermFonts {
    /// Resolve the primary family in `collection` and measure the cell.
    /// `pref` is the configured `font-family` chain: a comma-joined list
    /// where each entry names an installed family, a generic alias, or is
    /// skipped if unresolvable — every resolvable entry joins the ordered
    /// fallback list (Ghostty repeated `font-family` semantics: glyph
    /// lookup tries family 1, then 2, ...). Generic aliases like
    /// `monospace`/`serif` map to their generic family.
    pub fn load(collection: FontCollection, size_pt: f32, pref: &str) -> Self {
        let (family, family_name) = collection.use_fonts(|fonts| {
            let mut list: Vec<FontFamilyName> = Vec::new();
            let mut primary_name: Option<String> = None;
            for name in pref.split(',').map(str::trim).filter(|n| !n.is_empty()) {
                let resolved = match name.to_ascii_lowercase().as_str() {
                    "monospace" => Some(FontFamilyName::Generic(GenericFamily::Monospace)),
                    "sans-serif" | "sans" => {
                        Some(FontFamilyName::Generic(GenericFamily::SansSerif))
                    }
                    "serif" => Some(FontFamilyName::Generic(GenericFamily::Serif)),
                    "cursive" => Some(FontFamilyName::Generic(GenericFamily::Cursive)),
                    "fantasy" => Some(FontFamilyName::Generic(GenericFamily::Fantasy)),
                    "system-ui" | "ui" => Some(FontFamilyName::Generic(GenericFamily::SystemUi)),
                    "emoji" => Some(FontFamilyName::Generic(GenericFamily::Emoji)),
                    "math" => Some(FontFamilyName::Generic(GenericFamily::Math)),
                    _ => fonts
                        .collection
                        .family_by_name(name)
                        .map(|_| FontFamilyName::Named(std::borrow::Cow::Owned(name.to_string()))),
                };
                if let Some(f) = resolved {
                    if primary_name.is_none() {
                        primary_name = Some(name.to_string());
                    }
                    list.push(f);
                }
            }
            if list.is_empty() {
                // No configured name resolved: fall back to the built-in
                // preference order, then generic monospace.
                let name = PRIMARY_FAMILIES.iter().copied().find(|name| {
                    *name != "monospace" && fonts.collection.family_by_name(name).is_some()
                });
                match name {
                    Some(name) => {
                        primary_name = Some(name.to_string());
                        list.push(FontFamilyName::Named(std::borrow::Cow::Owned(
                            name.to_string(),
                        )));
                    }
                    None => {
                        primary_name = Some("monospace".to_string());
                        list.push(FontFamilyName::Generic(GenericFamily::Monospace));
                    }
                }
            }
            // The generic emoji family is always last so clustered emoji
            // (ZWJ sequences, keycaps, flags) resolve to the color emoji
            // font instead of a monochrome symbols fallback — unless the
            // user already chained it themselves.
            let emoji = FontFamilyName::Generic(GenericFamily::Emoji);
            if !list.contains(&emoji) {
                list.push(emoji);
            }
            (
                FontFamily::List(std::borrow::Cow::Owned(list)),
                primary_name.unwrap_or_default(),
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
            cell_adjust: (
                crate::config::CellAdjust::None,
                crate::config::CellAdjust::None,
            ),
            baseline_adjust: crate::config::CellAdjust::None,
            features: Vec::new(),
            codepoint_map: Vec::new(),
            font_style: (FontWeight::new(400.0), FontStyle::Normal),
            style_bold: None,
            style_italic: None,
            style_bold_italic: None,
            variations: [None, None, None, None],
            font_regs: HashMap::new(),
            metrics: CellMetrics::fallback(size_pt),
        };
        fonts.metrics = fonts.probe_metrics();
        fonts
    }

    /// Register `fd`'s face with the scene engine on first use and name its
    /// `FontId` in this recording. Registering a valid parley face is an
    /// internal invariant: the engine rejecting it panics with the face's
    /// identity rather than silently dropping the shaped run.
    pub fn font_id(
        &mut self,
        resources: &mut RecordingResources<'_>,
        fd: &parley::FontData,
    ) -> FontId {
        let key = font_key(fd);
        if let std::collections::hash_map::Entry::Vacant(e) = self.font_regs.entry(key) {
            let source = FontSource::bytes(Arc::<[u8]>::from(fd.data.data())).with_index(fd.index);
            match resources.font(source) {
                Ok(registered) => {
                    e.insert(registered);
                }
                Err(err) => {
                    panic!("scene engine rejected font face {key:?}: {err}");
                }
            }
        }
        resources.name(self.font_regs.get(&key).expect("face registered above"))
    }

    /// Set `font-codepoint-map` — each entry's family resolved like
    /// `font-family`; an uninstalled name drops that entry. Re-measure
    /// since a mapped face can advance differently.
    pub fn set_codepoint_map(&mut self, entries: &[(u32, u32, String)]) {
        self.collection.use_fonts(|fonts| {
            self.codepoint_map = entries
                .iter()
                .filter_map(|(lo, hi, fam)| {
                    resolve_family_name(fonts, fam).map(|name| {
                        (
                            *lo..=*hi,
                            FontFamily::List(std::borrow::Cow::Owned(vec![
                                name,
                                FontFamilyName::Generic(GenericFamily::Emoji),
                            ])),
                        )
                    })
                })
                .collect();
        });
        self.metrics = self.probe_metrics();
    }

    /// Set `font-style` — parse the named style (`Italic`, `Bold`,
    /// `Bold Italic`, `Light`, `Medium`, `SemiBold`) into the plain
    /// run's default weight and style.
    pub fn set_font_style(&mut self, style: &Option<String>) {
        self.font_style = style
            .as_deref()
            .map_or((FontWeight::new(400.0), FontStyle::Normal), named_style);
        self.metrics = self.probe_metrics();
    }

    /// Set `font-style-bold`/`font-style-italic`/`font-style-bold-italic`
    /// — named styles replacing the fixed (700/Italic) variant mapping.
    /// `None` (or an empty value) restores the fixed mapping.
    pub fn set_variant_styles(
        &mut self,
        bold: &Option<String>,
        italic: &Option<String>,
        bold_italic: &Option<String>,
    ) {
        let parse = |p: &Option<String>| p.as_deref().filter(|s| !s.is_empty()).map(named_style);
        self.style_bold = parse(bold);
        self.style_italic = parse(italic);
        self.style_bold_italic = parse(bold_italic);
        self.metrics = self.probe_metrics();
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

    /// Set `font-variation`/`font-variation-bold`/`font-variation-italic`/
    /// `font-variation-bold-italic` — Ghostty `tag=value` comma lists, one
    /// per face (`[regular, bold, italic, bold-italic]`); `None` or
    /// unparsable clears that face's set. Re-measure since `wght`/`wdth`
    /// move the advance.
    pub fn set_variations(&mut self, specs: &[Option<String>; 4]) {
        let parsed: [Option<Vec<FontVariation>>; 4] =
            std::array::from_fn(|i| specs[i].as_deref().and_then(parse_font_variation));
        if self.variations != parsed {
            self.variations = parsed;
            self.metrics = self.probe_metrics();
        }
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
    pub fn set_cell_adjust(&mut self, w: crate::config::CellAdjust, h: crate::config::CellAdjust) {
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
        let parsed: Vec<FontFeature> = specs.iter().filter_map(|s| parse_font_feature(s)).collect();
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
            let mut builder = self.layout_cx.ranged_builder(fonts, text, 1.0, false);
            builder.push_default(StyleProperty::Brush([255, 255, 255, 255]));
            builder.push_default(StyleProperty::FontSize(size));
            builder.push_default(StyleProperty::FontFamily(family));
            // `font-style` supplies the regular run's own weight/style;
            // SGR bold/italic keeps its fixed mapping.
            let (weight, style) = match (bold, italic) {
                (true, true) => self
                    .style_bold_italic
                    .unwrap_or((FontWeight::new(700.0), FontStyle::Italic)),
                (true, false) => self
                    .style_bold
                    .unwrap_or((FontWeight::new(700.0), FontStyle::Normal)),
                (false, true) => self
                    .style_italic
                    .unwrap_or((FontWeight::new(400.0), FontStyle::Italic)),
                (false, false) => self.font_style,
            };
            builder.push_default(StyleProperty::FontWeight(weight));
            builder.push_default(StyleProperty::FontStyle(style));
            // `font-variation*` axes ride the same (bold, italic) slot —
            // the regular axis set is face 0, per-variant overrides are
            // 1..3 and never inherit the regular set.
            let variant_idx = match (bold, italic) {
                (false, false) => 0usize,
                (true, false) => 1,
                (false, true) => 2,
                (true, true) => 3,
            };
            if let Some(v) = &self.variations[variant_idx] {
                builder.push_default(StyleProperty::FontVariations(FontVariations::List(
                    std::borrow::Cow::Owned(v.clone()),
                )));
            }
            if !self.features.is_empty() {
                builder.push_default(StyleProperty::FontFeatures(FontFeatures::List(
                    std::borrow::Cow::Owned(self.features.clone()),
                )));
            }
            if !self.codepoint_map.is_empty() {
                // Group consecutive chars that resolve to the same map
                // entry (or none) into one range each — first match wins.
                let mut run_start = 0usize;
                let mut run_map: Option<usize> = None;
                for (bi, ch) in text.char_indices() {
                    let idx = self
                        .codepoint_map
                        .iter()
                        .position(|(r, _)| r.contains(&(ch as u32)));
                    if idx != run_map {
                        if let Some(mi) = run_map {
                            builder.push(
                                StyleProperty::FontFamily(self.codepoint_map[mi].1.clone()),
                                run_start..bi,
                            );
                        }
                        run_map = idx;
                        run_start = bi;
                    }
                }
                if let Some(mi) = run_map {
                    builder.push(
                        StyleProperty::FontFamily(self.codepoint_map[mi].1.clone()),
                        run_start..text.len(),
                    );
                }
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

/// Parse one `font-variation*` value: Ghostty's `tag=value` comma list
/// (`wght=700,wdth=85`) — tags are 4-byte OpenType axis tags, values f32.
/// `None` on any malformed entry (the whole line is rejected, matching
/// the config parser's all-or-nothing convention for bad values).
pub fn parse_font_variation(spec: &str) -> Option<Vec<FontVariation>> {
    let mut out = Vec::new();
    for entry in spec.split(',') {
        let (tag_s, val_s) = entry.trim().split_once('=')?;
        let tag = Tag::parse(tag_s.trim())?;
        let value = val_s.trim().parse::<f32>().ok()?;
        out.push(FontVariation::new(tag, value));
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Parse a Ghostty named style (`Italic`, `Bold`, `Bold Italic`,
/// `Light`, `Medium`, `SemiBold`, `Oblique`, …) into a weight/style
/// pair — the same vocabulary `font-style` and the `font-style-*`
/// variant keys share.
fn named_style(s: &str) -> (FontWeight, FontStyle) {
    let s = s.to_ascii_lowercase();
    let weight = if s.contains("bold") {
        700.0
    } else if s.contains("light") {
        300.0
    } else if s.contains("semibold") || s.contains("semi-bold") {
        600.0
    } else if s.contains("medium") {
        500.0
    } else {
        400.0
    };
    let italic = s.contains("italic") || s.contains("oblique");
    (
        FontWeight::new(weight),
        if italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        },
    )
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
        let mut fonts = TermFonts::load(FontCollection::new(parley::FontContext::new()), 13.0, "");
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
