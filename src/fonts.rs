//! Font loading, discovery, shaping, and the cell-metrics math a terminal
//! grid is built on.

use std::collections::HashMap;
use std::sync::Arc;

use peniko::{Blob, FontData};

/// One loaded font face: bytes kept alive for the process, a rustybuzz face
/// for shaping and metrics, and a peniko `FontData` the Vello scene draws.
pub struct Face {
    pub font: FontData,
    pub face: rustybuzz::Face<'static>,
}

impl Face {
    fn from_bytes(bytes: &'static [u8], index: u32) -> Option<Self> {
        let face = rustybuzz::Face::from_slice(bytes, index)?;
        Some(Self { font: FontData::new(Blob::new(Arc::new(bytes.to_vec())), index), face })
    }
}

/// Pixel metrics of the terminal cell grid, derived from the primary face at
/// the configured point size and the display scale.
#[derive(Debug, Clone, Copy)]
pub struct CellMetrics {
    /// Grid cell width in physical pixels.
    pub cell_w: f32,
    /// Grid cell height in physical pixels.
    pub cell_h: f32,
    /// Distance from a cell's top edge to the text baseline, in pixels.
    pub baseline: f32,
    /// Underline offset below the baseline, in pixels.
    pub underline_pos: f32,
    /// Underline / strikethrough stroke thickness, in pixels.
    pub stroke: f32,
    /// Strikethrough offset below the baseline, in pixels.
    pub strikeout_pos: f32,
    /// Physical pixels per logical unit.
    pub scale: f64,
    /// Em size in physical pixels (point size × scale).
    pub size_px: f32,
}

/// The loaded font family variants plus the coverage-driven fallback list.
pub struct FontStack {
    pub regular: Face,
    pub bold: Option<Face>,
    pub italic: Option<Face>,
    pub bold_italic: Option<Face>,
    /// Faces consulted in order when the styled variant lacks a glyph.
    pub fallbacks: Vec<Face>,
    /// Cache: char → index into `fallbacks`, for codepoints the primary faces
    /// could not cover. Negative answers are tracked by simply not caching;
    /// a miss is retried only when the same char shows up again.
    coverage_cache: HashMap<char, usize>,
    /// Raw fontdb face records kept for lazy fallback loading.
    db_faces: Vec<(fontdb::ID, fontdb::Source, u32)>,
    loaded_fallback: HashMap<fontdb::ID, usize>,
    pub metrics: CellMetrics,
}

/// Which family the app prefers, in order.
const PRIMARY_FAMILIES: &[&str] = &[
    "JetBrains Mono",
    "Fira Code",
    "Cascadia Code",
    "Source Code Pro",
    "DejaVu Sans Mono",
    "Liberation Mono",
    "monospace",
];

/// Curated broad-coverage families consulted before a linear scan of the
/// whole system database (which is ordered arbitrarily and often useless).
const FALLBACK_FAMILIES: &[&str] = &[
    "Noto Sans Mono",
    "Noto Sans",
    "DejaVu Sans",
    "FreeMono",
    "Noto Sans Symbols2",
    "Noto Sans Symbols",
    "Noto Color Emoji",
    "OpenMoji",
    "Noto Sans CJK SC",
    "Noto Sans CJK JP",
    "WenQuanYi Micro Hei",
];

fn face_source_bytes(source: &fontdb::Source) -> Option<Vec<u8>> {
    match source {
        fontdb::Source::File(path) => std::fs::read(path).ok(),
        fontdb::Source::SharedFile(path, data) => {
            // A shared file's bytes are already mapped; copy the whole file —
            // the Face is built against it with `index` picking the face.
            let _ = data;
            std::fs::read(path).ok()
        }
        fontdb::Source::Binary(data) => Some(data.as_ref().as_ref().to_vec()),
    }
}

fn load_face(db: &fontdb::Database, id: fontdb::ID) -> Option<Face> {
    let info = db.face(id)?;
    let bytes = face_source_bytes(&info.source)?;
    Face::from_bytes(Box::leak(bytes.into_boxed_slice()), info.index)
}

fn query_family(
    db: &fontdb::Database,
    family: &str,
    weight: fontdb::Weight,
    style: fontdb::Style,
) -> Option<fontdb::ID> {
    db.query(&fontdb::Query {
        families: &[fontdb::Family::Name(family)],
        weight,
        style,
        ..fontdb::Query::default()
    })
}

impl FontStack {
    /// Load the primary monospace family and the curated fallback set.
    pub fn load(size_pt: f32, scale: f64) -> Self {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();

        let mut primary_id: Option<fontdb::ID> = None;
        for family in PRIMARY_FAMILIES {
            if let Some(id) = query_family(&db, family, fontdb::Weight::NORMAL, fontdb::Style::Normal)
            {
                primary_id = Some(id);
                break;
            }
        }
        let primary_id = primary_id.expect("no monospace font found on this system");
        let family_name = db
            .face(primary_id)
            .and_then(|f| f.families.first().map(|(name, _)| name.clone()))
            .unwrap_or_else(|| "monospace".to_owned());
        tracing::info!(family = %family_name, "terminal primary font");

        let regular = load_face(&db, primary_id).expect("primary font bytes unreadable");
        let bold = query_family(&db, &family_name, fontdb::Weight::BOLD, fontdb::Style::Normal)
            .and_then(|id| load_face(&db, id));
        let italic = query_family(&db, &family_name, fontdb::Weight::NORMAL, fontdb::Style::Italic)
            .and_then(|id| load_face(&db, id));
        let bold_italic =
            query_family(&db, &family_name, fontdb::Weight::BOLD, fontdb::Style::Italic)
                .and_then(|id| load_face(&db, id));

        // Lazily-loadable records for every face in the database, used by the
        // coverage cascade.
        let db_faces: Vec<(fontdb::ID, fontdb::Source, u32)> = db
            .faces()
            .map(|f| (f.id, f.source.clone(), f.index))
            .collect();

        // Eagerly load the curated fallback families — they cover the bulk of
        // what a terminal actually sees (box drawing, powerline, CJK, emoji).
        let mut fallbacks = Vec::new();
        let mut loaded_fallback = HashMap::new();
        for family in FALLBACK_FAMILIES {
            if let Some(id) =
                query_family(&db, family, fontdb::Weight::NORMAL, fontdb::Style::Normal)
            {
                if id == primary_id || loaded_fallback.contains_key(&id) {
                    continue;
                }
                if let Some(face) = load_face(&db, id) {
                    loaded_fallback.insert(id, fallbacks.len());
                    fallbacks.push(face);
                }
            }
        }

        let metrics = CellMetrics::compute(&regular.face, size_pt, scale);

        Self {
            regular,
            bold,
            italic,
            bold_italic,
            fallbacks,
            coverage_cache: HashMap::new(),
            db_faces,
            loaded_fallback,
            metrics,
        }
    }

    /// The styled variant for a cell's flags, if a designed face exists.
    pub fn variant(&self, bold: bool, italic: bool) -> &Face {
        match (bold, italic) {
            (true, true) => self.bold_italic.as_ref().or(self.bold.as_ref()).unwrap_or(&self.regular),
            (true, false) => self.bold.as_ref().unwrap_or(&self.regular),
            (false, true) => self.italic.as_ref().unwrap_or(&self.regular),
            (false, false) => &self.regular,
        }
    }

    /// True when the styled run should draw with a synthetic bold overdraw:
    /// the run wants bold but no designed bold face exists.
    pub fn needs_synthetic_bold(&self, bold: bool) -> bool {
        bold && self.bold.is_none()
    }

    /// True when the styled run should shear: italic wanted, no italic face.
    pub fn needs_synthetic_italic(&self, italic: bool) -> bool {
        italic && self.italic.is_none()
    }

    /// Does this face have a glyph for `ch`?
    pub fn face_covers(face: &rustybuzz::Face, ch: char) -> bool {
        face.glyph_index(ch).is_some_and(|gid| gid.0 != 0)
    }

    /// Index into `self.fallbacks` covering `ch`, loading new faces lazily.
    pub fn fallback_for(&mut self, ch: char) -> Option<usize> {
        if let Some(&idx) = self.coverage_cache.get(&ch) {
            return Some(idx);
        }
        for (idx, face) in self.fallbacks.iter().enumerate() {
            if Self::face_covers(&face.face, ch) {
                self.coverage_cache.insert(ch, idx);
                return Some(idx);
            }
        }
        // Scan the remaining database faces lazily.
        for &(id, ref source, index) in &self.db_faces.clone() {
            if self.loaded_fallback.contains_key(&id) {
                continue;
            }
            let Some(bytes) = face_source_bytes(source) else { continue };
            let Some(face) = Face::from_bytes(Box::leak(bytes.into_boxed_slice()), index)
            else {
                continue;
            };
            self.loaded_fallback.insert(id, self.fallbacks.len());
            self.fallbacks.push(face);
            if Self::face_covers(&self.fallbacks.last().unwrap().face, ch) {
                let idx = self.fallbacks.len() - 1;
                self.coverage_cache.insert(ch, idx);
                return Some(idx);
            }
        }
        None
    }
}

impl CellMetrics {
    /// Derive cell geometry from a face's vertical metrics and the pixel size.
    pub fn compute(face: &rustybuzz::Face, size_pt: f32, scale: f64) -> Self {
        let px = (size_pt as f64 * scale) as f32;
        let upem = face.units_per_em() as f32;
        let k = px / upem;
        let ascent = face.ascender() as f32 * k;
        let descent = (-face.descender()) as f32 * k;
        let leading = face.line_gap() as f32 * k;
        let cell_h = (ascent + descent + leading.max(0.0)).ceil().max(1.0);
        let baseline = (ascent + leading.max(0.0) * 0.5).round();

        let cell_w = face
            .glyph_index('0')
            .and_then(|gid| face.glyph_hor_advance(gid))
            .map(|adv| adv as f32 * k)
            .unwrap_or(px * 0.6)
            .ceil()
            .max(1.0);

        let stroke = (px / 18.0).max(1.0);
        Self {
            cell_w,
            cell_h,
            baseline,
            underline_pos: (px / 11.0).max(1.0).round(),
            stroke,
            strikeout_pos: (ascent * 0.32).round(),
            scale,
            size_px: px,
        }
    }
}

/// Shape `text` with `face` at `size_px`. Returns per glyph
/// `(glyph_id, cluster_byte_offset, x_offset, y_offset, x_advance, y_advance)`
/// with geometry scaled to pixels. Harfbuzz clusters are byte offsets into
/// `text`.
pub fn shape_span(
    face: &rustybuzz::Face,
    text: &str,
    size_px: f32,
) -> Vec<(u32, u32, f32, f32, f32, f32)> {
    let upem = face.units_per_em() as f32;
    let k = size_px / upem;
    let mut buffer = rustybuzz::UnicodeBuffer::new();
    buffer.push_str(text);
    let out = rustybuzz::shape(face, &[], buffer);
    let infos = out.glyph_infos();
    let positions = out.glyph_positions();
    let mut result = Vec::with_capacity(infos.len());
    for (info, pos) in infos.iter().zip(positions.iter()) {
        result.push((
            info.glyph_id,
            info.cluster,
            pos.x_offset as f32 * k,
            pos.y_offset as f32 * k,
            pos.x_advance as f32 * k,
            pos.y_advance as f32 * k,
        ));
    }
    result
}
