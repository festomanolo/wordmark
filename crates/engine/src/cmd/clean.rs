//! Watermark and web-address removal (`wordcraft-clean`): find what looks like a watermark in a
//! PDF (pictures, text, links and stamps drawn over its pages), learn from what a person picks,
//! save the PDF without it (the rest of each page kept as it was) or open it rebuilt as an
//! editable document, and clean the open document. The Remove Watermarks pane and agents both go through these commands.
//!
//! The learned model is a JSON file named by `model`; without it the built-in starting model
//! is used and nothing is learned. Every file is read through [`crate::file_access`], so
//! confined automation reaches only its roots.

use serde_json::{Value, json};
use wordcraft_clean::detect::{Decision, normalize};
use wordcraft_clean::{FileReport, Model, PageText, RebuildOptions, UrlAction, objects};

use super::sel_result;
use crate::{CmdError, CmdResult, CommandSpec, Session, p};

/// The largest PDF read (bytes); a bigger one is refused rather than held in memory.
const MAX_PDF: u64 = 512 * 1024 * 1024;
/// The largest learned model file read (bytes).
const MAX_MODEL: u64 = 64 * 1024 * 1024;
/// How many candidates a scan lists by default, and at most.
const DEFAULT_LIMIT: u64 = 30;
const MAX_LIMIT: u64 = 500;

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("clean.scan", "Find Watermarks", "Design › Watermark › Remove Watermarks", scan)
            .params(r#"{"path": string (a PDF), "password"?: string, "model"?: string (learned model file), "limit"?: number (candidates listed, default 30)}"#)
            .pure(),
        CommandSpec::new("clean.learn", "Learn Watermarks", "Design › Watermark › Remove Watermarks", learn)
            .params(r#"{"path": string (the PDF the texts come from), "model": string (model file, written), "watermarks"?: [string] (texts that are watermarks), "content"?: [string] (texts that are not), "watermarkObjects"?: [string] (object ids from clean.scan that are watermarks), "contentObjects"?: [string] (object ids that are not), "password"?: string, "limit"?: number (candidates the person was shown; unmarked ones train as content)}"#)
            .pure(),
        CommandSpec::new("clean.savePdf", "Save PDF Without Watermarks", "Design › Watermark › Remove Watermarks", save_pdf)
            .params(r#"{"path": string (a PDF), "out": string (the PDF written), "password"?: string, "model"?: string, "remove"?: [string] (object ids from clean.scan to remove as well), "keep"?: [string] (object ids to keep although the model would remove them)}"#)
            .pure(),
        CommandSpec::new("clean.openPdf", "Open PDF Without Watermarks", "Design › Watermark › Remove Watermarks", open_pdf)
            .params(r#"{"path": string (a PDF), "password"?: string, "model"?: string, "urls"?: "remove|unlink|keep" (default remove), "dropFlagged"?: bool (also drop text it is unsure of), "pageBreaks"?: bool (default true), "watermarks"?: [string], "content"?: [string], "limit"?: number (as clean.learn, for this call only)}"#)
            .pure(),
        CommandSpec::new("clean.document", "Remove Watermarks and Links", "Design › Watermark › Remove Watermarks", clean_document)
            .params(r#"{"model"?: string, "urls"?: "remove|unlink|keep" (default remove)}"#),
    ]
}

/// The model `v` names (`model`), or the starting model.
fn model(v: &Value) -> Result<Model, CmdError> {
    match p::str(v, "model").filter(|m| !m.is_empty()) {
        // A model file not written yet is a new model.
        Some(path) if !std::path::Path::new(path).exists() => {
            crate::file_access::check_read(path).map_err(CmdError::Failed)?;
            Ok(Model::default())
        }
        Some(path) => {
            let bytes = crate::io::read_file_capped(path, MAX_MODEL, "model").map_err(CmdError::Failed)?;
            serde_json::from_slice(&bytes).map_err(|e| CmdError::Failed(format!("{path}: {e}")))
        }
        None => Ok(Model::default()),
    }
}

/// The pages of the PDF `v` names (`path`, `password`).
fn read_pdf(v: &Value) -> Result<(String, Vec<PageText>), CmdError> {
    let path = p::req_str(v, "path")?;
    let bytes = crate::io::read_file_capped(path, MAX_PDF, "PDF").map_err(CmdError::Failed)?;
    if !bytes.starts_with(b"%PDF") && !bytes.windows(5).take(1024).any(|w| w == b"%PDF-") {
        return Err(CmdError::Failed(format!("{path} is not a PDF")));
    }
    let pages = wordcraft_clean::extract_pdf(&bytes, p::str(v, "password")).map_err(|e| CmdError::Failed(format!("{path}: {e}")))?;
    Ok((path.to_string(), pages))
}

fn limit(v: &Value) -> usize {
    usize::try_from(p::u64(v, "limit").unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)).unwrap_or(30)
}

fn urls(v: &Value) -> Result<UrlAction, CmdError> {
    match p::str(v, "urls") {
        None => Ok(UrlAction::Remove),
        Some(s) => UrlAction::parse(s).ok_or_else(|| CmdError::Params(format!("`urls` is remove, unlink or keep, not {s:?}"))),
    }
}

fn decision_name(d: Decision) -> &'static str {
    match d {
        Decision::Remove => "remove",
        Decision::Review => "review",
        Decision::Keep => "keep",
    }
}

/// 1-based page numbers of `pages` that pass `f`.
fn pages_where(pages: &[PageText], f: impl Fn(&PageText) -> bool) -> Vec<usize> {
    pages.iter().enumerate().filter(|(_, p)| f(p)).map(|(i, _)| i + 1).collect()
}

/// A string list parameter (`[string]`); missing is empty.
fn texts(v: &Value, key: &str) -> Result<Vec<String>, CmdError> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => {
            a.iter().map(|x| x.as_str().map(str::to_string).ok_or_else(|| CmdError::Params(format!("`{key}` is a list of texts")))).collect()
        }
        Some(_) => Err(CmdError::Params(format!("`{key}` is a list of texts"))),
    }
}

/// The PDF's distinct texts, most watermark-like first, with what the model would do to each.
fn scan(_: &mut Session, v: &Value) -> CmdResult {
    let model = model(v)?;
    let (path, pages) = read_pdf(v)?;
    let found = wordcraft_clean::candidates(&pages, &model, limit(v));
    let candidates: Vec<Value> =
        found.iter().map(|c| json!({"text": c.text, "pages": c.pages, "score": c.score, "decision": decision_name(model.decide(c.score))})).collect();
    Ok(json!({
        "path": path,
        "pages": pages.len(),
        "needsOcr": pages_where(&pages, PageText::needs_ocr),
        "incompletePages": pages_where(&pages, |p| p.failed || p.truncated),
        "candidates": candidates,
    }))
}

/// Teach `model` the texts of `pages` that `v` names (`watermarks`, `content`); the other texts
/// among the first `limit` shown train as content without being remembered. Returns how many
/// were learned and the named texts that were not found.
fn teach(model: &mut Model, pages: &[PageText], v: &Value) -> Result<(usize, Vec<String>), CmdError> {
    let watermarks = texts(v, "watermarks")?;
    let content = texts(v, "content")?;
    if watermarks.is_empty() && content.is_empty() {
        return Ok((0, Vec::new()));
    }
    let marked: Vec<String> = watermarks.iter().map(|t| normalize(t)).collect();
    let not: Vec<String> = content.iter().map(|t| normalize(t)).collect();
    // Every text of the PDF can be named, not only the listed ones.
    let all = wordcraft_clean::candidates(pages, model, usize::MAX);
    let shown = limit(v);
    let (mut labelled, mut unmarked, mut matched) = (Vec::new(), Vec::new(), Vec::new());
    for (at, c) in all.into_iter().enumerate() {
        let key = normalize(&c.text);
        if marked.contains(&key) {
            matched.push(key);
            labelled.push((c.text, c.features, true));
        } else if not.contains(&key) {
            matched.push(key);
            labelled.push((c.text, c.features, false));
        } else if at < shown {
            unmarked.push((c.text, c.features));
        }
    }
    let missing: Vec<String> = watermarks.into_iter().chain(content).filter(|t| !matched.contains(&normalize(t))).collect();
    if labelled.is_empty() {
        return Err(CmdError::Failed(format!("none of the texts were found in the PDF: {missing:?}")));
    }
    Ok((model.learn_with(labelled, unmarked), missing))
}

/// Teach the model which of the PDF's texts are watermarks, and save it.
fn learn(_: &mut Session, v: &Value) -> CmdResult {
    let model_path = p::req_str(v, "model")?.to_string();
    if texts(v, "watermarks")?.is_empty() && texts(v, "content")?.is_empty() {
        return Err(CmdError::Params("name at least one text in `watermarks` or `content`".into()));
    }
    crate::file_access::check_write(&model_path).map_err(CmdError::Failed)?;
    let mut model = model(v)?;
    let (_, pages) = read_pdf(v)?;
    let (learned, missing) = teach(&mut model, &pages, v)?;
    crate::io::write_file(&model_path, model.to_json().as_bytes()).map_err(CmdError::Failed)?;
    Ok(json!({"learned": learned, "examples": model.examples.len(), "model": model_path, "notFound": missing}))
}

/// Rebuild the PDF without its watermarks and open it as a new, unsaved document.
fn open_pdf(s: &mut Session, v: &Value) -> CmdResult {
    let mut model = model(v)?;
    let (path, pages) = read_pdf(v)?;
    // Texts named here are learned for this call only; `clean.learn` keeps them.
    let (_, not_found) = teach(&mut model, &pages, v)?;
    let opts = RebuildOptions {
        drop_flagged: p::bool(v, "dropFlagged").unwrap_or(false),
        urls: urls(v)?,
        page_breaks: p::bool(v, "pageBreaks").unwrap_or(true),
    };
    let mut report = FileReport::default();
    let mut doc = wordcraft_clean::pipeline::clean_pdf(&pages, &model, &opts, &mut report);
    doc.ensure_nonempty();
    let id = if s.used_because().is_none() {
        s.set_document(doc);
        s.document_id()
    } else {
        s.add_document(doc)
    };
    s.path = None;
    s.dirty = true;
    Ok(json!({
        "id": id,
        "from": path,
        "pages": report.pages,
        "removed": report.removed.len(),
        "flagged": report.flagged.iter().map(|i| &i.text).collect::<Vec<_>>(),
        "flaggedDropped": opts.drop_flagged,
        "urlsRemoved": report.urls_removed,
        "paragraphs": report.paragraphs,
        "needsOcr": report.needs_ocr,
        "incompletePages": report.incomplete_pages,
        "notFound": not_found,
    }))
}

/// Remove the open document's watermark, the texts the model learned as watermarks, and its
/// links and web addresses.
fn clean_document(s: &mut Session, v: &Value) -> CmdResult {
    let model = model(v)?;
    let urls = urls(v)?;
    let stats = wordcraft_clean::doc_clean::clean_document(&mut s.doc, &model, urls);
    s.doc.ensure_nonempty();
    s.clamp_selection();
    let mut r = sel_result(s)?;
    if let Some(o) = r.as_object_mut() {
        o.insert("watermarkRemoved".into(), json!(stats.native_watermark));
        o.insert("learnedTextRemoved".into(), json!(stats.watermark_text_removed));
        o.insert("linksUnlinked".into(), json!(stats.links_unlinked));
        o.insert("urlsRemoved".into(), json!(stats.urls_removed));
    }
    Ok(r)
}
