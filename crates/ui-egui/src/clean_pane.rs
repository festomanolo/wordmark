//! The Remove Watermarks pane: pick a PDF, see its pages with what looks like a watermark marked
//! (a logo pasted over a scan, a scanner app's footer, link areas, stamps), tick what to remove,
//! and save the PDF without it: everything else on each page stays as it was. What is ticked
//! teaches the model, so the next PDF from the same source is marked without asking. The PDF can
//! also be opened rebuilt as an editable document, and the open document cleaned. A whole folder
//! of PDFs can be cleaned too ([`crate::clean_batch`]). Every action runs a `clean.*` command
//! ([`wordcraft_engine::cmd::clean`]) or the work behind one.

use craft::behavior::{ColumnKey, ColumnWidth, EditEvent, Input, Metrics, Sense, keyed};
use craft::theme::variant::Tone;
use craft::ui::buttons::Button;
use craft::ui::choice::Checkbox;
use craft::ui::contract::{
    Bind, CollectionEvent, ColumnSpec, Common, Component, Cx, Entry, MeasureCx, PaintUi, Props, Role, SelectMode, Show as _, ShowCollection as _,
    ShowContainer as _, Sizing, SpaceAction, ToggleColumn,
};
use craft::ui::display::Text;
use craft::ui::layout_components::Row;
use craft::ui::list::{List, ListRow, ToggleCell, ToggleState};
use craft::ui::notices::Notice;
use craft::ui::pickers::Select;
use craft::ui::primitives::{PaintShape, StrokeWidth, TextureCache};
use craft::ui::runtime::env::Label;
use craft::ui::sections::Section;
use craft::ui::style::{ComponentStyle, StyleColor, StyleCx};
use egui::{CornerRadius, Pos2, Rect, Ui};
use serde_json::{Value, json};

use crate::WordApp;

/// The list's check column.
const CHECK: ColumnKey = ColumnKey::new("watermark");
/// The list's text column.
const TEXT: ColumnKey = ColumnKey::new("text");
/// Rows shown before the list scrolls (the pane scrolls too, so the list is capped).
const ROWS_SHOWN: u16 = 10;
/// The preview's longest side is rendered in steps of this many pixels, so a pane resized by a
/// pixel doesn't draw the page again.
const PREVIEW_STEP: f32 = 200.0;
const PREVIEW_MAX: f32 = 1800.0;

/// What to do with web addresses, with their `clean.*` names.
const URL_MODES: [(&str, &str); 3] = [("remove", "Remove them"), ("unlink", "Keep the text, remove the link"), ("keep", "Keep them")];

/// One text found by the text reader (for the editable document).
#[derive(Clone, Debug)]
pub(crate) struct Candidate {
    pub text: String,
}

/// Things that are the same on different pages (a footer on every page), listed as one row.
#[derive(Clone, Debug)]
pub(crate) struct Group {
    /// The object ids (`clean.scan` `objects[].id`), one per page it is on.
    pub ids: Vec<String>,
    /// `image`, `graphic`, `text`, `link` or `stamp`.
    pub kind: String,
    pub text: String,
    /// 1-based pages, ascending.
    pub pages: Vec<u64>,
    /// Its box on each of those pages (points, top-left origin), with the page.
    pub boxes: Vec<(u64, [f32; 4])>,
    pub score: f64,
    pub decision: String,
}

/// The pane's own state.
#[derive(Clone, Debug, Default)]
pub(crate) struct CleanPane {
    /// The pane is shown.
    pub open: bool,
    /// The PDF being looked at.
    pub path: String,
    /// Its pages, once scanned.
    pub pages: u64,
    /// Each page's size as shown, points.
    pub page_sizes: Vec<[f32; 2]>,
    /// Pages that are pictures with no text (OCR needed for an editable document).
    pub needs_ocr: Vec<u64>,
    pub groups: Vec<Group>,
    /// Ticked for removal, by group.
    pub picked: Vec<bool>,
    /// Texts the text reader found, for the editable document.
    pub candidates: Vec<Candidate>,
    /// The page the preview shows (0-based).
    pub view_page: usize,
    /// Counts scans, so a scan of the same file again draws its pages again.
    pub serial: u64,
    /// Which [`URL_MODES`] entry.
    pub urls: usize,
    /// Also drop text the model isn't sure of (editable document).
    pub drop_unsure: bool,
    /// The last result, and whether it was a failure.
    pub message: String,
    pub failed: bool,
    /// Cleaning a whole folder.
    pub batch: crate::clean_batch::Batch,
    /// The folder copy this PDF's clean copy goes to, when it was opened from a folder run to
    /// be checked.
    pub save_target: Option<String>,
    /// What Clean This Document did, shown under its button.
    pub doc_message: String,
}

impl CleanPane {
    fn say(&mut self, message: String, failed: bool) {
        self.message = message;
        self.failed = failed;
    }

    fn urls(&self) -> &'static str {
        URL_MODES.get(self.urls).map_or("remove", |m| m.0)
    }

    /// Take a `clean.scan` result: the groups, ticked where the model would remove.
    fn take_scan(&mut self, r: &Value) {
        self.pages = r["pages"].as_u64().unwrap_or(0);
        self.needs_ocr = r["needsOcr"].as_array().map(|a| a.iter().filter_map(Value::as_u64).collect()).unwrap_or_default();
        self.page_sizes = r["pageSizes"]
            .as_array()
            .map(|a| a.iter().map(|s| [s[0].as_f64().unwrap_or(1.0) as f32, s[1].as_f64().unwrap_or(1.0) as f32]).collect())
            .unwrap_or_default();
        self.candidates = r["candidates"]
            .as_array()
            .map(|a| a.iter().map(|c| Candidate { text: c["text"].as_str().unwrap_or_default().to_string() }).collect())
            .unwrap_or_default();
        self.groups = groups(r["objects"].as_array().map(Vec::as_slice).unwrap_or_default());
        self.picked = self.groups.iter().map(|g| g.decision == "remove").collect();
        self.view_page =
            self.groups.iter().zip(&self.picked).find(|(_, on)| **on).and_then(|(g, _)| g.pages.first()).map_or(0, |p| p.saturating_sub(1) as usize);
        self.serial = self.serial.wrapping_add(1);
    }
}

/// The objects worth a row, grouped. Body text the model keeps and the page's own picture (the
/// scan) are left out: ticking them would empty the page.
fn groups(objects: &[Value]) -> Vec<Group> {
    let mut out: Vec<Group> = Vec::new();
    let mut at: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for o in objects {
        let kind = o["kind"].as_str().unwrap_or_default();
        let decision = o["decision"].as_str().unwrap_or("keep");
        let page_picture = o["reasons"].as_array().is_some_and(|r| r.iter().any(|x| x.as_str() == Some("page picture")));
        let invisible = o["reasons"].as_array().is_some_and(|r| r.iter().any(|x| x.as_str().is_some_and(|s| s.starts_with("invisible"))));
        if page_picture || invisible || (kind == "text" && decision == "keep") {
            continue;
        }
        let (Some(id), Some(page)) = (o["id"].as_str(), o["page"].as_u64()) else { continue };
        let b = &o["bbox"];
        let bbox = [0, 1, 2, 3].map(|i| b[i].as_f64().unwrap_or(0.0) as f32);
        let group = o["group"].as_str().unwrap_or(id).to_string();
        let score = o["score"].as_f64().unwrap_or(0.0);
        let g = match at.get(&group) {
            Some(&i) => &mut out[i],
            None => {
                at.insert(group, out.len());
                out.push(Group {
                    ids: Vec::new(),
                    kind: kind.to_string(),
                    text: o["text"].as_str().unwrap_or_default().to_string(),
                    pages: Vec::new(),
                    boxes: Vec::new(),
                    score,
                    decision: decision.to_string(),
                });
                out.last_mut().expect("just pushed")
            }
        };
        g.ids.push(id.to_string());
        if !g.pages.contains(&page) {
            g.pages.push(page);
        }
        g.boxes.push((page, bbox));
        if score > g.score {
            g.score = score;
            g.decision = decision.to_string();
        }
    }
    for g in &mut out {
        g.pages.sort_unstable();
    }
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then(b.pages.len().cmp(&a.pages.len())));
    out
}

/// The model file, where the app keeps it (`None` on the web: nothing is learned there).
fn model_path(app: &WordApp) -> Option<String> {
    app.services.watermark_model.as_ref().map(|p| p.to_string_lossy().into_owned())
}

/// The params every `clean.*` call shares.
fn base_params(app: &WordApp, st: &CleanPane) -> Value {
    let mut p = json!({"path": st.path, "urls": st.urls()});
    if let Some(m) = model_path(app).filter(|m| std::path::Path::new(m).exists()) {
        p["model"] = json!(m);
    }
    p
}

/// The ids of the ticked groups, and of the unticked ones the model would remove (the person
/// overruled it).
fn picked_ids(st: &CleanPane) -> (Vec<String>, Vec<String>) {
    let (mut remove, mut keep) = (Vec::new(), Vec::new());
    for (g, on) in st.groups.iter().zip(&st.picked) {
        if *on {
            remove.extend(g.ids.iter().cloned());
        } else if g.decision == "remove" {
            keep.extend(g.ids.iter().cloned());
        }
    }
    (remove, keep)
}

/// Scan the PDF at `path` and show what was found (after the file dialog, or Scan Again).
pub(crate) fn scan(app: &mut WordApp, path: String) {
    let mut st = std::mem::take(&mut app.clean);
    st.open = true;
    st.path = path;
    st.save_target = None;
    st.groups.clear();
    st.picked.clear();
    st.candidates.clear();
    let p = base_params(app, &st);
    match app.run("clean.scan", p) {
        Ok(r) => {
            st.take_scan(&r);
            let found = st.picked.iter().filter(|p| **p).count();
            let unsure = st.groups.iter().filter(|g| g.decision == "review").count();
            let msg = crate::i18n::fmt(
                tl!("Watermarks found: {found}. Not sure: {unsure}. Tick everything to remove, then save."),
                &[("found", &found.to_string()), ("unsure", &unsure.to_string())],
            );
            app.status(msg.clone());
            st.say(msg, false);
        }
        Err(e) => st.say(e, true),
    }
    app.clean = st;
}

/// The pane body. The pane area scrolls it, so it brings no scroll area of its own.
pub(crate) fn show(app: &mut WordApp, ui: &mut Ui) {
    let mut st = std::mem::take(&mut app.clean);
    pdf_section(app, ui, &mut st);
    document_section(app, ui, &mut st);
    if !st.message.is_empty() {
        Notice::new(if st.failed { Tone::Bad } else { Tone::Neutral }, st.message.clone()).show(ui);
    }
    app.clean = st;
}

fn pdf_section(app: &mut WordApp, ui: &mut Ui, st: &mut CleanPane) {
    Section::fixed(tl!("1. Choose a PDF")).show(ui, |ui, _| {
        Text::new(tl!("Pictures, text and links drawn over its pages are found and marked.")).show(ui);
        Row::new().show(ui, |ui, _| {
            if Button::new(tl!("Choose PDF…")).show(ui).clicked() {
                app.clean = std::mem::take(st);
                app.pick_watermarked_pdf();
                *st = std::mem::take(&mut app.clean);
            }
            if !st.path.is_empty() && Button::new(tl!("Scan Again")).show(ui).clicked() {
                let path = st.path.clone();
                app.clean = std::mem::take(st);
                scan(app, path);
                *st = std::mem::take(&mut app.clean);
            }
        });
        if !st.path.is_empty() {
            Text::new(crate::i18n::fmt(tl!("{name}, {pages} pages"), &[("name", &file_name(&st.path)), ("pages", &st.pages.to_string())])).show(ui);
        }
    });
    if st.path.is_empty() || st.page_sizes.is_empty() {
        return;
    }
    Section::fixed(tl!("2. Check the pages")).show(ui, |ui, _| {
        page_nav(ui, st);
        page_preview(app, ui, st);
        Text::new(tl!("Crossed boxes are removed. Dashed boxes: not sure, kept unless you tick them.")).show(ui);
    });
    Section::fixed(tl!("3. Tick what to remove")).show(ui, |ui, _| {
        if st.groups.is_empty() {
            Text::new(tl!("Nothing is drawn over these pages.")).show(ui);
        } else {
            Row::new().show(ui, |ui, _| {
                if Button::new(tl!("All")).show(ui).clicked() {
                    st.picked.iter_mut().for_each(|p| *p = true);
                }
                if Button::new(tl!("None")).show(ui).clicked() {
                    st.picked.iter_mut().for_each(|p| *p = false);
                }
            });
            group_list(ui, st);
        }
    });
    Section::fixed(tl!("4. Save")).show(ui, |ui, _| {
        Text::new(tl!("A new PDF without the ticked items. The rest of each page stays exactly as it was.")).show(ui);
        let learnable = model_path(app).is_some();
        Row::new().show(ui, |ui, _| {
            if Button::new(tl!("Save Clean PDF…")).show(ui).clicked() {
                app.clean = std::mem::take(st);
                app.pick_clean_pdf_out();
                *st = std::mem::take(&mut app.clean);
            }
            // Scanned again after learning, so the list shows what the detector now thinks.
            if Button::new(tl!("Learn Only")).enabled(learnable).show(ui).clicked() && learn(app, st) {
                let path = st.path.clone();
                app.clean = std::mem::take(st);
                scan(app, path);
                *st = std::mem::take(&mut app.clean);
            }
        });
        if !learnable {
            Text::new(tl!("Picks can't be remembered here: they apply to this PDF only.")).show(ui);
        }
    });
    Section::fixed(tl!("Or open as an editable document")).show(ui, |ui, _| {
        Text::new(tl!("Rebuilds the PDF's text as paragraphs you can edit, without the ticked text. Pictures are not carried over.")).show(ui);
        if !st.needs_ocr.is_empty() {
            let pages = st.needs_ocr.iter().map(u64::to_string).collect::<Vec<_>>().join(", ");
            Notice::new(
                Tone::Warn,
                crate::i18n::fmt(
                    tl!("Pages {pages} are scanned pictures with no text: they come out empty. Save a clean PDF instead."),
                    &[("pages", &pages)],
                ),
            )
            .show(ui);
        }
        url_choice(ui, st);
        Checkbox::new(Bind::bound(&mut st.drop_unsure)).label(tl!("Also remove unticked text it is unsure of")).show(ui);
        if Button::new(tl!("Open as Editable Document")).show(ui).clicked() {
            open_cleaned(app, st);
        }
    });
}

fn file_name(path: &str) -> String {
    std::path::Path::new(path).file_name().map_or(path.to_string(), |n| n.to_string_lossy().into_owned())
}

/// Previous / Next and which page is shown.
fn page_nav(ui: &mut Ui, st: &mut CleanPane) {
    let total = st.page_sizes.len();
    Row::new().show(ui, |ui, _| {
        if Button::new(tl!("Previous Page")).enabled(st.view_page > 0).show(ui).clicked() {
            st.view_page = st.view_page.saturating_sub(1);
        }
        if Button::new(tl!("Next Page")).enabled(st.view_page + 1 < total).show(ui).clicked() {
            st.view_page = (st.view_page + 1).min(total.saturating_sub(1));
        }
    });
    Text::new(crate::i18n::fmt(tl!("Page {n} of {total}"), &[("n", &(st.view_page + 1).to_string()), ("total", &total.to_string())])).show(ui);
}

/// How a box on the preview is marked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mark {
    /// Ticked: removed when saved.
    Remove,
    /// Not ticked, and the detector is not sure.
    Unsure,
    /// Not ticked.
    Keep,
}

fn page_preview(app: &mut WordApp, ui: &mut Ui, st: &mut CleanPane) {
    let page = st.view_page.min(st.page_sizes.len().saturating_sub(1));
    let [pw, ph] = st.page_sizes.get(page).copied().unwrap_or([1.0, 1.0]);
    let (pw, ph) = (pw.max(1.0), ph.max(1.0));
    let ppp = ui.ctx().pixels_per_point();
    let side = ((ui.available_width() * ppp * (pw.max(ph) / pw) / PREVIEW_STEP).ceil() * PREVIEW_STEP).clamp(PREVIEW_STEP, PREVIEW_MAX) as u32;
    let path = st.path.clone();
    let mut failed = None;
    let texture =
        TextureCache::texture(ui.ctx(), ("clean-preview", path.clone(), st.serial, page, side), || match wordcraft_engine::cmd::clean::page_preview(
            &path, None, page, side,
        ) {
            Ok(pic) => egui::ColorImage::from_rgba_premultiplied([pic.width as usize, pic.height as usize], &pic.rgba),
            Err(e) => {
                failed = Some(e);
                egui::ColorImage::filled([1, 1], egui::Color32::TRANSPARENT)
            }
        });
    if let Some(e) = failed {
        app.status(e.clone());
        st.say(e, true);
    }
    let shown = (page + 1) as u64;
    let mut boxes = Vec::new();
    let mut marked = 0;
    for (g, on) in st.groups.iter().zip(&st.picked) {
        let mark = if *on {
            Mark::Remove
        } else if g.decision == "review" {
            Mark::Unsure
        } else {
            Mark::Keep
        };
        for (p, b) in &g.boxes {
            if *p == shown {
                boxes.push(([b[0] / pw, b[1] / ph, b[2] / pw, b[3] / ph], mark));
                marked += usize::from(mark == Mark::Remove);
            }
        }
    }
    let label = crate::i18n::fmt(tl!("Page {n}: {marked} items to remove"), &[("n", &shown.to_string()), ("marked", &marked.to_string())]);
    PagePreview::new(label, texture, ph / pw, boxes).show(ui);
}

/// A page of the PDF with a box round each thing found on it: crossed where it is removed,
/// dashed where the detector is not sure, thin where it is kept. The marks differ in shape as
/// well as colour.
#[derive(Component, Props)]
struct PagePreview {
    #[common]
    common: Common,
    texture: egui::TextureId,
    /// Height over width.
    aspect: f32,
    /// Boxes as shares of the page `[x0 y0 x1 y1]`, each with its mark.
    boxes: Vec<([f32; 4], Mark)>,
}

impl PagePreview {
    fn new(label: impl Into<Label>, texture: egui::TextureId, aspect: f32, boxes: Vec<([f32; 4], Mark)>) -> Self {
        PagePreview { common: Common::new(), texture, aspect, boxes }.label(label)
    }
}

#[derive(Clone, Debug, PartialEq, ComponentStyle)]
struct PagePreviewStyle {
    #[style(metrics)]
    metrics: Metrics,
    frame: StyleColor,
    remove: StyleColor,
    unsure: StyleColor,
    keep: StyleColor,
}

impl Component for PagePreview {
    const ROLE: Role = Role::IMAGE;
    const PARTS: bool = false;
    const STOPS: bool = false;
    const INPUT: Input = Input::Display;
    const SHOWS_LABEL: bool = false;
    const OVERLAYS: bool = false;
    type Style = PagePreviewStyle;
    type Output = egui::Response;

    fn style(cx: &StyleCx<'_>, _: PagePreviewStyleKey) -> PagePreviewStyle {
        let c = &cx.tokens.color;
        PagePreviewStyle {
            metrics: Metrics::new(),
            frame: cx.color(c.line.frame),
            remove: cx.color(c.status.bad.solid),
            unsure: cx.color(c.status.warn.solid),
            keep: cx.color(c.accent.solid),
        }
    }

    fn measure(&mut self, cx: &MeasureCx<'_, Self>) -> egui::Vec2 {
        let w = cx.room.filter(|w| w.is_finite() && *w > 0.0).unwrap_or(280.0);
        egui::vec2(w, w * self.aspect)
    }

    fn sense(&self) -> Sense {
        Sense::empty()
    }

    fn paint(&mut self, ui: &mut PaintUi<'_>, cx: &Cx<'_, Self>) {
        let style = &cx.style;
        let r = cx.content_rect;
        let draw = ui.draw();
        draw.texture(r, self.texture, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)));
        draw.stroke(&PaintShape::Rect(r, CornerRadius::ZERO), StrokeWidth::Hairline, style.frame);
        let at = |x: f32, y: f32| Pos2::new(r.min.x + x * r.width(), r.min.y + y * r.height());
        for (b, mark) in &self.boxes {
            let bx = Rect::from_min_max(at(b[0], b[1]), at(b[2], b[3])).expand(2.0).intersect(r);
            let corners = [bx.left_top(), bx.right_top(), bx.right_bottom(), bx.left_bottom(), bx.left_top()];
            match mark {
                Mark::Remove => {
                    draw.stroke(&PaintShape::Rect(bx, CornerRadius::ZERO), StrokeWidth::Thick, style.remove);
                    draw.stroke(&PaintShape::Path(vec![bx.left_top(), bx.right_bottom()]), StrokeWidth::Thin, style.remove);
                    draw.stroke(&PaintShape::Path(vec![bx.right_top(), bx.left_bottom()]), StrokeWidth::Thin, style.remove);
                }
                Mark::Unsure => draw.dashed(&corners, draw.dashes(), StrokeWidth::Medium, style.unsure),
                Mark::Keep => draw.stroke(&PaintShape::Rect(bx, CornerRadius::ZERO), StrokeWidth::Hairline, style.keep),
            }
        }
    }

    fn finish(self, cx: Cx<'_, Self>) -> egui::Response {
        cx.response.clone()
    }
}

/// The groups, each with a tick, what it is and where.
fn group_list(ui: &mut Ui, st: &mut CleanPane) {
    let rows: Vec<String> = st.groups.iter().map(|g| row_label(g, &st.page_sizes)).collect();
    let positions: Vec<usize> = (0..rows.len()).collect();
    let source = keyed(&positions, |at: &usize| *at);
    let columns = vec![ColumnSpec::toggle(CHECK, Label::Empty, ToggleColumn::default()), ColumnSpec::new(TEXT, Label::Empty, ColumnWidth::Content)];
    let picked = &mut st.picked;
    let out = List::<usize>::new(tl!("Tick what to remove"))
        .select(SelectMode::Single)
        .space(SpaceAction::ToggleCell)
        .header(false)
        .sizing(Sizing::Content)
        .max_rows(ROWS_SHOWN)
        .columns(columns)
        .show_items(ui, &source, &mut |at, icx| {
            let Some(label) = rows.get(*at) else { return ListRow::new(Label::Empty) };
            let on = picked.get(*at).copied().unwrap_or(false);
            ListRow::new(label.clone()).cells(icx.cells()).toggle(ToggleCell::check(CHECK, ToggleState::of(on), label.clone()))
        });
    for event in &out.events {
        if let CollectionEvent::Toggle(EditEvent::Commit { after, .. } | EditEvent::Set { after, .. }) = event
            && let Some(p) = picked.get_mut(after.key)
        {
            *p = after.on;
            // Show the page it is on, so the tick can be checked on the page.
            if let Some(first) = st.groups.get(after.key).and_then(|g| g.pages.first()) {
                st.view_page = first.saturating_sub(1) as usize;
            }
        }
    }
}

/// Where on the page a box sits, in words: "bottom right", "middle".
fn place(b: [f32; 4], size: [f32; 2]) -> String {
    let (x, y) = ((b[0] + b[2]) / 2.0 / size[0].max(1.0), (b[1] + b[3]) / 2.0 / size[1].max(1.0));
    let row = if y < 1.0 / 3.0 {
        0
    } else if y < 2.0 / 3.0 {
        1
    } else {
        2
    };
    let col = if x < 1.0 / 3.0 {
        0
    } else if x < 2.0 / 3.0 {
        1
    } else {
        2
    };
    let words = [["top left", "top", "top right"], ["left", "middle", "right"], ["bottom left", "bottom", "bottom right"]];
    tl!(words[row][col]).to_string()
}

/// "92% watermark · Picture, middle · page 1": the verdict first, in words as well as a number,
/// so a narrow pane cuts off the description rather than the verdict.
fn row_label(g: &Group, sizes: &[[f32; 2]]) -> String {
    let pages = match (g.pages.first(), g.pages.last()) {
        (Some(a), Some(b)) if a != b => crate::i18n::fmt(tl!("pages {a}–{b}"), &[("a", &a.to_string()), ("b", &b.to_string())]),
        (Some(a), _) => crate::i18n::fmt(tl!("page {a}"), &[("a", &a.to_string())]),
        _ => String::new(),
    };
    let verdict = match g.decision.as_str() {
        "remove" => tl!("watermark"),
        "review" => tl!("not sure"),
        _ => tl!("kept"),
    };
    let where_ = g.boxes.first().map(|(p, b)| place(*b, sizes.get(p.saturating_sub(1) as usize).copied().unwrap_or([1.0, 1.0]))).unwrap_or_default();
    let text: String = g.text.chars().take(60).collect();
    let what = match g.kind.as_str() {
        "image" => crate::i18n::fmt(tl!("Picture, {place}"), &[("place", &where_)]),
        "graphic" => crate::i18n::fmt(tl!("Graphic, {place}"), &[("place", &where_)]),
        "link" if text.is_empty() => crate::i18n::fmt(tl!("Link area, {place}"), &[("place", &where_)]),
        "link" => crate::i18n::fmt(tl!("Link to {url}"), &[("url", &text)]),
        "stamp" => crate::i18n::fmt(tl!("Stamp \"{text}\""), &[("text", &text)]),
        _ if text.is_empty() => crate::i18n::fmt(tl!("Text, {place}"), &[("place", &where_)]),
        _ => format!("\u{201c}{text}\u{201d}"),
    };
    format!("{:.0}% {verdict} · {what} · {pages}", g.score * 100.0)
}

fn url_choice(ui: &mut Ui, st: &mut CleanPane) {
    let entries: Vec<Entry<String>> = URL_MODES.iter().map(|(id, label)| Entry::new((*id).to_string(), tl!(label))).collect();
    let picked = Select::new(tl!("Web addresses:"), Bind::given(st.urls().to_string()), &entries).show(ui).value().cloned();
    if let Some(id) = picked
        && let Some(at) = URL_MODES.iter().position(|m| m.0 == id)
    {
        st.urls = at;
    }
}

/// Teach the model the picks and keep it; `false` when it failed (said in the pane).
fn learn(app: &mut WordApp, st: &mut CleanPane) -> bool {
    let Some(model) = model_path(app) else { return false };
    let (remove, keep) = picked_ids(st);
    if remove.is_empty() && keep.is_empty() {
        return true;
    }
    let mut p = base_params(app, st);
    p["watermarkObjects"] = json!(remove);
    p["contentObjects"] = json!(keep);
    p["model"] = json!(model);
    match app.run("clean.learn", p) {
        Ok(r) => {
            let n = r["learned"].as_u64().unwrap_or(0);
            let msg = crate::i18n::fmt(tl!("Picks learned: {n}."), &[("n", &n.to_string())]);
            app.status(msg.clone());
            st.say(msg, false);
            true
        }
        Err(e) => {
            st.say(e, true);
            false
        }
    }
}

/// Keep the picks (where a model is kept), then write the PDF without the ticked items to `out`
/// (after the Save dialog).
pub(crate) fn save_to(app: &mut WordApp, out: String) {
    let mut st = std::mem::take(&mut app.clean);
    if model_path(app).is_none() || learn(app, &mut st) {
        let (remove, keep) = picked_ids(&st);
        let mut p = base_params(app, &st);
        p["out"] = json!(out);
        p["remove"] = json!(remove);
        p["keep"] = json!(keep);
        match app.run("clean.savePdf", p) {
            Ok(r) => {
                let n = |k: &str| r[k].as_u64().unwrap_or(0).to_string();
                let msg = crate::i18n::fmt(
                    tl!("Saved {name}. Removed: {pictures} pictures, {texts} texts, {links} links, {stamps} stamps."),
                    &[
                        ("name", &file_name(&out)),
                        ("pictures", &(r["images"].as_u64().unwrap_or(0) + r["graphics"].as_u64().unwrap_or(0)).to_string()),
                        ("texts", &n("texts")),
                        ("links", &n("links")),
                        ("stamps", &n("stamps")),
                    ],
                );
                app.status(msg.clone());
                st.say(msg, false);
            }
            Err(e) => st.say(e, true),
        }
    }
    app.clean = st;
}

/// Keep the picks (where a model is kept), then open the PDF rebuilt without the watermarks.
/// The ticked texts also go with the call (the ones the text reader found too), so they apply
/// even where nothing is kept.
fn open_cleaned(app: &mut WordApp, st: &mut CleanPane) {
    if model_path(app).is_some() && !learn(app, st) {
        return;
    }
    let same = |a: &str, b: &str| a.trim().eq_ignore_ascii_case(b.trim());
    let known = |t: &str| st.candidates.iter().any(|c| same(&c.text, t));
    let pairs = || st.groups.iter().zip(&st.picked).filter(|(g, _)| g.kind == "text" && !g.text.is_empty() && known(&g.text));
    let watermarks: Vec<&str> = pairs().filter(|(_, on)| **on).map(|(g, _)| g.text.as_str()).collect();
    let content: Vec<&str> = pairs().filter(|(g, on)| !**on && g.decision == "remove").map(|(g, _)| g.text.as_str()).collect();
    let mut p = base_params(app, st);
    p["watermarks"] = json!(watermarks);
    p["content"] = json!(content);
    p["limit"] = json!(st.candidates.len().max(1));
    p["dropFlagged"] = json!(st.drop_unsure);
    match app.run("clean.openPdf", p) {
        Ok(r) => {
            let removed = r["removed"].as_u64().unwrap_or(0);
            let urls = r["urlsRemoved"].as_u64().unwrap_or(0);
            let unsure = r["flagged"].as_array().map_or(0, Vec::len);
            let mut msg = crate::i18n::fmt(
                tl!("Opened. Watermark texts removed: {removed}. Web addresses removed: {urls}."),
                &[("removed", &removed.to_string()), ("urls", &urls.to_string())],
            );
            if unsure > 0 && !st.drop_unsure {
                msg.push(' ');
                msg.push_str(&crate::i18n::fmt(tl!("Unsure texts kept: {n}."), &[("n", &unsure.to_string())]));
            }
            if r["needsOcr"].as_array().is_some_and(|a| !a.is_empty()) {
                msg.push(' ');
                msg.push_str(tl!("Some pages had no text to read."));
            }
            app.status(msg.clone());
            st.say(msg, false);
        }
        Err(e) => st.say(e, true),
    }
}

fn document_section(app: &mut WordApp, ui: &mut Ui, st: &mut CleanPane) {
    Section::fixed(tl!("This document")).show(ui, |ui, _| {
        Text::new(tl!("Remove its watermark, the watermark texts the detector has learned, and its links and web addresses.")).show(ui);
        if Button::new(tl!("Clean This Document")).show(ui).clicked() {
            let mut p = json!({"urls": st.urls()});
            if let Some(m) = model_path(app).filter(|m| std::path::Path::new(m).exists()) {
                p["model"] = json!(m);
            }
            match app.run("clean.document", p) {
                Ok(r) => {
                    let n = r["learnedTextRemoved"].as_u64().unwrap_or(0) + u64::from(r["watermarkRemoved"].as_bool().unwrap_or(false));
                    let links = r["linksUnlinked"].as_u64().unwrap_or(0) + r["urlsRemoved"].as_u64().unwrap_or(0);
                    let msg = crate::i18n::fmt(
                        tl!("Watermarks removed: {n}. Links and web addresses removed: {links}."),
                        &[("n", &n.to_string()), ("links", &links.to_string())],
                    );
                    app.status(msg.clone());
                    st.say(msg, false);
                }
                Err(e) => st.say(e, true),
            }
        }
    });
}
