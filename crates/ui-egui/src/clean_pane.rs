//! The Remove Watermarks pane: pick a PDF, tick the texts that are its watermarks, and open it
//! rebuilt as an editable document without them. What is ticked teaches the model, so the next
//! PDF from the same source is cleaned without asking. It also cleans the open document. Every
//! action runs a `clean.*` command ([`wordcraft_engine::cmd::clean`]).

use craft::behavior::{ColumnKey, ColumnWidth, EditEvent, keyed};
use craft::theme::variant::Tone;
use craft::ui::buttons::Button;
use craft::ui::choice::Checkbox;
use craft::ui::contract::{
    Bind, CollectionEvent, ColumnSpec, Entry, Props as _, SelectMode, Show as _, ShowCollection as _, ShowContainer as _, Sizing, SpaceAction,
    ToggleColumn,
};
use craft::ui::display::Text;
use craft::ui::layout_components::Row;
use craft::ui::list::{List, ListRow, ToggleCell, ToggleState};
use craft::ui::notices::Notice;
use craft::ui::pickers::Select;
use craft::ui::runtime::env::Label;
use craft::ui::sections::Section;
use egui::Ui;
use serde_json::{Value, json};

use crate::WordApp;

/// The candidate list's check column.
const CHECK: ColumnKey = ColumnKey::new("watermark");
/// The candidate list's text column.
const TEXT: ColumnKey = ColumnKey::new("text");
/// Candidate rows shown before the list scrolls (the pane scrolls too, so the list is capped).
const ROWS_SHOWN: u16 = 12;

/// What to do with web addresses, with their `clean.*` names.
const URL_MODES: [(&str, &str); 3] = [("remove", "Remove them"), ("unlink", "Keep the text, remove the link"), ("keep", "Keep them")];

/// One text found in the PDF.
#[derive(Clone, Debug)]
pub(crate) struct Candidate {
    pub text: String,
    pub pages: Vec<u64>,
    pub score: f64,
    /// `remove`, `review` or `keep`: what the model would do.
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
    /// Pages that are pictures with no text (OCR needed).
    pub needs_ocr: Vec<u64>,
    pub candidates: Vec<Candidate>,
    /// Ticked as a watermark, by candidate.
    pub picked: Vec<bool>,
    /// Which [`URL_MODES`] entry.
    pub urls: usize,
    /// Also drop text the model isn't sure of.
    pub drop_unsure: bool,
    /// The last result, and whether it was a failure.
    pub message: String,
    pub failed: bool,
}

impl CleanPane {
    fn say(&mut self, message: String, failed: bool) {
        self.message = message;
        self.failed = failed;
    }

    fn urls(&self) -> &'static str {
        URL_MODES.get(self.urls).map_or("remove", |m| m.0)
    }

    /// Take a `clean.scan` result: the list, ticked where the model would remove.
    fn take_scan(&mut self, r: &Value) {
        self.pages = r["pages"].as_u64().unwrap_or(0);
        self.needs_ocr = r["needsOcr"].as_array().map(|a| a.iter().filter_map(Value::as_u64).collect()).unwrap_or_default();
        self.candidates = r["candidates"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|c| Candidate {
                        text: c["text"].as_str().unwrap_or_default().to_string(),
                        pages: c["pages"].as_array().map(|p| p.iter().filter_map(Value::as_u64).collect()).unwrap_or_default(),
                        score: c["score"].as_f64().unwrap_or(0.0),
                        decision: c["decision"].as_str().unwrap_or("keep").to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.picked = self.candidates.iter().map(|c| c.decision == "remove").collect();
    }
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

/// The person's picks as `clean.*` params: ticked texts are watermarks; a text the detector would
/// remove that was unticked is content.
fn picks(st: &CleanPane, p: &mut Value) {
    let pairs = || st.candidates.iter().zip(&st.picked);
    let watermarks: Vec<&str> = pairs().filter(|(_, on)| **on).map(|(c, _)| c.text.as_str()).collect();
    let content: Vec<&str> = pairs().filter(|(c, on)| !**on && c.decision == "remove").map(|(c, _)| c.text.as_str()).collect();
    p["watermarks"] = json!(watermarks);
    p["content"] = json!(content);
    p["limit"] = json!(st.candidates.len());
}

/// Scan the PDF at `path` and show what was found (after the file dialog, or Scan Again).
pub(crate) fn scan(app: &mut WordApp, path: String) {
    let mut st = std::mem::take(&mut app.clean);
    st.open = true;
    st.path = path;
    st.candidates.clear();
    st.picked.clear();
    let p = base_params(app, &st);
    match app.run("clean.scan", p) {
        Ok(r) => {
            st.take_scan(&r);
            let found = st.picked.iter().filter(|p| **p).count();
            let unsure = st.candidates.iter().filter(|c| c.decision == "review").count();
            let msg = crate::i18n::fmt(
                tl!("Watermarks found: {found}. Not sure: {unsure}. Tick every text that is a watermark."),
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
        Text::new(tl!("It opens as an editable document, without the watermarks you tick.")).show(ui);
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
            let name = std::path::Path::new(&st.path).file_name().map_or(st.path.clone(), |n| n.to_string_lossy().into_owned());
            Text::new(crate::i18n::fmt(tl!("{name}, {pages} pages"), &[("name", &name), ("pages", &st.pages.to_string())])).show(ui);
        }
        if !st.needs_ocr.is_empty() {
            let pages = st.needs_ocr.iter().map(u64::to_string).collect::<Vec<_>>().join(", ");
            Notice::new(
                Tone::Bad,
                crate::i18n::fmt(
                    tl!("Pages {pages} are scanned pictures with no text: run OCR on the PDF first, or they come out empty."),
                    &[("pages", &pages)],
                ),
            )
            .show(ui);
        }
    });
    if st.candidates.is_empty() {
        return;
    }
    Section::fixed(tl!("2. Tick the watermarks")).show(ui, |ui, _| {
        Text::new(tl!("Ticked texts are removed and taught to the detector. \"Not sure\" texts stay unless you tick them.")).show(ui);
        Row::new().show(ui, |ui, _| {
            if Button::new(tl!("All")).show(ui).clicked() {
                st.picked.iter_mut().for_each(|p| *p = true);
            }
            if Button::new(tl!("None")).show(ui).clicked() {
                st.picked.iter_mut().for_each(|p| *p = false);
            }
        });
        candidate_list(ui, st);
    });
    Section::fixed(tl!("3. Open without watermarks")).show(ui, |ui, _| {
        url_choice(ui, st);
        Checkbox::new(Bind::bound(&mut st.drop_unsure)).label(tl!("Also remove unticked text it is unsure of")).show(ui);
        let learnable = model_path(app).is_some();
        Row::new().show(ui, |ui, _| {
            if Button::new(tl!("Open Without Watermarks")).show(ui).clicked() {
                open_cleaned(app, st);
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
}

/// The candidates, each with a tick, its pages and how sure the detector is.
fn candidate_list(ui: &mut Ui, st: &mut CleanPane) {
    let rows: Vec<String> = st.candidates.iter().map(row_label).collect();
    let positions: Vec<usize> = (0..rows.len()).collect();
    let source = keyed(&positions, |at: &usize| *at);
    let columns = vec![ColumnSpec::toggle(CHECK, Label::Empty, ToggleColumn::default()), ColumnSpec::new(TEXT, Label::Empty, ColumnWidth::Content)];
    let picked = &mut st.picked;
    let out = List::<usize>::new(tl!("Tick the watermarks"))
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
        }
    }
}

/// "92% watermark · DRAFT COPY · pages 1–6": the verdict first, in words as well as a number, so a
/// narrow pane cuts off the text rather than the verdict.
fn row_label(c: &Candidate) -> String {
    let pages = match (c.pages.first(), c.pages.last()) {
        (Some(a), Some(b)) if a != b => crate::i18n::fmt(tl!("pages {a}–{b}"), &[("a", &a.to_string()), ("b", &b.to_string())]),
        (Some(a), _) => crate::i18n::fmt(tl!("page {a}"), &[("a", &a.to_string())]),
        _ => String::new(),
    };
    let verdict = match c.decision.as_str() {
        "remove" => tl!("watermark"),
        "review" => tl!("not sure"),
        _ => tl!("content"),
    };
    let text: String = c.text.chars().take(80).collect();
    format!("{text} · {pages} · {:.0}%, {verdict}", c.score * 100.0)
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
    let mut p = base_params(app, st);
    picks(st, &mut p);
    if p["watermarks"].as_array().is_some_and(Vec::is_empty) && p["content"].as_array().is_some_and(Vec::is_empty) {
        return true;
    }
    p["model"] = json!(model);
    match app.run("clean.learn", p) {
        Ok(r) => {
            let n = r["learned"].as_u64().unwrap_or(0);
            let msg = crate::i18n::fmt(tl!("Learned from {n} picks."), &[("n", &n.to_string())]);
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

/// Keep the picks (where a model is kept), then open the PDF rebuilt without the watermarks.
/// The picks also go with the call, so they apply even where nothing is kept.
fn open_cleaned(app: &mut WordApp, st: &mut CleanPane) {
    if model_path(app).is_some() && !learn(app, st) {
        return;
    }
    let mut p = base_params(app, st);
    picks(st, &mut p);
    p["dropFlagged"] = json!(st.drop_unsure);
    match app.run("clean.openPdf", p) {
        Ok(r) => {
            let removed = r["removed"].as_u64().unwrap_or(0);
            let urls = r["urlsRemoved"].as_u64().unwrap_or(0);
            let unsure = r["flagged"].as_array().map_or(0, Vec::len);
            let mut msg = crate::i18n::fmt(tl!("Opened without {removed} watermark texts and {urls} web addresses."), &[
                ("removed", &removed.to_string()),
                ("urls", &urls.to_string()),
            ]);
            if unsure > 0 && !st.drop_unsure {
                msg.push(' ');
                msg.push_str(&crate::i18n::fmt(tl!("{n} unsure texts were kept."), &[("n", &unsure.to_string())]));
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
                    let msg = crate::i18n::fmt(tl!("Removed {n} watermarks and {links} links or web addresses."), &[
                        ("n", &n.to_string()),
                        ("links", &links.to_string()),
                    ]);
                    app.status(msg.clone());
                    st.say(msg, false);
                }
                Err(e) => st.say(e, true),
            }
        }
    });
}
