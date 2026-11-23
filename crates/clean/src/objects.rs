//! The things a PDF page draws on top of its content, found and removed in place: pictures,
//! text, link areas and watermark annotations.
//!
//! [`scan`] reads every page's content stream on the object layer and lists what it draws, each
//! with its box on the page and a score for how much it looks like a watermark: a picture pasted
//! over a scanned page, a logo or line repeated in the same spot on most pages, something in the
//! page margin under a link, a see-through or turned stamp. [`remove`] takes the objects out of
//! the content streams and annotation lists and writes the PDF again in full, so the rest of the
//! page (the scan, its fonts, its layout) is kept as it was and what was removed is not left in
//! an older revision of the file.
//!
//! Pictures and text drawn inside a form XObject are one object (the form); vector drawings
//! (paths) are not listed.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use craft_pdf::cos::content::{self, Matrix, Pieces};
use craft_pdf::cos::{Dict, Document, ObjRef, Object, SaveOptions, Stream, write_full};
use serde::{Deserialize, Serialize};

use crate::detect::{Decision, Model, normalize, similarity};

/// The largest decoded content stream read from one page (bytes).
const MAX_CONTENT: usize = 64 << 20;
/// The most pages walked; a page tree claiming more is cut short.
const MAX_PAGES: usize = 20_000;
/// How deep a page tree may nest.
const MAX_TREE_DEPTH: usize = 64;
/// A picture covering this share of the page is the page itself (a scan or a full-page photo).
const BACKGROUND_SHARE: f64 = 0.4;
/// Margin bands: the share of the page height (top and bottom) and width (left and right).
const EDGE_BAND: f64 = 0.1;
const SIDE_BAND: f64 = 0.08;
/// Two objects on different pages are in the same spot when their centres are this close (share
/// of the page side) and their sizes within this ratio.
const SAME_SPOT: f64 = 0.04;
const SAME_SIZE: f64 = 0.3;
/// The text similarity at which a learned watermark or content text is recognised.
const LEARNED_MATCH: f32 = 0.85;

/// What kind of thing an object is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A raster picture (an image XObject or an inline image).
    Image,
    /// A form XObject: a group of drawing drawn as one.
    Graphic,
    /// A text block (`BT … ET`).
    Text,
    /// A link area (a `/Link` annotation).
    Link,
    /// A watermark or stamp annotation.
    Stamp,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Image => "image",
            Kind::Graphic => "graphic",
            Kind::Text => "text",
            Kind::Link => "link",
            Kind::Stamp => "stamp",
        }
    }
}

/// One object a page draws.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PageObject {
    /// Stable within one file: `"<page>:<c|a><index>"`, the operator index in the page's content
    /// or the annotation's index. [`remove`] takes these.
    pub id: String,
    /// 1-based page.
    pub page: usize,
    pub kind: Kind,
    /// Box on the page as shown (crop box, page rotation applied), points, origin top-left, y down.
    pub bbox: [f32; 4],
    /// The text drawn, a link's address, or a stamp's contents.
    pub text: String,
    /// A picture's size in pixels.
    pub pixels: Option<[u32; 2]>,
    /// What is learned when a person marks it (what it reads, links to or holds, and where it
    /// sits): the same for the look-alikes on other pages and in other files from the same source.
    pub keys: Vec<String>,
    /// Objects that are the same thing on different pages share a group (the first one's id).
    pub group: String,
    pub score: f32,
    pub decision: Decision,
    /// The signals that raised or lowered the score, by name.
    pub reasons: Vec<String>,
}

/// What [`scan`] found.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Scan {
    /// Each page's size as shown, points (width, height).
    pub pages: Vec<[f32; 2]>,
    pub objects: Vec<PageObject>,
    /// 1-based pages whose content could not be read; nothing on them is listed.
    pub unreadable: Vec<usize>,
}

/// What [`remove`] did.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Removed {
    pub images: usize,
    pub graphics: usize,
    pub texts: usize,
    pub links: usize,
    pub stamps: usize,
    /// Ids asked for that no page has.
    pub not_found: Vec<String>,
}

impl Removed {
    pub fn total(&self) -> usize {
        self.images + self.graphics + self.texts + self.links + self.stamps
    }
}

// ── reading ─────────────────────────────────────────────────────────────────────────────────

fn open(bytes: &[u8], password: Option<&str>) -> Result<Document, String> {
    Document::open_with_password(Arc::new(bytes.to_vec()), password).map_err(|e| e.to_string())
}

/// A page with the attributes it inherits.
struct PageRef {
    r: ObjRef,
    dict: Dict,
    resources: Dict,
    /// The shown box in user space `[x0 y0 x1 y1]`.
    view: [f64; 4],
    rotate: i64,
}

impl PageRef {
    /// Shown size (rotation applied).
    fn size(&self) -> [f64; 2] {
        let (w, h) = (self.view[2] - self.view[0], self.view[3] - self.view[1]);
        if self.rotate % 180 == 0 { [w, h] } else { [h, w] }
    }

    /// A user-space point as shown: origin top-left, y down.
    fn to_view(&self, x: f64, y: f64) -> (f64, f64) {
        let (x, y) = (x - self.view[0], y - self.view[1]);
        let (w, h) = (self.view[2] - self.view[0], self.view[3] - self.view[1]);
        match self.rotate {
            90 => (y, x),
            180 => (w - x, y),
            270 => (h - y, w - x),
            _ => (x, h - y),
        }
    }

    /// A user-space box `[x0 y0 x1 y1]` as shown.
    fn rect_to_view(&self, r: [f64; 4]) -> [f32; 4] {
        let pts = [self.to_view(r[0], r[1]), self.to_view(r[2], r[1]), self.to_view(r[0], r[3]), self.to_view(r[2], r[3])];
        let mut b = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
        for (x, y) in pts {
            b = [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)];
        }
        [b[0] as f32, b[1] as f32, b[2] as f32, b[3] as f32]
    }
}

fn rect_of(doc: &Document, o: Option<&Object>) -> Option<[f64; 4]> {
    let o = doc.resolve(o?);
    let a = o.as_array()?;
    if a.len() != 4 {
        return None;
    }
    let mut v = [0.0; 4];
    for (x, o) in v.iter_mut().zip(a) {
        *x = doc.resolve(o).as_f64()?;
    }
    Some([v[0].min(v[2]), v[1].min(v[3]), v[0].max(v[2]), v[1].max(v[3])])
}

/// Every page in order, with inherited `/Resources`, `/MediaBox`, `/CropBox` and `/Rotate`.
fn pages(doc: &Document) -> Vec<PageRef> {
    struct Inherited {
        resources: Option<Object>,
        media: Option<Object>,
        crop: Option<Object>,
        rotate: Option<Object>,
    }
    fn walk(doc: &Document, r: ObjRef, inh: &Inherited, depth: usize, seen: &mut HashSet<u32>, out: &mut Vec<PageRef>) {
        if depth > MAX_TREE_DEPTH || out.len() >= MAX_PAGES || !seen.insert(r.num) {
            return;
        }
        let Some(d) = doc.get(r).as_dict().cloned() else { return };
        let pick = |key: &[u8], up: &Option<Object>| d.get(key).cloned().or_else(|| up.clone());
        let here = Inherited {
            resources: pick(b"Resources", &inh.resources),
            media: pick(b"MediaBox", &inh.media),
            crop: pick(b"CropBox", &inh.crop),
            rotate: pick(b"Rotate", &inh.rotate),
        };
        let is_pages = d.name(b"Type") == Some(b"Pages") || (d.name(b"Type").is_none() && d.contains(b"Kids"));
        if is_pages {
            let kids = doc.resolve(d.get(b"Kids").unwrap_or(&Object::Null));
            for k in kids.as_array().map(Vec::as_slice).unwrap_or_default() {
                if let Some(kr) = k.as_ref() {
                    walk(doc, kr, &here, depth + 1, seen, out);
                }
            }
            return;
        }
        let media = rect_of(doc, here.media.as_ref()).unwrap_or([0.0, 0.0, 612.0, 792.0]);
        let view = match rect_of(doc, here.crop.as_ref()) {
            // The crop box clipped to the media box.
            Some(c) if c[0].max(media[0]) < c[2].min(media[2]) && c[1].max(media[1]) < c[3].min(media[3]) => {
                [c[0].max(media[0]), c[1].max(media[1]), c[2].min(media[2]), c[3].min(media[3])]
            }
            _ => media,
        };
        let rotate = here.rotate.as_ref().and_then(|o| doc.resolve(o).as_int()).unwrap_or(0).rem_euclid(360) / 90 * 90;
        let resources = here.resources.as_ref().and_then(|o| doc.dict(o)).unwrap_or_default();
        out.push(PageRef { r, dict: d, resources, view, rotate });
    }
    let mut out = Vec::new();
    let Some(root) = doc.root() else { return out };
    let Some(pages_ref) = doc.get(root).as_dict().and_then(|d| d.reference(b"Pages")) else { return out };
    let none = Inherited { resources: None, media: None, crop: None, rotate: None };
    walk(doc, pages_ref, &none, 0, &mut HashSet::new(), &mut out);
    out
}

/// The page's content streams, decoded, joined as the renderer joins them.
fn page_content(doc: &Document, page: &PageRef) -> Result<Pieces, String> {
    let Some(c) = page.dict.get(b"Contents") else { return Ok(Pieces::join::<Vec<u8>>(&[])) };
    let refs: Vec<Object> = match &*doc.resolve(c) {
        Object::Array(a) => a.clone(),
        _ => vec![c.clone()],
    };
    let mut pieces = Vec::with_capacity(refs.len());
    for o in refs {
        let held = doc.resolve(&o);
        let Object::Stream(s) = &*held else { continue };
        let data = doc.with_direct_parms(s).decoded_within(MAX_CONTENT).map_err(|e| e.to_string())?;
        pieces.push(data);
    }
    Ok(Pieces::join(&pieces))
}

/// A named resource of a category (`XObject`, `ExtGState`).
fn resource(doc: &Document, resources: &Dict, category: &[u8], name: &[u8]) -> Option<Arc<Object>> {
    let cat = doc.dict(resources.get(category)?)?;
    Some(doc.resolve(cat.get(name)?))
}

/// The graphics state the scan follows.
#[derive(Clone, Copy)]
struct Gs {
    ctm: Matrix,
    alpha: f64,
    soft_mask: bool,
    /// Fill colour lightness 0..=1.
    light: f64,
    font_size: f64,
    leading: f64,
    render_mode: i64,
}

/// One object as found in the content, before scoring.
struct Found {
    obj: PageObject,
    /// Content operators that draw it (indices into the parsed stream).
    ops: Vec<usize>,
    /// The resource name a `Do` draws.
    xobject: Option<Vec<u8>>,
    /// Facts for scoring.
    order: usize,
    user_box: [f64; 4],
    see_through: bool,
    light: bool,
    turned: bool,
    invisible: bool,
    font_size: f64,
    image_hash: u64,
    uri: Option<String>,
    internal_link: bool,
}

fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Is the matrix's x axis off the page axes (a turned stamp)? Quarter turns are not.
fn turned(m: &Matrix) -> bool {
    let [a, b, ..] = m.0;
    let deg = b.atan2(a).to_degrees().rem_euclid(90.0);
    deg > 3.0 && deg < 87.0
}

/// Text of a string operand: PDFDocEncoding or UTF-16 as declared, control bytes dropped.
fn show_text(o: &Object) -> String {
    match o {
        Object::String(s) => s.to_text().chars().filter(|c| !c.is_control()).collect(),
        Object::Array(a) => {
            let mut t = String::new();
            for x in a {
                match x {
                    Object::String(_) => t.push_str(&show_text(x)),
                    // A large negative adjustment is a word space.
                    _ if x.as_f64().is_some_and(|v| v < -200.0) => t.push(' '),
                    _ => {}
                }
            }
            t
        }
        _ => String::new(),
    }
}

/// Bytes a show operator draws (an estimate of its glyph count) and its kerning, in 1/1000 em.
fn show_extent(o: &Object) -> (usize, f64) {
    match o {
        Object::String(s) => (s.bytes.len(), 0.0),
        Object::Array(a) => a.iter().fold((0, 0.0), |(n, k), x| match x {
            Object::String(s) => (n + s.bytes.len(), k),
            _ => (n, k - x.as_f64().unwrap_or(0.0)),
        }),
        _ => (0, 0.0),
    }
}

/// Mostly readable text: letters, digits, punctuation and spaces.
fn readable(t: &str) -> bool {
    let n = t.chars().count();
    n > 0 && t.chars().filter(|c| c.is_alphanumeric() || c.is_ascii_punctuation() || c.is_whitespace()).count() * 10 >= n * 8
}

/// Everything page `page` (0-based) draws, and its parsed content.
fn read_page(doc: &Document, page: &PageRef, index: usize) -> Result<(Vec<Found>, content::Parsed), String> {
    let pieces = page_content(doc, page)?;
    let parsed = pieces.parse();
    let mut out = Vec::new();
    let mut gs = Gs { ctm: Matrix::IDENTITY, alpha: 1.0, soft_mask: false, light: 0.0, font_size: 0.0, leading: 0.0, render_mode: 0 };
    let mut stack: Vec<Gs> = Vec::new();
    let (mut tm, mut tlm) = (Matrix::IDENTITY, Matrix::IDENTITY);
    // The text block being read: its first operator, show operators, box, text.
    let mut block: Option<(usize, Vec<usize>, Option<[f64; 4]>, String, Gs, bool)> = None;
    let id = |kind: char, i: usize| format!("{}:{kind}{i}", index + 1);
    for (i, op) in parsed.ops.iter().enumerate() {
        match op.op.as_slice() {
            b"q" => stack.push(gs),
            b"Q" => gs = stack.pop().unwrap_or(gs),
            b"cm" => {
                if let Some(m) = op.operands.len().checked_sub(6).and_then(|k| Matrix::from_operands(&op.operands[k..])) {
                    gs.ctm = m.then(&gs.ctm);
                }
            }
            b"gs" => {
                if let Some(d) = op.last_name().and_then(|n| resource(doc, &page.resources, b"ExtGState", n)).and_then(|o| o.as_dict().cloned()) {
                    if let Some(a) = d.get(b"ca").and_then(Object::as_f64) {
                        gs.alpha = a.clamp(0.0, 1.0);
                    }
                    if let Some(sm) = d.get(b"SMask") {
                        gs.soft_mask = sm.as_name() != Some(b"None");
                    }
                }
            }
            b"g" => gs.light = op.num_back(0).unwrap_or(0.0),
            b"rg" => {
                if let Some([r, g, b]) = op.nums::<3>() {
                    gs.light = 0.3 * r + 0.59 * g + 0.11 * b;
                }
            }
            b"k" => {
                if let Some([c, m, y, k]) = op.nums::<4>() {
                    gs.light = (1.0 - k) * (1.0 - (0.3 * c + 0.59 * m + 0.11 * y));
                }
            }
            b"BT" => {
                tm = Matrix::IDENTITY;
                tlm = Matrix::IDENTITY;
                block = Some((i, Vec::new(), None, String::new(), gs, false));
            }
            b"ET" => {
                if let Some((first, shows, Some(b), text, at, turned_text)) = block.take()
                    && !shows.is_empty()
                {
                    // Text in a font whose codes aren't characters reads as nothing, not noise.
                    let text = if readable(text.trim()) { text.trim().to_string() } else { String::new() };
                    out.push(Found {
                        obj: object(id('c', first), index, Kind::Text, page.rect_to_view(b), text),
                        ops: shows,
                        xobject: None,
                        order: first,
                        user_box: b,
                        see_through: at.alpha < 0.95 || at.soft_mask,
                        light: at.light > 0.75,
                        turned: turned_text,
                        invisible: at.render_mode % 4 == 3,
                        font_size: at.font_size,
                        image_hash: 0,
                        uri: None,
                        internal_link: false,
                    });
                }
            }
            b"Tf" => gs.font_size = op.num_back(0).unwrap_or(gs.font_size).abs(),
            b"TL" => gs.leading = op.num_back(0).unwrap_or(0.0),
            b"Tr" => gs.render_mode = op.num_back(0).map_or(0, |v| v as i64),
            b"Td" | b"TD" => {
                if let Some([x, y]) = op.nums::<2>() {
                    if op.is("TD") {
                        gs.leading = -y;
                    }
                    tlm = Matrix::translate(x, y).then(&tlm);
                    tm = tlm;
                }
            }
            b"Tm" => {
                if let Some(m) = op.operands.len().checked_sub(6).and_then(|k| Matrix::from_operands(&op.operands[k..])) {
                    tlm = m;
                    tm = m;
                }
            }
            b"T*" => {
                tlm = Matrix::translate(0.0, -gs.leading).then(&tlm);
                tm = tlm;
            }
            b"Tj" | b"TJ" | b"'" | b"\"" => {
                if op.is("'") || op.is("\"") {
                    tlm = Matrix::translate(0.0, -gs.leading).then(&tlm);
                    tm = tlm;
                }
                let Some(arg) = op.operands.last() else { continue };
                let (n, kern) = show_extent(arg);
                let size = if gs.font_size > 0.0 { gs.font_size } else { 12.0 };
                // Half an em a glyph: a guess that is good enough for a box.
                let width = ((n as f64) * 500.0 + kern).max(0.0) / 1000.0 * size;
                let to_page = tm.then(&gs.ctm);
                let b = to_page.bbox([0.0, -0.25 * size, width, 0.85 * size]);
                tm = Matrix::translate(width, 0.0).then(&tm);
                if let Some((_, shows, bbox, text, at, turned_text)) = block.as_mut() {
                    shows.push(i);
                    *bbox = Some(bbox.map_or(b, |a| [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]));
                    let t = show_text(arg);
                    if !text.is_empty() && !t.is_empty() {
                        text.push(' ');
                    }
                    text.push_str(&t);
                    // The block takes the state of its first drawing.
                    if shows.len() == 1 {
                        *at = gs;
                        *turned_text = turned(&to_page);
                    }
                }
            }
            b"Do" => {
                let Some(name) = op.last_name() else { continue };
                let Some(x) = resource(doc, &page.resources, b"XObject", name) else { continue };
                let Object::Stream(s) = &*x else { continue };
                let d = &s.dict;
                let (kind, b, pixels) = match d.name(b"Subtype") {
                    Some(b"Image") => {
                        let px = [d.int(b"Width").unwrap_or(0), d.int(b"Height").unwrap_or(0)].map(|v| u32::try_from(v).unwrap_or(0));
                        (Kind::Image, gs.ctm.bbox([0.0, 0.0, 1.0, 1.0]), Some(px))
                    }
                    Some(b"Form") => {
                        let m = d.get(b"Matrix").and_then(|o| Matrix::from_operands(doc.resolve(o).as_array()?)).unwrap_or(Matrix::IDENTITY);
                        let bb = rect_of(doc, d.get(b"BBox")).unwrap_or([0.0, 0.0, 1.0, 1.0]);
                        (Kind::Graphic, m.then(&gs.ctm).bbox(bb), None)
                    }
                    _ => continue,
                };
                let mask = d.contains(b"SMask") || d.int(b"SMaskInData").unwrap_or(0) > 0 || d.contains(b"Mask");
                out.push(Found {
                    obj: object(id('c', i), index, kind, page.rect_to_view(b), String::new()).with_pixels(pixels),
                    ops: vec![i],
                    xobject: Some(name.to_vec()),
                    order: i,
                    user_box: b,
                    see_through: gs.alpha < 0.95 || gs.soft_mask || mask,
                    light: false,
                    turned: turned(&gs.ctm),
                    invisible: false,
                    font_size: 0.0,
                    image_hash: fnv(&s.raw),
                    uri: None,
                    internal_link: false,
                });
            }
            b"BI" => {
                let Some((d, data)) = &op.inline else { continue };
                let px = [d.int(b"W").or_else(|| d.int(b"Width")).unwrap_or(0), d.int(b"H").or_else(|| d.int(b"Height")).unwrap_or(0)]
                    .map(|v| u32::try_from(v).unwrap_or(0));
                let b = gs.ctm.bbox([0.0, 0.0, 1.0, 1.0]);
                out.push(Found {
                    obj: object(id('c', i), index, Kind::Image, page.rect_to_view(b), String::new()).with_pixels(Some(px)),
                    ops: vec![i],
                    xobject: None,
                    order: i,
                    user_box: b,
                    see_through: gs.alpha < 0.95 || gs.soft_mask,
                    light: false,
                    turned: turned(&gs.ctm),
                    invisible: false,
                    font_size: 0.0,
                    image_hash: fnv(data),
                    uri: None,
                    internal_link: false,
                });
            }
            _ => {}
        }
    }
    // Annotations: link areas and watermark stamps.
    let annots = page.dict.get(b"Annots").map(|o| doc.resolve(o));
    for (k, a) in annots.as_deref().and_then(Object::as_array).map(Vec::as_slice).unwrap_or_default().iter().enumerate() {
        let Some(d) = doc.dict(a) else { continue };
        let Some(r) = rect_of(doc, d.get(b"Rect")) else { continue };
        let (kind, text, uri, internal) = match d.name(b"Subtype") {
            Some(b"Link") => {
                let action = d.get(b"A").and_then(|o| doc.dict(o));
                let uri = action
                    .as_ref()
                    .filter(|a| a.name(b"S") == Some(b"URI"))
                    .and_then(|a| doc.resolve(a.get(b"URI")?).as_string().map(|s| s.to_text()));
                let internal = uri.is_none() && (d.contains(b"Dest") || action.is_some_and(|a| matches!(a.name(b"S"), Some(b"GoTo" | b"Named"))));
                (Kind::Link, uri.clone().unwrap_or_default(), uri, internal)
            }
            Some(b"Watermark" | b"Stamp") => {
                let t = d.get(b"Contents").and_then(|o| doc.resolve(o).as_string().map(|s| s.to_text())).unwrap_or_default();
                (Kind::Stamp, t, None, false)
            }
            _ => continue,
        };
        let watermark_annot = d.name(b"Subtype") == Some(b"Watermark");
        out.push(Found {
            obj: object(id('a', k), index, kind, page.rect_to_view(r), text),
            ops: Vec::new(),
            xobject: None,
            order: usize::MAX,
            user_box: r,
            see_through: false,
            light: false,
            turned: false,
            invisible: false,
            font_size: if watermark_annot { 1.0 } else { 0.0 },
            image_hash: 0,
            uri,
            internal_link: internal,
        });
    }
    Ok((out, parsed))
}

fn object(id: String, index: usize, kind: Kind, bbox: [f32; 4], text: String) -> PageObject {
    PageObject {
        id,
        page: index + 1,
        kind,
        bbox,
        text,
        pixels: None,
        keys: Vec::new(),
        group: String::new(),
        score: 0.0,
        decision: Decision::Keep,
        reasons: Vec::new(),
    }
}

impl PageObject {
    fn with_pixels(mut self, pixels: Option<[u32; 2]>) -> Self {
        self.pixels = pixels;
        self
    }
}

// ── scoring ─────────────────────────────────────────────────────────────────────────────────

fn area(b: [f64; 4]) -> f64 {
    (b[2] - b[0]).max(0.0) * (b[3] - b[1]).max(0.0)
}

fn overlap(a: [f64; 4], b: [f64; 4]) -> bool {
    content::overlaps(a, b, 0.0)
}

/// Where an object sits, as a share of the shown page: centre and size, for "the same spot".
fn spot(o: &PageObject, size: [f32; 2]) -> [f64; 4] {
    let [w, h] = [f64::from(size[0].max(1.0)), f64::from(size[1].max(1.0))];
    let b = o.bbox.map(f64::from);
    [(b[0] + b[2]) / 2.0 / w, (b[1] + b[3]) / 2.0 / h, (b[2] - b[0]) / w, (b[3] - b[1]) / h]
}

fn same_spot(a: [f64; 4], b: [f64; 4]) -> bool {
    let size = |x: f64, y: f64| (x - y).abs() <= SAME_SIZE * x.max(y) + 0.005;
    (a[0] - b[0]).abs() <= SAME_SPOT && (a[1] - b[1]).abs() <= SAME_SPOT && size(a[2], b[2]) && size(a[3], b[3])
}

/// Where on the page, in words learnable across files: "image@0.85,0.97,0.04,0.03" (centre and
/// size to 2% of the page).
fn spot_key(kind: Kind, s: [f64; 4]) -> String {
    let r = |v: f64| (v * 50.0).round() / 50.0;
    format!("{}@{:.2},{:.2},{:.2},{:.2}", kind.name(), r(s[0]), r(s[1]), r(s[2]), r(s[3]))
}

fn link_host(uri: &str) -> String {
    let rest = uri.split_once("://").map_or(uri, |(_, r)| r);
    rest.split(['/', '?', '#']).next().unwrap_or(rest).trim_start_matches("www.").to_lowercase()
}

/// The keys a found object is learned under: what it reads or links to, and where it sits.
fn keys(f: &Found, s: [f64; 4]) -> Vec<String> {
    let mut k = Vec::new();
    match f.obj.kind {
        Kind::Text | Kind::Stamp if !f.obj.text.is_empty() => k.push(format!("text:{}", normalize(&f.obj.text))),
        Kind::Link => k.push(format!("link:{}", link_host(&f.obj.text))),
        Kind::Image | Kind::Graphic => k.push(format!("data:{:016x}", f.image_hash)),
        _ => {}
    }
    k.push(spot_key(f.obj.kind, s));
    k
}

fn add(z: &mut f32, why: &mut Vec<&'static str>, weight: f32, name: &'static str) {
    *z += weight;
    why.push(name);
}

fn sigmoid(z: f32) -> f32 {
    1.0 / (1.0 + (-z).exp())
}

/// The look-alikes across pages: every object as `(page, index)`, its group's root, the id that
/// names each group, and on how many pages the same thing sits in the same spot.
struct Groups {
    flat: Vec<(usize, usize)>,
    root: Vec<usize>,
    name: HashMap<usize, String>,
    repeat_pages: Vec<usize>,
}

/// Group: same kind and (same text, same link host, same picture data, or same spot) on another
/// page. Union-find over (page, index).
fn group(found: &[Vec<Found>], spots: &[Vec<[f64; 4]>]) -> Groups {
    let flat: Vec<(usize, usize)> = found.iter().enumerate().flat_map(|(p, fs)| (0..fs.len()).map(move |i| (p, i))).collect();
    let mut parent: Vec<usize> = (0..flat.len()).collect();
    fn root(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    let mut by_kind: HashMap<(Kind, String), Vec<usize>> = HashMap::new();
    for (n, &(p, i)) in flat.iter().enumerate() {
        let f = &found[p][i];
        let ident = match f.obj.kind {
            Kind::Text => (!f.obj.text.trim().is_empty()).then(|| normalize(&f.obj.text)),
            Kind::Link => f.uri.as_deref().map(link_host),
            Kind::Image | Kind::Graphic => Some(format!("{:016x}", f.image_hash)),
            Kind::Stamp => Some(normalize(&f.obj.text)),
        };
        if let Some(ident) = ident.filter(|s| !s.is_empty()) {
            by_kind.entry((f.obj.kind, ident)).or_default().push(n);
        }
    }
    for members in by_kind.values() {
        for w in members.windows(2) {
            let (a, b) = (root(&mut parent, w[0]), root(&mut parent, w[1]));
            parent[a] = b;
        }
    }
    // Same spot on different pages (pictures and graphics only: text in the same spot is often
    // different text, a heading or a page number, and groups by what it reads).
    for (x, &(p, i)) in flat.iter().enumerate() {
        let f = &found[p][i];
        if !matches!(f.obj.kind, Kind::Image | Kind::Graphic) || area(f.user_box) <= 0.0 {
            continue;
        }
        for (y, &(q, j)) in flat.iter().enumerate().skip(x + 1) {
            if q != p && found[q][j].obj.kind == f.obj.kind && same_spot(spots[p][i], spots[q][j]) {
                let (a, b) = (root(&mut parent, x), root(&mut parent, y));
                parent[a] = b;
            }
        }
    }
    let mut members: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut group_first: HashMap<usize, String> = HashMap::new();
    for (n, &(p, i)) in flat.iter().enumerate() {
        let r = root(&mut parent, n);
        members.entry(r).or_default().push(n);
        group_first.entry(r).or_insert_with(|| found[p][i].obj.id.clone());
    }
    // On how many pages the same thing sits in the same spot (a footer, a corner logo): the same
    // line elsewhere on another page is not a repeat.
    let mut repeat_pages: Vec<usize> = vec![1; flat.len()];
    for (n, &(p, i)) in flat.iter().enumerate() {
        let r = root(&mut parent, n);
        let pages: HashSet<usize> =
            members[&r].iter().map(|&m| flat[m]).filter(|&(q, j)| q == p || same_spot(spots[p][i], spots[q][j])).map(|(q, _)| q).collect();
        repeat_pages[n] = pages.len();
    }

    for (n, &(p, i)) in flat.iter().enumerate() {
        let r = root(&mut parent, n);
        let pages_with = repeat_pages[n];
        let page_area = f64::from(sizes[p][0]) * f64::from(sizes[p][1]);
        let s = spots[p][i];
        // The page's own pictures (a scan) and the links on it, from the same page.
        let (backgrounds, links): (Vec<(usize, [f64; 4])>, Vec<[f64; 4]>) = {
            let fs = &found[p];
            (
                fs.iter()
                    .filter(|g| g.obj.kind == Kind::Image && area(g.user_box) >= BACKGROUND_SHARE * page_area)
                    .map(|g| (g.order, g.user_box))
                    .collect(),
                fs.iter().filter(|g| g.obj.kind == Kind::Link).map(|g| g.user_box).collect(),
            )
        };
        let f = &found[p][i];
        let mut z: f32;
        let mut why: Vec<&'static str> = Vec::new();
        let b = f.obj.bbox.map(f64::from);
        let [w, h] = sizes[p].map(f64::from);
        let edge = b[3] <= EDGE_BAND * h || b[1] >= (1.0 - EDGE_BAND) * h || b[2] <= SIDE_BAND * w || b[0] >= (1.0 - SIDE_BAND) * w;
        let centred = (s[0] - 0.5).abs() < 0.15 && (s[1] - 0.5).abs() < 0.15;
        let repeats = pages_with >= 2 && pages_with * 2 >= n_pages;
        match f.obj.kind {
            Kind::Link => {
                if f.internal_link {
                    z = -4.0;
                    why.push("internal link");
                } else {
                    // Links out of the document are what "remove links" means.
                    z = 1.5;
                    why.push("web link");
                    if edge {
                        add(&mut z, &mut why, 1.0, "page margin");
                    }
                    if repeats {
                        add(&mut z, &mut why, 1.0, "on most pages");
                    }
                }
            }
            Kind::Stamp => {
                z = if f.font_size > 0.0 { 4.0 } else { 0.0 };
                why.push(if f.font_size > 0.0 { "watermark annotation" } else { "stamp annotation" });
            }
            Kind::Image | Kind::Graphic | Kind::Text => {
                z = -3.0;
                let size_share = area(f.user_box) / page_area.max(1.0);
                if f.obj.kind == Kind::Image && size_share >= BACKGROUND_SHARE {
                    add(&mut z, &mut why, -6.0, "page picture");
                }
                if f.invisible {
                    add(&mut z, &mut why, -6.0, "invisible text (OCR layer)");
                }
                let over_scan = backgrounds.iter().any(|(order, bb)| f.order > *order && overlap(f.user_box, *bb) && *bb != f.user_box);
                // A picture pasted over a scan is the commonest stamp; text over a full-page
                // picture is as often a designed page's real text, so it only leans.
                if over_scan && !f.invisible {
                    add(&mut z, &mut why, if f.obj.kind == Kind::Text { 0.5 } else { 3.5 }, "drawn over the page picture");
                }
                if repeats {
                    add(&mut z, &mut why, 2.0, "on most pages");
                }
                if edge {
                    add(&mut z, &mut why, 1.5, "page margin");
                }
                if links.iter().any(|l| overlap(f.user_box, *l)) {
                    add(&mut z, &mut why, 2.0, "under a link");
                }
                if f.see_through {
                    add(&mut z, &mut why, 0.7, "see-through");
                }
                if f.turned {
                    add(&mut z, &mut why, 1.5, "turned");
                }
                if centred {
                    add(&mut z, &mut why, 0.8, "middle of the page");
                }
                if f.obj.kind == Kind::Text {
                    let norm = normalize(&f.obj.text);
                    if crate::urls::contains_url(&norm) {
                        add(&mut z, &mut why, 1.5, "web address");
                    }
                    if f.light {
                        add(&mut z, &mut why, 0.8, "pale");
                    }
                    if body > 0.0 && f.font_size > 2.5 * body {
                        add(&mut z, &mut why, 0.8, "large");
                    }
                } else if size_share < 0.03 && size_share > 0.0 {
                    add(&mut z, &mut why, 0.5, "small");
                }
            }
        }
        // What a person marked before: by what it reads, links to or holds, or where it sits.
        let ks = keys(f, s);
        let text_norm = (matches!(f.obj.kind, Kind::Text | Kind::Stamp) && !f.obj.text.is_empty()).then(|| normalize(&f.obj.text));
        let learned_mark = ks.iter().any(|k| model.watermark_objects.contains(k))
            || text_norm.as_deref().is_some_and(|t| model.watermark_texts.iter().any(|w| similarity(t, w) >= LEARNED_MATCH));
        let learned_keep = ks.iter().any(|k| model.content_objects.contains(k))
            || text_norm.as_deref().is_some_and(|t| model.content_texts.iter().any(|w| similarity(t, w) >= LEARNED_MATCH));
        if learned_keep {
            add(&mut z, &mut why, -8.0, "marked as content before");
        } else if learned_mark {
            add(&mut z, &mut why, 5.0, "marked as a watermark before");
        }
        let score = sigmoid(z);
        let obj = &mut found[p][i].obj;
        obj.score = score;
        obj.decision = model.decide(score);
        obj.reasons = why.into_iter().map(str::to_string).collect();
        obj.keys = ks;
        obj.group = group_first.get(&r).cloned().unwrap_or_default();
    }

    // The same thing on several pages is one thing: it takes its strongest page's verdict on
    // every page (a header drawn over the scan on some pages and before it on others), unless a
    // person marked it as content.
    let mut best: HashMap<String, f32> = HashMap::new();
    for o in found.iter().flatten().map(|f| &f.obj) {
        let b = best.entry(o.group.clone()).or_insert(o.score);
        *b = b.max(o.score);
    }
    for o in found.iter_mut().flatten().map(|f| &mut f.obj) {
        if let Some(&b) = best.get(&o.group)
            && b > o.score
            && !o.reasons.iter().any(|r| r == "marked as content before")
        {
            o.score = b;
            o.decision = model.decide(b);
            o.reasons.push("as on its other pages".to_string());
        }
    }
}

/// Every object of the PDF's pages with how much it looks like a watermark. `password` opens an
/// encrypted file.
pub fn scan(bytes: &[u8], password: Option<&str>, model: &Model) -> Result<Scan, String> {
    let doc = open(bytes, password)?;
    let pages = pages(&doc);
    let mut found = Vec::with_capacity(pages.len());
    let mut unreadable = Vec::new();
    for (i, p) in pages.iter().enumerate() {
        match read_page(&doc, p, i) {
            Ok((f, _)) => found.push(f),
            Err(_) => {
                unreadable.push(i + 1);
                found.push(Vec::new());
            }
        }
    }
    let sizes: Vec<[f32; 2]> = pages.iter().map(|p| p.size().map(|v| v as f32)).collect();
    score(&mut found, &sizes, model);
    let objects = found.into_iter().flatten().map(|f| f.obj).collect();
    Ok(Scan { pages: sizes, objects, unreadable })
}

/// The keys under which `ids` (from [`scan`] of the same file) are learned.
pub fn keys_of(scan: &Scan, ids: &[String]) -> Vec<String> {
    let want: HashSet<&str> = ids.iter().map(String::as_str).collect();
    let mut out: Vec<String> = scan.objects.iter().filter(|o| want.contains(o.id.as_str())).flat_map(|o| o.keys.iter().cloned()).collect();
    out.sort();
    out.dedup();
    out
}

// ── removing ────────────────────────────────────────────────────────────────────────────────

/// The PDF without the objects `ids` names (from [`scan`] of the same file), written in full so
/// no older revision keeps them. Pictures no page draws any more are left out of the file.
pub fn remove(bytes: &[u8], password: Option<&str>, ids: &[String]) -> Result<(Vec<u8>, Removed), String> {
    let mut doc = open(bytes, password)?;
    let pages = pages(&doc);
    let mut want: HashSet<&str> = ids.iter().map(String::as_str).collect();
    let mut done = Removed::default();
    for (i, page) in pages.iter().enumerate() {
        let prefix = format!("{}:", i + 1);
        if !want.iter().any(|id| id.starts_with(&prefix)) {
            continue;
        }
        let (found, parsed) = read_page(&doc, page, i)?;
        let pieces = page_content(&doc, page)?;
        let mut edits: Vec<(std::ops::Range<usize>, Vec<u8>)> = Vec::new();
        let mut dropped_annots: HashSet<usize> = HashSet::new();
        let mut dropped_names: HashSet<Vec<u8>> = HashSet::new();
        for f in &found {
            if !want.remove(f.obj.id.as_str()) {
                continue;
            }
            match f.obj.kind {
                Kind::Image => done.images += 1,
                Kind::Graphic => done.graphics += 1,
                Kind::Text => done.texts += 1,
                Kind::Link => done.links += 1,
                Kind::Stamp => done.stamps += 1,
            }
            if matches!(f.obj.kind, Kind::Link | Kind::Stamp) {
                if let Some(k) = f.obj.id.rsplit_once(":a").and_then(|(_, k)| k.parse().ok()) {
                    dropped_annots.insert(k);
                }
                continue;
            }
            for &o in &f.ops {
                let Some(op) = parsed.ops.get(o) else { continue };
                // A `'` or `"` also moves to the next line: keep the move.
                let keep = if op.is("'") || op.is("\"") { b" T* ".to_vec() } else { b" ".to_vec() };
                edits.push((op.span.clone(), keep));
            }
            if let Some(n) = &f.xobject {
                dropped_names.insert(n.clone());
            }
        }
        edits.sort_by_key(|(r, _)| r.start);
        let mut dict = page.dict.clone();
        if !edits.is_empty() {
            let new = pieces.splice(edits);
            let refs: Vec<Object> = new.iter().map(|data| Object::Ref(doc.add(Stream::flate(Dict::new(), data)))).collect();
            dict.set(b"Contents".to_vec(), Object::Array(refs));
            // Pictures the page no longer draws leave its resources (a copy: they may be shared).
            let still: HashSet<Vec<u8>> =
                Pieces::join(&new).parse().ops.iter().filter(|o| o.is("Do")).filter_map(|o| o.last_name().map(<[u8]>::to_vec)).collect();
            let gone: Vec<Vec<u8>> = dropped_names.into_iter().filter(|n| !still.contains(n)).collect();
            if !gone.is_empty() {
                let mut res = page.resources.clone();
                if let Some(mut xo) = res.get(b"XObject").and_then(|o| doc.dict(o)) {
                    for n in &gone {
                        xo.remove(n);
                    }
                    res.set(b"XObject".to_vec(), Object::Dict(xo));
                }
                dict.set(b"Resources".to_vec(), Object::Dict(res));
            }
        }
        if !dropped_annots.is_empty()
            && let Some(a) = dict.get(b"Annots").map(|o| doc.resolve(o))
            && let Some(list) = a.as_array()
        {
            let kept: Vec<Object> = list.iter().enumerate().filter(|(k, _)| !dropped_annots.contains(k)).map(|(_, o)| o.clone()).collect();
            if kept.is_empty() {
                dict.remove(b"Annots");
            } else {
                dict.set(b"Annots".to_vec(), Object::Array(kept));
            }
        }
        doc.set(page.r, Object::Dict(dict));
    }
    done.not_found = want.into_iter().map(str::to_string).collect();
    done.not_found.sort();
    let out = write_full(&doc, &SaveOptions::default()).map_err(|e| e.to_string())?;
    Ok((out, done))
}

// ── preview ─────────────────────────────────────────────────────────────────────────────────

/// A page drawn for showing: premultiplied RGBA, `width × height` pixels.
#[derive(Clone, Debug)]
pub struct PagePicture {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Page `page` (0-based) drawn on white, its longer side `max_side` pixels at most.
pub fn render_page(bytes: &[u8], password: Option<&str>, page: usize, max_side: u32) -> Result<PagePicture, String> {
    use craft_pdf::raster::{self, Options, RenderCache};
    let pdf = craft_pdf::open_slice(bytes, password).map_err(|e| e.to_string())?;
    let pages = pdf.pages();
    let p = pages.iter().nth(page).ok_or_else(|| format!("the PDF has no page {}", page + 1))?;
    let one = Options { background: [255, 255, 255, 255], ..Options::default() };
    let [w, h] = raster::dimensions(p, &one).map_err(|e| e.to_string())?;
    let side = w.max(h).max(1) as f32;
    let scale = (max_side.clamp(16, raster::MAX_SIDE) as f32 / side).min(4.0);
    let opts = Options { scale: [scale, scale], ..one };
    let cache = RenderCache::new();
    let r = raster::render(p, &cache, &craft_pdf::interpret::InterpreterSettings::default(), &opts).map_err(|e| e.to_string())?;
    Ok(PagePicture { width: r.width, height: r.height, rgba: r.bytes().to_vec() })
}
