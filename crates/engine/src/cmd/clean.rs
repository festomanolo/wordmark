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
        CommandSpec::new("clean.folder", "Clean a Folder of PDFs", "Design › Watermark › Remove Watermarks", folder)
            .params(r#"{"dir": string (a folder of PDFs; each clean copy is written beside its PDF, named <name>-clean.pdf), "subfolders"?: bool (default true), "password"?: string, "model"?: string}"#)
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
    load_model(p::str(v, "model")).map_err(CmdError::Failed)
}

/// The learned model in the file at `path`, or the starting model (no file named, or not written
/// yet).
pub fn load_model(path: Option<&str>) -> Result<Model, String> {
    match path.filter(|m| !m.is_empty()) {
        // A model file not written yet is a new model.
        Some(path) if !std::path::Path::new(path).exists() => {
            crate::file_access::check_read(path)?;
            Ok(Model::default())
        }
        Some(path) => {
            let bytes = crate::io::read_file_capped(path, MAX_MODEL, "model")?;
            serde_json::from_slice(&bytes).map_err(|e| format!("{path}: {e}"))
        }
        None => Ok(Model::default()),
    }
}

/// The pages of the PDF `v` names (`path`, `password`).
fn read_pdf(v: &Value) -> Result<(String, Vec<PageText>), CmdError> {
    let bytes = pdf_bytes(v)?;
    text_pages(v, &bytes)
}

/// The text pages of `bytes`, the PDF `v` names.
fn text_pages(v: &Value, bytes: &[u8]) -> Result<(String, Vec<PageText>), CmdError> {
    let path = p::req_str(v, "path")?;
    let pages = wordcraft_clean::extract_pdf(bytes, p::str(v, "password")).map_err(|e| CmdError::Failed(format!("{path}: {e}")))?;
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
    let bytes = pdf_bytes(v)?;
    let (path, pages) = text_pages(v, &bytes)?;
    let found = wordcraft_clean::candidates(&pages, &model, limit(v));
    let candidates: Vec<Value> =
        found.iter().map(|c| json!({"text": c.text, "pages": c.pages, "score": c.score, "decision": decision_name(model.decide(c.score))})).collect();
    let objects = objects::scan(&bytes, p::str(v, "password"), &model).map_err(|e| CmdError::Failed(format!("{path}: {e}")))?;
    Ok(json!({
        "path": path,
        "pages": pages.len(),
        "pageSizes": objects.pages,
        "needsOcr": pages_where(&pages, PageText::needs_ocr),
        "incompletePages": pages_where(&pages, |p| p.failed || p.truncated),
        "unreadablePages": objects.unreadable,
        "candidates": candidates,
        "objects": objects.objects.iter().map(object_json).collect::<Vec<_>>(),
    }))
}

/// The bytes of the PDF `v` names (`path`).
fn pdf_bytes(v: &Value) -> Result<Vec<u8>, CmdError> {
    let path = p::req_str(v, "path")?;
    let bytes = crate::io::read_file_capped(path, MAX_PDF, "PDF").map_err(CmdError::Failed)?;
    if !bytes.starts_with(b"%PDF") && !bytes.windows(5).take(1024).any(|w| w == b"%PDF-") {
        return Err(CmdError::Failed(format!("{path} is not a PDF")));
    }
    Ok(bytes)
}

/// The pictures, text, links and stamps drawn on the pages of the PDF `v` names.
fn scan_objects(v: &Value, model: &Model) -> Result<objects::Scan, CmdError> {
    let path = p::req_str(v, "path")?;
    objects::scan(&pdf_bytes(v)?, p::str(v, "password"), model).map_err(|e| CmdError::Failed(format!("{path}: {e}")))
}

fn object_json(o: &objects::PageObject) -> Value {
    json!({
        "id": o.id,
        "page": o.page,
        "kind": o.kind.name(),
        "bbox": o.bbox,
        "text": o.text,
        "pixels": o.pixels,
        "group": o.group,
        "score": o.score,
        "decision": decision_name(o.decision),
        "reasons": o.reasons,
    })
}

/// Write the PDF without its watermark objects: what the model would remove, plus `remove`,
/// less `keep`. Everything else on each page stays as it was.
fn save_pdf(_: &mut Session, v: &Value) -> CmdResult {
    let out = p::req_str(v, "out")?.to_string();
    crate::file_access::check_write(&out).map_err(CmdError::Failed)?;
    let model = model(v)?;
    let bytes = pdf_bytes(v)?;
    let path = p::req_str(v, "path")?;
    let password = p::str(v, "password");
    let found = objects::scan(&bytes, password, &model).map_err(|e| CmdError::Failed(format!("{path}: {e}")))?;
    let extra = texts(v, "remove")?;
    let keep = texts(v, "keep")?;
    let mut ids: Vec<String> = found.objects.iter().filter(|o| o.decision == Decision::Remove).map(|o| o.id.clone()).collect();
    ids.extend(extra);
    ids.retain(|id| !keep.contains(id));
    ids.sort();
    ids.dedup();
    let (pdf, done) = objects::remove(&bytes, password, &ids).map_err(|e| CmdError::Failed(format!("{path}: {e}")))?;
    crate::io::write_file(&out, &pdf).map_err(CmdError::Failed)?;
    Ok(json!({
        "from": path,
        "out": out,
        "removed": done.total(),
        "images": done.images,
        "graphics": done.graphics,
        "texts": done.texts,
        "links": done.links,
        "stamps": done.stamps,
        "notFound": done.not_found,
    }))
}

/// The most PDFs `clean.folder` takes from one folder; a bigger one is refused, not cut short.
const MAX_FOLDER_FILES: usize = 20_000;
/// How deep `clean.folder` goes into subfolders.
const MAX_FOLDER_DEPTH: usize = 32;

/// One PDF of a folder, cleaned by [`clean_folder_files`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FolderFile {
    pub input: String,
    pub output: String,
    /// Things removed.
    pub removed: usize,
    /// Things it was not sure of (counted once however many pages they are on): kept in the
    /// output for a person to check.
    pub unsure: usize,
    /// Empty when the file was cleaned and written.
    pub error: String,
}

impl FolderFile {
    pub fn to_json(&self) -> Value {
        json!({"input": self.input, "output": self.output, "removed": self.removed, "unsure": self.unsure, "error": self.error})
    }
}

/// The suffix a clean copy's name gets: `exam.pdf` is cleaned into `exam-clean.pdf`.
const CLEAN_SUFFIX: &str = "-clean";

/// Where the clean copy of `pdf` goes: beside it, named `<name>-clean.pdf`.
pub fn clean_copy_path(pdf: &str) -> String {
    let p = std::path::Path::new(pdf);
    let stem = p.file_stem().map_or_else(|| "document".to_string(), |s| s.to_string_lossy().into_owned());
    let ext = p.extension().map_or_else(|| "pdf".to_string(), |e| e.to_string_lossy().into_owned());
    p.with_file_name(format!("{stem}{CLEAN_SUFFIX}.{ext}")).to_string_lossy().into_owned()
}

/// The PDFs in `dir` (and its subfolders when `subfolders`), each with the path of its clean
/// copy beside it ([`clean_copy_path`]), sorted. Clean copies from an earlier run are not cleaned
/// again, links to folders are not followed, and hidden files are left out.
pub fn folder_jobs(dir: &str, subfolders: bool) -> Result<Vec<(String, String)>, String> {
    use std::path::{Path, PathBuf};
    crate::file_access::check_read(dir)?;
    let root = Path::new(dir);
    if !root.is_dir() {
        return Err(format!("{dir} is not a folder"));
    }
    let mut found: Vec<PathBuf> = Vec::new();
    let mut todo: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((d, depth)) = todo.pop() {
        if same(&d, out_root) {
            continue;
        }
        let entries = std::fs::read_dir(&d).map_err(|e| format!("{}: {e}", d.display()))?;
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let Ok(kind) = e.file_type() else { continue };
            let path = e.path();
            if kind.is_dir() {
                if subfolders && depth < MAX_FOLDER_DEPTH {
                    todo.push((path, depth + 1));
                }
            } else if Path::new(&name).extension().is_some_and(|x| x.eq_ignore_ascii_case("pdf")) && (kind.is_file() || path.is_file()) {
                found.push(path);
                if found.len() > MAX_FOLDER_FILES {
                    return Err(format!("{dir} has more than {MAX_FOLDER_FILES} PDFs: clean it a part at a time"));
                }
            }
        }
    }
    found.sort();
    Ok(found
        .into_iter()
        .map(|f| {
            let rel = f.strip_prefix(root).map_or_else(|_| PathBuf::from(f.file_name().unwrap_or_default()), Path::to_path_buf);
            (f.to_string_lossy().into_owned(), out_root.join(rel).to_string_lossy().into_owned())
        })
        .collect())
}

/// Clean one PDF with `model`: what it is sure of is removed and the result written to `output`
/// (a copy as it was when nothing is). Failures are in the result's `error`.
pub fn clean_folder_file(input: &str, output: &str, model: &Model, password: Option<&str>) -> FolderFile {
    let mut f = FolderFile { input: input.to_string(), output: output.to_string(), ..FolderFile::default() };
    let run = || -> Result<(usize, usize), String> {
        crate::file_access::check_write(output)?;
        let bytes = crate::io::read_file_capped(input, MAX_PDF, "PDF")?;
        let found = objects::scan(&bytes, password, model).map_err(|e| format!("{input}: {e}"))?;
        let ids: Vec<String> = found.objects.iter().filter(|o| o.decision == Decision::Remove).map(|o| o.id.clone()).collect();
        let unsure: std::collections::HashSet<&str> =
            found.objects.iter().filter(|o| o.decision == Decision::Review).map(|o| o.group.as_str()).collect();
        let (pdf, removed) = if ids.is_empty() {
            (bytes, 0)
        } else {
            let (pdf, done) = objects::remove(&bytes, password, &ids).map_err(|e| format!("{input}: {e}"))?;
            (pdf, done.total())
        };
        if let Some(dir) = std::path::Path::new(output).parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        crate::io::write_file(output, &pdf)?;
        Ok((removed, unsure.len()))
    };
    match run() {
        Ok((removed, unsure)) => {
            f.removed = removed;
            f.unsure = unsure;
        }
        Err(e) => f.error = e,
    }
    f
}

/// Clean every `(input, output)` of `jobs` with the model in `model_path` (or the starting
/// model), a few files at once. `progress` is called as each file finishes, from a worker
/// thread; once `cancel` is set no more files are started. Returns the files done, in `jobs`
/// order. The file-access roots in force on the calling thread apply on the workers too.
pub fn clean_folder_files(
    jobs: &[(String, String)],
    model_path: Option<&str>,
    password: Option<&str>,
    cancel: &std::sync::atomic::AtomicBool,
    progress: &(dyn Fn(&FolderFile) + Sync),
) -> Result<Vec<FolderFile>, String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let model = load_model(model_path)?;
    let roots = crate::file_access::current();
    let next = AtomicUsize::new(0);
    let done: std::sync::Mutex<Vec<(usize, FolderFile)>> = std::sync::Mutex::new(Vec::with_capacity(jobs.len()));
    let workers = std::thread::available_parallelism().map_or(2, usize::from).clamp(1, 4).min(jobs.len().max(1));
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                crate::file_access::confine(roots.as_ref(), || {
                    while !cancel.load(Ordering::Relaxed) {
                        let at = next.fetch_add(1, Ordering::Relaxed);
                        let Some((input, output)) = jobs.get(at) else { break };
                        let f = clean_folder_file(input, output, &model, password);
                        progress(&f);
                        if let Ok(mut d) = done.lock() {
                            d.push((at, f));
                        }
                    }
                });
            });
        }
    });
    let mut done = done.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner);
    done.sort_by_key(|(at, _)| *at);
    Ok(done.into_iter().map(|(_, f)| f).collect())
}

/// `clean.folder`: clean every PDF in a folder with what the model has learned.
fn folder(_: &mut Session, v: &Value) -> CmdResult {
    let dir = p::req_str(v, "dir")?;
    let out = p::str(v, "out").filter(|o| !o.is_empty()).map_or_else(|| default_folder_out(dir), str::to_string);
    let subfolders = v.get("subfolders").and_then(Value::as_bool).unwrap_or(true);
    let jobs = folder_jobs(dir, &out, subfolders).map_err(CmdError::Failed)?;
    let model = p::str(v, "model");
    let never = std::sync::atomic::AtomicBool::new(false);
    let files = clean_folder_files(&jobs, model, p::str(v, "password"), &never, &|_| {}).map_err(CmdError::Failed)?;
    let count = |f: &dyn Fn(&FolderFile) -> bool| files.iter().filter(|x| f(x)).count();
    Ok(json!({
        "dir": dir,
        "out": out,
        "pdfs": files.len(),
        "cleaned": count(&|f| f.error.is_empty()),
        "needCheck": count(&|f| f.error.is_empty() && f.unsure > 0),
        "failed": count(&|f| !f.error.is_empty()),
        "removed": files.iter().map(|f| f.removed).sum::<usize>(),
        "files": files.iter().map(FolderFile::to_json).collect::<Vec<_>>(),
    }))
}

/// The largest preview side (pixels).
const MAX_PREVIEW_SIDE: u32 = 2400;

/// Page `page` (0-based) of the PDF at `path`, drawn for the Remove Watermarks pane. Not a
/// command (it changes nothing and its result is pixels); automation reads boxes from
/// `clean.scan`. The file is read through [`crate::file_access`] like the commands' files.
pub fn page_preview(path: &str, password: Option<&str>, page: usize, max_side: u32) -> Result<objects::PagePicture, String> {
    let bytes = crate::io::read_file_capped(path, MAX_PDF, "PDF")?;
    objects::render_page(&bytes, password, page, max_side.min(MAX_PREVIEW_SIDE)).map_err(|e| format!("{path}: {e}"))
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
    let (marked_objects, kept_objects) = (texts(v, "watermarkObjects")?, texts(v, "contentObjects")?);
    let some_texts = !(texts(v, "watermarks")?.is_empty() && texts(v, "content")?.is_empty());
    if !some_texts && marked_objects.is_empty() && kept_objects.is_empty() {
        return Err(CmdError::Params(
            "name at least one text in `watermarks` or `content`, or an object in `watermarkObjects` or `contentObjects`".into(),
        ));
    }
    crate::file_access::check_write(&model_path).map_err(CmdError::Failed)?;
    let mut model = model(v)?;
    let (mut learned, mut missing) = (0, Vec::new());
    if some_texts {
        let (_, pages) = read_pdf(v)?;
        (learned, missing) = teach(&mut model, &pages, v)?;
    }
    if !marked_objects.is_empty() || !kept_objects.is_empty() {
        // Keys are read from the file as the model saw it before this lesson.
        let found = scan_objects(v, &model)?;
        let known: std::collections::HashSet<&str> = found.objects.iter().map(|o| o.id.as_str()).collect();
        missing.extend(marked_objects.iter().chain(&kept_objects).filter(|id| !known.contains(id.as_str())).cloned());
        learned += model.learn_objects(&objects::keys_of(&found, &marked_objects), &objects::keys_of(&found, &kept_objects));
    }
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
