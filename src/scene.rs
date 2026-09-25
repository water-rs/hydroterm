//! Turn the terminal grid into Scene2D commands: background runs, glyph runs
//! with font fallback + ligature shaping, decorations (underline variants,
//! strikethrough), the selection overlay, the cursor, and the scrollbar.

use alacritty_terminal::index::Point;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{CursorShape, NamedColor, Rgb};
use kurbo::{Affine, BezPath, Rect, Shape, Stroke};
use peniko::{Brush, Color, Fill, StyleRef};
use waterui_graphics::scene2d::{Glyph, GlyphRun, Scene2D};

use crate::fonts::TermFonts;
use crate::palette::{Palette, peniko, peniko_alpha};
use crate::terminal::EventProxy;

/// Padding around the text area, in logical units (scaled to px at draw).
pub const PADDING: f32 = 6.0;

/// How far the viewport is scrolled back, for the scrollbar.
#[derive(Debug, Clone, Copy)]
pub struct ScrollInfo {
    /// Lines of scrollback currently above the viewport.
    pub display_offset: usize,
    /// Total scrollback lines available.
    pub history_size: usize,
    /// Visible rows.
    pub screen_lines: usize,
}

/// One numbered URL-hint chip over a link span (URL hint mode).
#[derive(Clone)]
pub struct HintSpan {
    /// `(start col, end col exclusive, viewport row)` per visible row
    /// part — a link that crosses a soft wrap has one per covered row.
    /// The badge anchors on the first visible segment.
    pub segments: Vec<(usize, usize, usize)>,
    /// 1-based hint number the user types to open this link.
    pub label: usize,
}

/// Runtime state the scene pass needs beyond the term's renderable content.
pub struct DrawContext<'a> {
    pub palette: &'a Palette,
    pub fonts: &'a mut TermFonts,
    /// Frame size in logical units.
    pub width: f32,
    pub height: f32,
    /// Cursor blink phase — when false a blinking cursor is not drawn.
    pub blink_on: bool,
    /// Whether the surface has keyboard focus (hollow block vs. solid).
    pub focused: bool,
    /// `cursor-invert-fg-bg` — swap cell fg onto the block cursor.
    pub cursor_invert_fg_bg: bool,
    /// `cursor-text` — glyph color under the block cursor (None = the
    /// inverted cell fg).
    pub cursor_text: Option<Rgb>,
    /// `cursor-opacity` — alpha of the block cursor fill over the cell.
    pub cursor_opacity: f32,
    /// IME preedit text shown at the caret, if any: (text, caret byte offset).
    pub preedit: Option<(String, usize)>,
    /// Scrollback overlay data.
    pub scroll: ScrollInfo,
    /// Search match cells to highlight (col,row in viewport coords).
    /// (start col, end col exclusive, row) per match.
    pub search_matches: &'a [(usize, usize, usize)],
    /// Segments of the currently active search match (a wrap-crossing
    /// match highlights its rows on both sides).
    pub search_active: &'a [(usize, usize, usize)],
    /// Alpha for the bell flash overlay.
    pub bell_flash: f32,
    /// Alpha of the default background fill (window transparency; 1.0 =
    /// opaque). Cells with explicit (non-default) backgrounds stay opaque.
    pub bg_opacity: f32,
    /// URL hint chips over link spans (empty = hint mode off).
    pub hints: &'a [HintSpan],
    /// Digits typed so far in hint mode — shown as a status chip.
    pub hint_digits: &'a str,
    /// `bold-color` config — bold-cell color override (bright slot,
    /// a fixed color, or unset).
    pub bold_color: crate::config::BoldColor,
    /// `faint-opacity` — opacity of faint (SGR 2) text (1.0 = opaque).
    pub faint_opacity: f32,
    /// `minimum-contrast` — WCAG ratio floor applied to each cell's
    /// resolved fg against its bg (1.0 = off).
    pub min_contrast: f32,
    /// `selection-invert-fg-bg` — selected cells swap their fg/bg
    /// instead of using `selection_bg`/`selection_fg`.
    pub selection_invert: bool,
    /// Ctrl-hovered link span to underline: `(start col, end col
    /// exclusive, viewport row)` per visible part (wrap-safe, like hints).
    pub hover_link: &'a [(usize, usize, usize)],
    /// `background-image` brush + image-space→surface transform, drawn
    /// between the theme base fill and the cell backgrounds so unstyled
    /// cells show the image (Ghostty `background-image`).
    pub bg_image: Option<(peniko::ImageBrush, kurbo::Affine)>,
    /// `unfocused-split-fill` — replaces the pane's default background
    /// while it is unfocused (cells with explicit backgrounds keep them).
    pub unfocused_fill: Option<Rgb>,
    /// Cursor thickness multiplier (Ghostty `adjust-cursor-thickness`);
    /// 1.0 = the default beam/underline size.
    pub cursor_thickness: f32,
    /// Points added to the font's underline offset and a stroke
    /// multiplier (Ghostty `adjust-underline-position` / `-thickness`).
    pub underline_adjust: (f32, f32),
    /// Same pair for strikethrough (Ghostty
    /// `adjust-strikethrough-position` / `-thickness`).
    pub strikethrough_adjust: (f32, f32),
    /// Grid origin within the frame — `PADDING`, or `PADDING` plus half
    /// the leftover space when `window-padding-balance` centers the grid.
    pub pad_x: f32,
    pub pad_y: f32,
    /// `font-thicken` — every glyph run is emboldened (Ghostty
    /// `font-thicken`).
    pub font_thicken: bool,
}

// ---------------------------------------------------------------------------
// Harvest: walk the display grid once, resolving colors/flags into owned data
// ---------------------------------------------------------------------------

const DECO_UNDERLINE: u8 = 1;
const DECO_DOUBLE: u8 = 2;
const DECO_CURL: u8 = 4;
const DECO_DOTTED: u8 = 8;
const DECO_DASHED: u8 = 16;
const DECO_STRIKE: u8 = 32;

fn deco_bits(cell: &Cell) -> u8 {
    let f = cell.flags;
    let mut d = 0u8;
    if f.contains(Flags::UNDERCURL) {
        d |= DECO_CURL;
    } else if f.contains(Flags::DOUBLE_UNDERLINE) {
        d |= DECO_DOUBLE;
    } else if f.contains(Flags::DOTTED_UNDERLINE) {
        d |= DECO_DOTTED;
    } else if f.contains(Flags::DASHED_UNDERLINE) {
        d |= DECO_DASHED;
    } else if f.contains(Flags::UNDERLINE) {
        d |= DECO_UNDERLINE;
    }
    if f.contains(Flags::STRIKEOUT) {
        d |= DECO_STRIKE;
    }
    if cell.hyperlink().is_some() {
        d |= DECO_UNDERLINE;
    }
    d
}

/// The per-cell style key that decides run boundaries.
#[derive(Clone, Copy, PartialEq)]
struct StyleKey {
    fg: Rgb,
    bg: Rgb,
    bold: bool,
    italic: bool,
    deco: u8,
    /// SGR 58 underline-color override.
    ul_color: Option<Rgb>,
}

/// One harvested cell — owned data, no borrow on the term.
struct CellData {
    text: String,
    style: StyleKey,
}

pub(crate) fn cell_text(cell: &Cell) -> String {
    let mut s = String::with_capacity(8);
    if !cell
        .flags
        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
    {
        s.push(cell.c);
        for ch in cell.zerowidth().unwrap_or(&[]) {
            s.push(*ch);
        }
    }
    s
}

fn lerp_rgb(from: Rgb, to: Rgb, t: f32) -> Rgb {
    let m = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
    Rgb {
        r: m(from.r, to.r),
        g: m(from.g, to.g),
        b: m(from.b, to.b),
    }
}

/// Is grid point `p` inside the selection?
fn in_selection(p: Point, sel: &alacritty_terminal::selection::SelectionRange) -> bool {
    if sel.is_block {
        p.line >= sel.start.line
            && p.line <= sel.end.line
            && p.column >= sel.start.column
            && p.column <= sel.end.column
    } else {
        (p.line > sel.start.line
            || (p.line == sel.start.line && p.column >= sel.start.column))
            && (p.line < sel.end.line
                || (p.line == sel.end.line && p.column <= sel.end.column))
    }
}

/// Cursor data pulled out of the render pass for the caller.
#[derive(Debug, Clone, Copy)]
pub struct CursorInfo {
    pub shape: CursorShape,
    /// Viewport coords (row, col).
    pub row: i32,
    pub col: usize,
    pub blinking: bool,
}

/// Cursor position/shape in viewport coords — for the IME caret reply.
pub fn cursor_info(term: &Term<EventProxy>) -> CursorInfo {
    let content = term.renderable_content();
    CursorInfo {
        shape: content.cursor.shape,
        row: content.cursor.point.line.0 + content.display_offset as i32,
        col: content.cursor.point.column.0,
        blinking: term.cursor_style().blinking,
    }
}

/// What the harvest produced.
struct Grid {
    /// Per visible row: cells left→right (each entry = one grid column).
    rows: Vec<Vec<CellData>>,
    num_cols: usize,
}

fn harvest(term: &Term<EventProxy>, ctx: &DrawContext<'_>) -> (Grid, CursorInfo) {
    let content = term.renderable_content();
    let palette = ctx.palette;
    let colors = content.colors;

    let cursor = content.cursor;
    let cursor_style = term.cursor_style();
    let show_block = matches!(cursor.shape, CursorShape::Block)
        && ctx.focused
        && (ctx.blink_on || !cursor_style.blinking);
    let cursor_info = CursorInfo {
        shape: cursor.shape,
        row: cursor.point.line.0 + content.display_offset as i32,
        col: cursor.point.column.0,
        blinking: cursor_style.blinking,
    };

    let mut grid = Grid { rows: Vec::new(), num_cols: 0 };

    for indexed in content.display_iter {
        let p = indexed.point;
        let cell = indexed.cell;
        let row_i = p.line.0 + content.display_offset as i32;
        let row_idx = row_i as usize;

        if grid.rows.len() <= row_idx {
            grid.rows.resize_with(row_idx + 1, Vec::new);
        }
        grid.num_cols = grid.num_cols.max(p.column.0 + 1);
        let row = &mut grid.rows[row_idx];

        let is_sel = content.selection.as_ref().is_some_and(|s| in_selection(p, s));
        let is_cursor =
            show_block && p.line == cursor.point.line && p.column == cursor.point.column;

        let pair = palette.resolve(
            colors,
            cell.fg,
            cell.bg,
            cell.flags,
            ctx.bold_color,
            ctx.min_contrast,
            ctx.faint_opacity,
        );
        let (mut fg, mut bg) = (pair.fg, pair.bg);
        if cell.flags.contains(Flags::HIDDEN) {
            fg = bg;
        }
        if is_sel {
            if ctx.selection_invert {
                std::mem::swap(&mut fg, &mut bg);
            } else {
                bg = palette.selection_bg;
                if let Some(f) = palette.selection_fg {
                    fg = f;
                }
            }
        }
        if is_cursor {
            // `cursor-text` wins, then `cursor-invert-fg-bg`'s swap;
            // neither = the glyph keeps its own fg under the block.
            if let Some(ct) = ctx.cursor_text {
                fg = ct;
            } else if ctx.cursor_invert_fg_bg {
                fg = bg;
            }
            let cc = palette.named(colors, NamedColor::Cursor);
            bg = if ctx.cursor_opacity < 1.0 {
                lerp_rgb(bg, cc, ctx.cursor_opacity)
            } else {
                cc
            };
        }

        row.push(CellData {
            text: cell_text(cell),
            style: StyleKey {
                fg,
                bg,
                bold: cell.flags.contains(Flags::BOLD),
                italic: cell.flags.contains(Flags::ITALIC),
                deco: deco_bits(cell),
                ul_color: cell.underline_color().map(|c| palette.lookup(colors, c)),
            },
        });
    }
    (grid, cursor_info)
}

// ---------------------------------------------------------------------------
// Draw: runs → fills / glyph runs / decorations / cursor / overlays
// ---------------------------------------------------------------------------

fn col_x(pad: f32, cw: f32, col: usize) -> f32 {
    pad + col as f32 * cw
}
fn row_y(pad: f32, ch: f32, row: usize) -> f32 {
    pad + row as f32 * ch
}

fn rect(x: f32, y: f32, w: f32, h: f32) -> BezPath {
    Rect::new(x as f64, y as f64, (x + w) as f64, (y + h) as f64).to_path(0.0)
}

/// Segment a row into same-style runs.
fn style_runs(row: &[CellData]) -> Vec<(usize, usize, StyleKey)> {
    let mut runs = Vec::new();
    let mut start = 0usize;
    for (i, cell) in row.iter().enumerate().skip(1) {
        if cell.style != row[start].style {
            runs.push((start, i, row[start].style));
            start = i;
        }
    }
    if !row.is_empty() {
        runs.push((start, row.len(), row[start].style));
    }
    runs
}

/// Draw one frame into `scene`. `term` stays locked by the caller.
#[allow(clippy::too_many_lines)]
/// Draw the terminal grid. `underlay`, when given, paints between the cell
/// backgrounds/highlights and the text layer — used for z<0 kitty images.
pub fn draw_term(
    scene: &mut dyn Scene2D,
    term: &Term<EventProxy>,
    ctx: &mut DrawContext<'_>,
    underlay: &mut dyn FnMut(&mut dyn Scene2D),
) {
    let m = ctx.fonts.metrics;
    let (cw, ch, padx, pady) = (m.cell_w, m.cell_h, ctx.pad_x, ctx.pad_y);

    let (grid, cursor) = harvest(term, ctx);
    let mode = *term.mode();
    let palette = ctx.palette;

    // -- Background fill ---------------------------------------------------
    // `unfocused-split-fill` swaps only the pane's base fill; the
    // cell-skip compare stays against the theme background so unstyled
    // cells still let the unfocused fill show through.
    let theme_bg = palette.background;
    let default_bg = if !ctx.focused {
        ctx.unfocused_fill.unwrap_or(theme_bg)
    } else {
        theme_bg
    };
    scene.fill(
        Fill::NonZero,
        Affine::IDENTITY,
        &Brush::Solid(peniko_alpha(default_bg, ctx.bg_opacity)),
        None,
        &rect(0.0, 0.0, ctx.width, ctx.height),
    );

    // -- `background-image` under the grid --------------------------------
    // Over the base fill, under cell backgrounds: cells on the default
    // bg skip their own fill so the image shows through unstyled text.
    if let Some((brush, transform)) = &ctx.bg_image {
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            &Brush::Image(brush.clone()),
            Some(*transform),
            &rect(0.0, 0.0, ctx.width, ctx.height),
        );
    }

    for (row_i, row) in grid.rows.iter().enumerate() {
        if row.is_empty() {
            continue;
        }
        let y = row_y(pady, ch, row_i);
        for (start, end, style) in style_runs(row) {
            // Cells on the default bg are covered by the base fill — skipping
            // them keeps explicit backgrounds opaque over a translucent base.
            if style.bg == theme_bg {
                continue;
            }
            let x = col_x(padx, cw, start);
            let w = (end - start) as f32 * cw;
            let bgc = style.bg;
            scene.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                &Brush::Solid(peniko(bgc)),
                None,
                &rect(x, y, w, ch),
            );
        }
    }

    // -- Search highlights --------------------------------------------------
    for &(c0, c1, r) in ctx.search_matches {
        let active = ctx.search_active.contains(&(c0, c1, r));
        let color = if active {
            Rgb { r: 0xff, g: 0xa5, b: 0x00 }
        } else {
            Rgb { r: 0x8a, g: 0x6d, b: 0x3b }
        };
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            &Brush::Solid(peniko_alpha(color, if active { 0.55 } else { 0.30 })),
            None,
            &rect(col_x(padx, cw, c0), row_y(pady, ch, r), (c1 - c0) as f32 * cw, ch),
        );
    }

    // -- Images below text --------------------------------------------------
    underlay(scene);

    // -- Text ---------------------------------------------------------------
    for (row_i, row) in grid.rows.iter().enumerate() {
        if row.is_empty() {
            continue;
        }
        let baseline_y = row_y(pady, ch, row_i) + m.baseline;
        for (start, end, style) in style_runs(row) {
            draw_text_run(scene, row, start, end, style, row_i, padx, pady, baseline_y, ctx);
            draw_decorations(scene, start, end, style, row_i, padx, ch, baseline_y, ctx);
        }
    }

    // -- Ctrl-hover link underline -------------------------------------------
    if !ctx.hover_link.is_empty() {
        let hover_line = Brush::Solid(peniko_alpha(palette.accent, 0.85));
        for &(c0, c1, row) in ctx.hover_link {
            let x = col_x(padx, cw, c0);
            let y = row_y(pady, ch, row);
            scene.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                &hover_line,
                None,
                &rect(x, y + ch - 1.5, (c1 - c0) as f32 * cw, 1.5),
            );
        }
    }

    // -- URL hints ------------------------------------------------------------
    // Each hint is a small opaque badge over the URL's first cell(s) —
    // theme accent background + accent foreground — the URL text itself
    // stays fully readable (no wash over the span).
    if !ctx.hints.is_empty() || !ctx.hint_digits.is_empty() {
        let chip_bg = Brush::Solid(peniko_alpha(palette.accent, 1.0));
        let span_line = Brush::Solid(peniko_alpha(palette.accent, 0.6));
        for h in ctx.hints {
            let label = h.label.to_string();
            let w = label.chars().count() as f32 * cw;
            // Accent underline under every visible row part ties the
            // badge to its link — including parts across a soft wrap —
            // without washing out the text.
            for &(c0, c1, row) in &h.segments {
                let x = col_x(padx, cw, c0);
                let y = row_y(pady, ch, row);
                let span_w = (c1 - c0) as f32 * cw;
                scene.fill(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    &span_line,
                    None,
                    &rect(x, y + ch - 1.5, span_w, 1.5),
                );
            }
            let &(bc, _, brow) = &h.segments[0];
            let x = col_x(padx, cw, bc);
            let y = row_y(pady, ch, brow);
            scene.fill(Fill::NonZero, Affine::IDENTITY, &chip_bg, None, &rect(x, y, w, ch));
            draw_chip_text(scene, &label, x, y + m.baseline, palette.accent_fg, ctx);
        }
        if !ctx.hint_digits.is_empty() {
            let label = format!("open: {}", ctx.hint_digits);
            let y = ctx.height - ch - 4.0;
            let w = label.chars().count() as f32 * cw;
            scene.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                &chip_bg,
                None,
                &rect(4.0, y, w, ch),
            );
            draw_chip_text(scene, &label, 4.0, y + m.baseline, palette.accent_fg, ctx);
        }
    }

    // -- Cursor --------------------------------------------------------------
    draw_cursor(scene, &grid, &cursor, ctx, mode);

    // -- IME preedit ----------------------------------------------------------
    let preedit = ctx.preedit.clone();
    if let Some((text, caret)) = preedit {
        draw_preedit(scene, &text, caret, &cursor, ctx);
    }

    // -- Scrollbar ------------------------------------------------------------
    draw_scrollbar(scene, ctx);

    // -- Bell flash -----------------------------------------------------------
    // Full-pane step flash in the theme's foreground colour — visible on
    // light and dark palettes alike (kitty `visual_bell` shape).
    if ctx.bell_flash > 0.0 {
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            &Brush::Solid(peniko_alpha(ctx.palette.foreground, ctx.bell_flash)),
            None,
            &rect(0.0, 0.0, ctx.width, ctx.height),
        );
    }
}

/// Shape one style run through the host's parley stack and emit one scene
/// `GlyphRun` per run fontique split it into. Fontique's per-cluster fallback
/// replaces the old hand-rolled coverage cascade; per-cluster cell anchoring
/// keeps every glyph at its grid position (ligatures included).
#[allow(clippy::too_many_arguments)]
fn draw_text_run(
    scene: &mut dyn Scene2D,
    row: &[CellData],
    start: usize,
    end: usize,
    style: StyleKey,
    _row_i: usize,
    padx: f32,
    _pady: f32,
    baseline_y: f32,
    ctx: &mut DrawContext<'_>,
) {
    // Sub-run text plus a byte-offset → grid column map the clusters anchor to.
    let mut text = String::new();
    let mut cell_marks: Vec<(usize, usize)> = Vec::with_capacity(end - start);
    for (k, cell) in row[start..end].iter().enumerate() {
        cell_marks.push((text.len(), start + k));
        text.push_str(&cell.text);
    }
    if text.is_empty() {
        return;
    }

    let layout = ctx.fonts.shape_run(&text, style.bold, style.italic);
    let cw = ctx.fonts.metrics.cell_w;
    let brush = Brush::Solid(peniko(style.fg));
    let mut glyphs: Vec<Glyph> = Vec::new();

    for line in layout.lines() {
        for item in line.items() {
            let parley::PositionedLayoutItem::GlyphRun(gr) = item else {
                continue;
            };
            let run = gr.run();
            glyphs.clear();

            // Anchor each cluster's pen at the cell its text range starts in,
            // then advance the pen inside the cluster — a ligature spanning
            // cells keeps its shaped geometry, wide chars stay on their cell.
            let mut mark_idx = 0usize;
            let mut cur_col = start;
            let mut pen = 0.0f32;
            for cluster in run.visual_clusters() {
                let cs = cluster.text_range().start;
                while mark_idx + 1 < cell_marks.len() && cell_marks[mark_idx + 1].0 <= cs {
                    mark_idx += 1;
                }
                let col = cell_marks[mark_idx].1;
                if col != cur_col {
                    cur_col = col;
                    pen = 0.0;
                }
                let cell_x = col_x(padx, cw, col);
                for g in cluster.glyphs() {
                    glyphs.push(Glyph {
                        id: g.id,
                        x: cell_x + pen + g.x,
                        y: g.y,
                    });
                    pen += g.advance;
                }
            }
            if glyphs.is_empty() {
                continue;
            }

            let synthesis = crate::fonts::RunStyle::from(run.synthesis());
            let mut transform = Affine::translate((0.0, baseline_y as f64));
            if let Some(deg) = synthesis.skew {
                transform *= Affine::skew(f64::from(-deg).to_radians(), 0.0);
            }
            let out = GlyphRun {
                font: run.font(),
                font_size: run.font_size(),
                normalized_coords: run.normalized_coords(),
                transform,
                brush: &brush,
                brush_alpha: 1.0,
                style: StyleRef::Fill(Fill::NonZero),
                glyphs: &glyphs,
            };
            scene.draw_glyph_run(&out);

            // Faux bold: redraw with a half-cell-fraction offset, like the
            // offset emboldening native text stacks apply. `font-thicken`
            // applies the same overdraw to every run.
            if synthesis.embolden || ctx.font_thicken {
                let bold_run = GlyphRun {
                    transform: Affine::translate((0.6, baseline_y as f64)),
                    ..out
                };
                scene.draw_glyph_run(&bold_run);
            }
        }
    }
}

/// Underline + strikethrough for one style run.
#[allow(clippy::too_many_arguments)]
fn draw_decorations(
    scene: &mut dyn Scene2D,
    start: usize,
    end: usize,
    style: StyleKey,
    _row_i: usize,
    padx: f32,
    _ch: f32,
    baseline_y: f32,
    ctx: &mut DrawContext<'_>,
) {
    let m = ctx.fonts.metrics;
    let cw = m.cell_w;
    let x = col_x(padx, cw, start);
    let w = (end - start) as f32 * cw;
    let brush = Brush::Solid(peniko(style.ul_color.unwrap_or(style.fg)));

    let stroke_w = (m.stroke * ctx.underline_adjust.1).max(1.0) as f64;
    let underline_y = baseline_y as f64 + m.underline_pos as f64 + ctx.underline_adjust.0 as f64;
    // Straight decorations are square-ended rects centered on the line y, so
    // the run's first and last cells bound them exactly.
    let band = |y: f64| Rect::new(x as f64, y - stroke_w / 2.0, (x + w) as f64, y + stroke_w / 2.0);

    if style.deco & DECO_UNDERLINE != 0 {
        let p = band(underline_y).to_path(0.0);
        scene.fill(Fill::NonZero, Affine::IDENTITY, &brush, None, &p);
    }
    if style.deco & DECO_DOUBLE != 0 {
        for dy in [0.0, stroke_w * 2.0] {
            let p = band(underline_y + dy).to_path(0.0);
            scene.fill(Fill::NonZero, Affine::IDENTITY, &brush, None, &p);
        }
    }
    if style.deco & DECO_CURL != 0 {
        // Squiggly underline: ~1.5px amplitude, half-cell period. Butt caps so
        // the path ends inside the run's cells.
        let mut p = BezPath::new();
        p.move_to((x as f64, underline_y));
        let period = cw as f64 * 0.9;
        let amp = (m.stroke * 1.2).max(1.5) as f64;
        let mut xx = x as f64;
        let mut up = true;
        while xx < (x + w) as f64 {
            let nx = (xx + period / 2.0).min((x + w) as f64);
            let ny = if up { underline_y - amp } else { underline_y + amp * 0.4 };
            p.quad_to(((xx + nx) / 2.0, ny), (nx, underline_y));
            xx = nx;
            up = !up;
        }
        scene.stroke(
            &Stroke::new(stroke_w)
                .with_start_cap(kurbo::Cap::Butt)
                .with_end_cap(kurbo::Cap::Butt),
            Affine::IDENTITY,
            &brush,
            None,
            &p,
        );
    }
    if style.deco & (DECO_DOTTED | DECO_DASHED) != 0 {
        let dash = if style.deco & DECO_DOTTED != 0 {
            vec![0.0, stroke_w * 2.2]
        } else {
            vec![stroke_w * 3.0, stroke_w * 2.0]
        };
        let mut p = BezPath::new();
        p.move_to((x as f64, underline_y));
        p.line_to(((x + w) as f64, underline_y));
        scene.stroke(
            &Stroke::new(stroke_w).with_dashes(0.0, dash),
            Affine::IDENTITY,
            &brush,
            None,
            &p,
        );
    }
    if style.deco & DECO_STRIKE != 0 {
        let y = baseline_y as f64 - m.strikeout_pos as f64 - ctx.strikethrough_adjust.0 as f64;
        let w_stroke = (m.stroke * ctx.strikethrough_adjust.1).max(1.0) as f64;
        let p = Rect::new(x as f64, y - w_stroke / 2.0, (x + w) as f64, y + w_stroke / 2.0)
            .to_path(0.0);
        scene.fill(Fill::NonZero, Affine::IDENTITY, &brush, None, &p);
    }
}

/// Draw the cursor over its cell.
fn draw_cursor(
    scene: &mut dyn Scene2D,
    _grid: &Grid,
    cursor: &CursorInfo,
    ctx: &mut DrawContext<'_>,
    _mode: TermMode,
) {
    if cursor.row < 0 || cursor.row as usize >= ctx.scroll.screen_lines {
        return;
    }
    let m = ctx.fonts.metrics;
    let (cw, ch, padx, pady) = (m.cell_w, m.cell_h, ctx.pad_x, ctx.pad_y);
    let row = cursor.row as usize;
    let x = col_x(padx, cw, cursor.col);
    let y = row_y(pady, ch, row);
    let brush = Brush::Solid(peniko(ctx.palette.cursor));

    match cursor.shape {
        CursorShape::Hidden => {}
        CursorShape::Block => {
            if !ctx.focused {
                // Hollow outline for an unfocused surface.
                let r = Rect::new(
                    x as f64 + 0.5,
                    y as f64 + 0.5,
                    (x + cw) as f64 - 0.5,
                    (y + ch) as f64 - 0.5,
                );
                scene.stroke(&Stroke::new(1.0), Affine::IDENTITY, &brush, None, &r.to_path(0.0));
            }
            // Focused: the block was painted as the cell bg during harvest
            // when (blink_on || !blinking); in the blink-off phase nothing is
            // drawn, so hollow only ever means unfocused.
        }
        CursorShape::Underline => {
            if ctx.blink_on || !cursor.blinking {
                let t = (3.0 * ctx.cursor_thickness).max(1.0);
                scene.fill(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    &brush,
                    None,
                    &rect(x, y + ch - t, cw, t),
                );
            }
        }
        CursorShape::Beam => {
            if ctx.blink_on || !cursor.blinking {
                let t = (2.0 * ctx.cursor_thickness).max(1.0);
                scene.fill(Fill::NonZero, Affine::IDENTITY, &brush, None, &rect(x, y, t, ch));
            }
        }
        CursorShape::HollowBlock => {
            let r = Rect::new(
                x as f64 + 0.5,
                y as f64 + 0.5,
                (x + cw) as f64 - 0.5,
                (y + ch) as f64 - 0.5,
            );
            scene.stroke(&Stroke::new(1.0), Affine::IDENTITY, &brush, None, &r.to_path(0.0));
        }
    }
}

/// IME preedit drawn at the caret with an underline.
fn draw_preedit(
    scene: &mut dyn Scene2D,
    text: &str,
    _caret: usize,
    cursor: &CursorInfo,
    ctx: &mut DrawContext<'_>,
) {
    if text.is_empty() || cursor.row < 0 {
        return;
    }
    let m = ctx.fonts.metrics;
    let (cw, ch, padx, pady) = (m.cell_w, m.cell_h, ctx.pad_x, ctx.pad_y);
    let baseline_y = row_y(pady, ch, cursor.row as usize) + m.baseline;
    let x0 = col_x(padx, cw, cursor.col);

    // Shape the preedit like any other run; the chip is sized off the total
    // advance so the composed text always has a backdrop.
    let layout = ctx.fonts.shape_run(text, false, false);
    let brush = Brush::Solid(peniko(ctx.palette.foreground));

    // Collect positioned glyphs per parley run; pen advances across runs.
    let mut pen = 0.0f32;
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut glyphs: Vec<Glyph> = Vec::new();
    for line in layout.lines() {
        for item in line.items() {
            let parley::PositionedLayoutItem::GlyphRun(gr) = item else {
                continue;
            };
            let start = glyphs.len();
            for g in gr.glyphs() {
                glyphs.push(Glyph {
                    id: g.id,
                    x: x0 + pen + g.x,
                    y: g.y,
                });
                pen += g.advance;
            }
            runs.push((start, glyphs.len()));
        }
    }

    let w = pen.max(cw);
    scene.fill(
        Fill::NonZero,
        Affine::IDENTITY,
        &Brush::Solid(peniko_alpha(ctx.palette.selection_bg, 0.8)),
        None,
        &rect(x0, baseline_y - m.baseline, w, ch),
    );

    let mut idx = 0usize;
    for line in layout.lines() {
        for item in line.items() {
            let parley::PositionedLayoutItem::GlyphRun(gr) = item else {
                continue;
            };
            let (start, end) = runs[idx];
            idx += 1;
            let run = gr.run();
            scene.draw_glyph_run(&GlyphRun {
                font: run.font(),
                font_size: run.font_size(),
                normalized_coords: run.normalized_coords(),
                transform: Affine::translate((0.0, baseline_y as f64)),
                brush: &brush,
                brush_alpha: 1.0,
                style: StyleRef::Fill(Fill::NonZero),
                glyphs: &glyphs[start..end],
            });
        }
    }
    let mut p = BezPath::new();
    p.move_to((x0 as f64, (baseline_y + m.underline_pos) as f64));
    p.line_to(((x0 + w) as f64, (baseline_y + m.underline_pos) as f64));
    scene.stroke(&Stroke::new(1.0), Affine::IDENTITY, &brush, None, &p);
}

/// Small text drawn inside a chip — same `shape_run` + `GlyphRun` path
/// as the IME preedit, without the underline stroke.
fn draw_chip_text(
    scene: &mut dyn Scene2D,
    text: &str,
    x0: f32,
    baseline_y: f32,
    color: Rgb,
    ctx: &mut DrawContext<'_>,
) {
    let layout = ctx.fonts.shape_run(text, false, false);
    let brush = Brush::Solid(peniko(color));
    let mut pen = 0.0f32;
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut glyphs: Vec<Glyph> = Vec::new();
    for line in layout.lines() {
        for item in line.items() {
            let parley::PositionedLayoutItem::GlyphRun(gr) = item else {
                continue;
            };
            let start = glyphs.len();
            for g in gr.glyphs() {
                glyphs.push(Glyph {
                    id: g.id,
                    x: x0 + pen + g.x,
                    y: g.y,
                });
                pen += g.advance;
            }
            runs.push((start, glyphs.len()));
        }
    }
    let mut idx = 0usize;
    for line in layout.lines() {
        for item in line.items() {
            let parley::PositionedLayoutItem::GlyphRun(gr) = item else {
                continue;
            };
            let (start, end) = runs[idx];
            idx += 1;
            let run = gr.run();
            scene.draw_glyph_run(&GlyphRun {
                font: run.font(),
                font_size: run.font_size(),
                normalized_coords: run.normalized_coords(),
                transform: Affine::translate((0.0, baseline_y as f64)),
                brush: &brush,
                brush_alpha: 1.0,
                style: StyleRef::Fill(Fill::NonZero),
                glyphs: &glyphs[start..end],
            });
        }
    }
}

/// Thin scrollbar at the right edge when scrollback exists.
fn draw_scrollbar(scene: &mut dyn Scene2D, ctx: &DrawContext<'_>) {
    let s = &ctx.scroll;
    if s.history_size == 0 {
        return;
    }
    let total = s.history_size + s.screen_lines;
    let frac = s.screen_lines as f32 / total as f32;
    // display_offset counts lines scrolled back; 0 = live bottom.
    let thumb_top = (s.history_size - s.display_offset) as f32 / total as f32;
    let track_h = ctx.height;
    let bar_h = (track_h * frac).max(24.0);
    let bar_y = track_h * thumb_top;
    scene.fill(
        Fill::NonZero,
        Affine::IDENTITY,
        &Brush::Solid(Color::new([1.0, 1.0, 1.0, 0.25])),
        None,
        &rect(ctx.width - 4.0, bar_y, 3.0, bar_h),
    );
}
