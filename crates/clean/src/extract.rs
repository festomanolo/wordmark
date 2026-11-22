//! PDF text extraction: every glyph a page draws, with where, how big, at what angle and how
//! opaque, grouped into spans (glyphs drawn together on one baseline). The watermark detector
//! works on these spans and the rebuilder turns the kept ones back into paragraphs.

use craft_pdf::interpret::font::Glyph;
use craft_pdf::interpret::hayro_cmap::BfString;
use craft_pdf::interpret::{
    BlendMode, ClipPath, Context, Device, GlyphDrawMode, Image, InterpreterCache, InterpreterSettings, Paint, PathDrawMode, SoftMask, TransformExt,
};
use kurbo::{Affine, BezPath, Point, Rect, Shape, Vec2};
use serde::{Deserialize, Serialize};

/// hayro's glyph space: `glyph_transform` maps 1/1000 em units.
const UNITS_PER_EM: f64 = 1000.0;
/// Most glyphs read from one page; past this the page is cut short (and says so).
const MAX_GLYPHS_PER_PAGE: usize = 400_000;

/// A run of glyphs drawn together on one baseline with one size and angle.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Span {
    pub text: String,
    /// Box on the page, points, origin top-left, y down: `[x0, y0, x1, y1]`.
    pub bbox: [f32; 4],
    /// Baseline origin of the first glyph (points, top-left origin).
    pub origin: [f32; 2],
    /// Baseline angle in degrees, counterclockwise from horizontal as the reader sees it (-180..180].
    pub angle: f32,
    /// Font size in points (the em height on the page).
    pub size: f32,
    /// Fill opacity 0..=1 (fill alpha times any transparency group around it).
    pub alpha: f32,
    /// Fill colour, sRGB.
    pub rgb: [u8; 3],
    /// Drawn inside an `/Artifact` marked-content sequence (headers, footers, watermarks).
    pub artifact: bool,
    /// Drawn with text render mode 3 (invisible): usually the OCR layer over a scanned page.
    pub invisible: bool,
}

/// One page as read.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PageText {
    pub width: f32,
    pub height: f32,
    pub spans: Vec<Span>,
    /// Share of the page covered by raster images (0..=1, overlaps counted once per image).
    pub image_cover: f32,
    /// The page drew more glyphs than are read; the rest are missing.
    pub truncated: bool,
    /// Interpreting the page failed partway; what was read before the failure is kept.
    pub failed: bool,
}

impl PageText {
    /// A page that is a picture with (almost) no text behind it: a scan without an OCR layer.
    /// Its words can't be edited until someone runs OCR on it.
    pub fn needs_ocr(&self) -> bool {
        let chars: usize = self.spans.iter().map(|s| s.text.chars().filter(|c| !c.is_whitespace()).count()).sum();
        self.image_cover > 0.5 && chars < 20
    }
}

/// Why a PDF could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("not a readable PDF: {0}")]
    Open(String),
}

/// Read every page of `bytes` (a PDF). `password` opens an encrypted file.
pub fn extract_pdf(bytes: &[u8], password: Option<&str>) -> Result<Vec<PageText>, ExtractError> {
    let pdf = craft_pdf::open_slice(bytes, password).map_err(|e| ExtractError::Open(e.to_string()))?;
    let pages = pdf.pages();
    let mut out = Vec::with_capacity(pages.len());
    for page in pages.iter() {
        out.push(extract_page(page));
    }
    Ok(out)
}

fn extract_page<'a>(page: &'a craft_pdf::syntax::page::Page<'a>) -> PageText {
    let (w, h) = page.render_dimensions();
    let cache = InterpreterCache::new();
    let mut ctx = Context::new(
        page.initial_transform(true).to_kurbo(),
        Rect::new(0.0, 0.0, f64::from(w), f64::from(h)),
        &cache,
        page.xref(),
        InterpreterSettings::default(),
    );
    let mut dev = Collector::default();
    let failed = craft_pdf::interpret_page(page, &mut ctx, &mut dev).is_err();
    let page_area = (f64::from(w) * f64::from(h)).max(1.0);
    PageText {
        width: w,
        height: h,
        spans: group_spans(&dev.glyphs),
        image_cover: (dev.image_area / page_area).clamp(0.0, 1.0) as f32,
        truncated: dev.truncated,
        failed,
    }
}

/// One glyph as drawn.
#[derive(Clone, Debug)]
pub(crate) struct RawGlyph {
    pub text: String,
    pub origin: Point,
    /// Baseline direction scaled to one em.
    pub em_x: Vec2,
    /// Up direction scaled to one em.
    pub em_y: Vec2,
    /// Ink box (page coordinates); `None` for a glyph with no outline (a space).
    pub ink: Option<Rect>,
    pub alpha: f32,
    pub rgb: [u8; 3],
    pub artifact: bool,
    pub invisible: bool,
}

#[derive(Default)]
struct Collector {
    glyphs: Vec<RawGlyph>,
    /// Opacity of each open transparency group.
    groups: Vec<f32>,
    /// Open marked-content tags.
    marked: Vec<bool>,
    /// Inside `/ActualText`: the replacement text and whether it was emitted yet.
    actual: Vec<Option<(String, bool)>>,
    image_area: f64,
    truncated: bool,
}

impl Collector {
    fn group_alpha(&self) -> f32 {
        self.groups.iter().product()
    }
}

impl<'a> Device<'a> for Collector {
    fn set_soft_mask(&mut self, _: Option<SoftMask<'a>>) {}
    fn set_blend_mode(&mut self, _: BlendMode) {}
    fn draw_path(&mut self, _: &BezPath, _: Affine, _: &Paint<'a>, _: &PathDrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, opacity: f32, _: Option<SoftMask<'a>>, _: BlendMode) {
        self.groups.push(opacity.clamp(0.0, 1.0));
    }
    fn draw_glyph(&mut self, glyph: &Glyph<'a>, transform: Affine, glyph_transform: Affine, paint: &Paint<'a>, mode: &GlyphDrawMode) {
        if matches!(mode, GlyphDrawMode::Clip) {
            return;
        }
        if self.glyphs.len() >= MAX_GLYPHS_PER_PAGE {
            self.truncated = true;
            return;
        }
        let full = transform * glyph_transform;
        let lin = full * Affine::scale(UNITS_PER_EM);
        let origin = full * Point::ORIGIN;
        let o = lin * Point::ORIGIN;
        let em_x = (lin * Point::new(1.0, 0.0)) - o;
        let em_y = (lin * Point::new(0.0, 1.0)) - o;
        // Inside an `/ActualText` span the replacement text stands for all its glyphs: give it
        // to the first, nothing to the rest.
        let text = match self.actual.iter_mut().rev().find_map(Option::as_mut) {
            Some((t, done)) => {
                if *done {
                    String::new()
                } else {
                    *done = true;
                    std::mem::take(t)
                }
            }
            None => match glyph.as_unicode() {
                Some(BfString::Char(c)) => c.to_string(),
                Some(BfString::String(s)) => s,
                None => String::from('\u{FFFD}'),
            },
        };
        let ink = match glyph {
            Glyph::Outline(g) => {
                let path = g.outline();
                let b = (full * path).bounding_box();
                (b.width() > 0.0 || b.height() > 0.0).then_some(b)
            }
            Glyph::Type3(_) => None,
        };
        let (rgb, fill_alpha) = match paint {
            Paint::Color(c) => {
                let [r, g, b, a] = c.to_rgba().to_rgba8();
                ([r, g, b], f32::from(a) / 255.0)
            }
            Paint::Pattern(_) => ([128, 128, 128], 1.0),
        };
        self.glyphs.push(RawGlyph {
            text,
            origin,
            em_x,
            em_y,
            ink,
            alpha: fill_alpha * self.group_alpha(),
            rgb,
            artifact: self.marked.iter().any(|a| *a),
            invisible: matches!(mode, GlyphDrawMode::Invisible),
        });
    }
    fn draw_image(&mut self, image: Image<'a, '_>, transform: Affine) {
        if let Image::Raster(_) = &image {
            // The transform places the image's pixel grid.
            let px = Rect::new(0.0, 0.0, f64::from(image.width()), f64::from(image.height()));
            let b = (transform * px.to_path(0.1)).bounding_box();
            self.image_area += b.area().abs();
        }
    }
    fn pop_clip_path(&mut self) {}
    fn pop_transparency_group(&mut self) {
        self.groups.pop();
    }
    fn begin_marked_content(&mut self, tag: &[u8], _mcid: Option<i32>) {
        self.marked.push(tag == b"Artifact");
        self.actual.push(None);
    }
    fn actual_text(&mut self, text: &[u8]) {
        if let Some(slot) = self.actual.last_mut() {
            *slot = Some((decode_text_string(text), false));
        }
    }
    fn end_marked_content(&mut self) {
        self.marked.pop();
        self.actual.pop();
    }
}

/// A PDF text string: UTF-16BE with a BOM, UTF-8 with a BOM, else PDFDocEncoding (read as Latin-1,
/// which agrees with it for the printable range).
fn decode_text_string(b: &[u8]) -> String {
    if let Some(rest) = b.strip_prefix(&[0xFE, 0xFF]) {
        let units: Vec<u16> = rest.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c)).collect();
        return String::from_utf16_lossy(&units);
    }
    if let Some(rest) = b.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    b.iter().map(|&c| char::from(c)).collect()
}

fn len(v: Vec2) -> f64 {
    v.hypot()
}

/// Group glyphs, in drawing order, into spans: a new span starts where the size, angle, colour,
/// opacity or marking changes, or where the next glyph is not further along the same baseline.
pub(crate) fn group_spans(glyphs: &[RawGlyph]) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut cur: Option<(Span, Point, Vec2)> = None; // span, pen position (end of last glyph), unit baseline
    for g in glyphs {
        let size = len(g.em_y).max(len(g.em_x));
        if !size.is_finite() || size <= 0.01 {
            continue;
        }
        let ex = len(g.em_x);
        let dir = if ex > 0.0 { g.em_x / ex } else { Vec2::new(1.0, 0.0) };
        // Page y runs down; the reader's counterclockwise angle flips the sign.
        let angle = (-dir.y).atan2(dir.x).to_degrees() as f32;
        let advance = g.ink.map_or(0.5 * ex, |r| {
            let far = [r.x0, r.x1]
                .into_iter()
                .flat_map(|x| [Point::new(x, r.y0), Point::new(x, r.y1)])
                .map(|p| (p - g.origin).dot(dir))
                .fold(0.0_f64, f64::max);
            far.max(0.25 * ex)
        });
        let size_f = size as f32;
        let joins = cur.as_ref().and_then(|(s, pen, sdir)| {
            let same_style = (s.size - size_f).abs() <= 0.12 * s.size
                && angle_diff(s.angle, angle) < 3.0
                && s.rgb == g.rgb
                && (s.alpha - g.alpha).abs() < 0.05
                && s.artifact == g.artifact
                && s.invisible == g.invisible;
            if !same_style {
                return None;
            }
            let d = g.origin - *pen;
            let along = d.dot(*sdir) / size;
            let across = (d.x * -sdir.y + d.y * sdir.x).abs() / size;
            (across < 0.35 && (-0.6..4.0).contains(&along)).then_some(along)
        });
        match (joins, cur.as_mut()) {
            (Some(along), Some((s, pen, _))) => {
                // A gap of a quarter em or more is a word space the PDF drew as a move.
                if along > 0.25 && !s.text.ends_with(char::is_whitespace) && !g.text.starts_with(char::is_whitespace) {
                    s.text.push(' ');
                }
                s.text.push_str(&g.text);
                *pen = g.origin + dir * advance;
                grow(&mut s.bbox, g, dir, advance);
            }
            _ => {
                if let Some((s, _, _)) = cur.take() {
                    push_span(&mut spans, s);
                }
                let mut s = Span {
                    text: g.text.clone(),
                    bbox: [g.origin.x as f32, g.origin.y as f32, g.origin.x as f32, g.origin.y as f32],
                    origin: [g.origin.x as f32, g.origin.y as f32],
                    angle,
                    size: size_f,
                    alpha: g.alpha,
                    rgb: g.rgb,
                    artifact: g.artifact,
                    invisible: g.invisible,
                };
                grow(&mut s.bbox, g, dir, advance);
                cur = Some((s, g.origin + dir * advance, dir));
            }
        }
    }
    if let Some((s, _, _)) = cur {
        push_span(&mut spans, s);
    }
    spans
}

fn push_span(spans: &mut Vec<Span>, mut s: Span) {
    let t = s.text.trim();
    if t.is_empty() {
        return;
    }
    if t.len() != s.text.len() {
        s.text = t.to_string();
    }
    spans.push(s);
}

/// Grow `bbox` by the glyph's em box: from the baseline origin along `advance`, a fifth of an
/// em below the baseline to four fifths above it.
fn grow(bbox: &mut [f32; 4], g: &RawGlyph, dir: Vec2, advance: f64) {
    // `em_y` is the glyph's up direction on the page, one em long.
    let up = g.em_y;
    let corners = [g.origin + up * -0.2, g.origin + up * 0.8, g.origin + dir * advance + up * -0.2, g.origin + dir * advance + up * 0.8];
    for p in corners {
        let (x, y) = (p.x as f32, p.y as f32);
        if x.is_finite() && y.is_finite() {
            bbox[0] = bbox[0].min(x);
            bbox[1] = bbox[1].min(y);
            bbox[2] = bbox[2].max(x);
            bbox[3] = bbox[3].max(y);
        }
    }
}

/// The smaller angle between two directions, degrees (0..=180).
pub fn angle_diff(a: f32, b: f32) -> f32 {
    let d = (a - b).rem_euclid(360.0);
    d.min(360.0 - d)
}
