//! Batch cleaning as a staged pipeline. Three stages run at once, each with its own workers,
//! joined by bounded queues:
//!
//! 1. **read**: load the file; a PDF has its text read page by page, anything else is opened as
//!    a document;
//! 2. **clean**: score every span (pages in parallel) and rebuild the PDF's text without the
//!    watermarks, or clean the opened document;
//! 3. **write**: save the result in the chosen format.
//!
//! While one file is being written the next is being cleaned and a third read, so reading,
//! computing and writing overlap. The queues are bounded so a fast reader can't fill memory with
//! parsed files the cleaners haven't reached.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use wordcraft_doc::Document;

use crate::detect::{Decision, Model, N, Scored};
use crate::doc_clean::clean_document;
use crate::extract::{PageText, extract_pdf};
use crate::rebuild::{RebuildOptions, rebuild};

/// Opening and saving documents in formats other than PDF, which the app's format layer does.
pub trait DocIo: Sync {
    /// Open `bytes`, read from `path`, as a document (by its extension).
    fn open(&self, path: &Path, bytes: &[u8]) -> Result<Document, String>;
    /// Save `doc` at `path` in the format its extension names.
    fn save(&self, path: &Path, doc: &Document) -> Result<(), String>;
}

/// One file to clean.
#[derive(Clone, Debug)]
pub struct Job {
    pub input: PathBuf,
    pub output: PathBuf,
}

/// Settings for a batch.
#[derive(Clone, Debug)]
pub struct Options {
    pub model: Arc<Model>,
    pub rebuild: RebuildOptions,
    pub password: Option<String>,
    /// Workers per stage (at least 1).
    pub workers: usize,
}

/// A span the report lists: removed, or flagged for review.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Item {
    /// 1-based page.
    pub page: usize,
    pub text: String,
    pub score: f32,
    pub angle: f32,
    pub bbox: [f32; 4],
    pub features: [f32; N],
}

impl From<&Scored> for Item {
    fn from(s: &Scored) -> Item {
        Item { page: s.page + 1, text: s.text.clone(), score: s.score, angle: s.angle, bbox: s.bbox, features: s.features }
    }
}

/// What happened to one file.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FileReport {
    pub input: PathBuf,
    pub output: PathBuf,
    /// `pdf` or `document`.
    pub kind: String,
    /// Empty when the file was cleaned and saved.
    pub error: String,
    pub pages: usize,
    /// 1-based pages that are pictures with no text behind them: their words are not in the
    /// output until OCR is run on them.
    pub needs_ocr: Vec<usize>,
    /// 1-based pages that could not be read completely.
    pub incomplete_pages: Vec<usize>,
    pub removed: Vec<Item>,
    /// Not sure: kept in the output (unless flagged spans are dropped) for a person to decide.
    pub flagged: Vec<Item>,
    pub native_watermark_removed: bool,
    pub learned_text_removed: usize,
    pub links_unlinked: usize,
    pub urls_removed: usize,
    pub paragraphs: usize,
    /// Milliseconds spent in each stage: read, clean, write.
    pub stage_ms: [u64; 3],
}

impl FileReport {
    pub fn ok(&self) -> bool {
        self.error.is_empty()
    }
}

enum Parsed {
    Pdf(Vec<PageText>),
    Doc(Box<Document>),
}

struct Read {
    slot: usize,
    report: FileReport,
    parsed: Option<Parsed>,
}

struct Cleaned {
    slot: usize,
    report: FileReport,
    doc: Option<Box<Document>>,
}

/// Whether a file is read as a PDF: by its extension, or its signature when it has none.
pub fn is_pdf(path: &Path, bytes: &[u8]) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(e) => e.eq_ignore_ascii_case("pdf"),
        None => bytes.get(..1024).is_some_and(|h| h.windows(5).any(|w| w == b"%PDF-")),
    }
}

/// Read every input up to this size; bigger files are refused rather than loaded.
const MAX_INPUT: u64 = 1 << 30;

fn read_stage(job: &Job, opts: &Options, io: &dyn DocIo) -> (FileReport, Option<Parsed>) {
    let mut report = FileReport { input: job.input.clone(), output: job.output.clone(), ..Default::default() };
    let bytes = match std::fs::metadata(&job.input) {
        Ok(m) if m.len() > MAX_INPUT => Err(format!("{} is larger than 1 GiB", job.input.display())),
        Ok(_) => std::fs::read(&job.input).map_err(|e| format!("{}: {e}", job.input.display())),
        Err(e) => Err(format!("{}: {e}", job.input.display())),
    };
    let bytes = match bytes {
        Ok(b) => b,
        Err(e) => {
            report.error = e;
            return (report, None);
        }
    };
    if is_pdf(&job.input, &bytes) {
        report.kind = "pdf".into();
        match extract_pdf(&bytes, opts.password.as_deref()) {
            Ok(pages) => (report, Some(Parsed::Pdf(pages))),
            Err(e) => {
                report.error = e.to_string();
                (report, None)
            }
        }
    } else {
        report.kind = "document".into();
        match io.open(&job.input, &bytes) {
            Ok(doc) => (report, Some(Parsed::Doc(Box::new(doc)))),
            Err(e) => {
                report.error = e;
                (report, None)
            }
        }
    }
}

/// Score a PDF's spans and rebuild it as a document without the watermarks.
pub fn clean_pdf(pages: &[PageText], model: &Model, opts: &RebuildOptions, report: &mut FileReport) -> Document {
    let scored = model.classify(pages);
    report.pages = pages.len();
    report.needs_ocr = pages.iter().enumerate().filter(|(_, p)| p.needs_ocr()).map(|(i, _)| i + 1).collect();
    report.incomplete_pages = pages.iter().enumerate().filter(|(_, p)| p.failed || p.truncated).map(|(i, _)| i + 1).collect();
    for s in &scored {
        match s.decision {
            Decision::Remove => report.removed.push(s.into()),
            Decision::Review => report.flagged.push(s.into()),
            Decision::Keep => {}
        }
    }
    let (doc, stats) = rebuild(pages, &scored, opts);
    report.urls_removed += stats.urls_removed;
    report.paragraphs = stats.paragraphs;
    doc
}

fn clean_stage(r: Read, opts: &Options) -> Cleaned {
    let Read { slot, mut report, parsed } = r;
    let doc = match parsed {
        Some(Parsed::Pdf(pages)) => Some(Box::new(clean_pdf(&pages, &opts.model, &opts.rebuild, &mut report))),
        Some(Parsed::Doc(mut doc)) => {
            let st = clean_document(&mut doc, &opts.model, opts.rebuild.urls);
            report.native_watermark_removed = st.native_watermark;
            report.learned_text_removed = st.watermark_text_removed;
            report.links_unlinked = st.links_unlinked;
            report.urls_removed = st.urls_removed;
            report.paragraphs = doc.paragraph_count();
            Some(doc)
        }
        None => None,
    };
    Cleaned { slot, report, doc }
}

fn ms(t: Instant) -> u64 {
    u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Pull the next item off a queue that several workers share.
fn next<T>(rx: &Mutex<Receiver<T>>) -> Option<T> {
    rx.lock().ok()?.recv().ok()
}

/// Clean every job through the three stages and return the reports in job order. `progress` is
/// called as each file finishes (from a worker thread, in finishing order).
pub fn run(jobs: &[Job], opts: &Options, io: &dyn DocIo, progress: &(dyn Fn(&FileReport) + Sync)) -> Vec<FileReport> {
    let workers = opts.workers.max(1);
    let depth = workers * 2;
    let (job_tx, job_rx) = sync_channel::<(usize, &Job)>(depth);
    let (read_tx, read_rx) = sync_channel::<Read>(depth);
    let (clean_tx, clean_rx) = sync_channel::<Cleaned>(depth);
    let (done_tx, done_rx) = sync_channel::<(usize, FileReport)>(depth);
    let (job_rx, read_rx, clean_rx) = (Mutex::new(job_rx), Mutex::new(read_rx), Mutex::new(clean_rx));
    let mut out: Vec<Option<FileReport>> = vec![None; jobs.len()];
    std::thread::scope(|s| {
        s.spawn(move || {
            for j in jobs.iter().enumerate() {
                if job_tx.send(j).is_err() {
                    break;
                }
            }
        });
        spawn_stage(s, workers, &job_rx, read_tx, |(slot, job): (usize, &Job)| {
            let t = Instant::now();
            let (mut report, parsed) = read_stage(job, opts, io);
            report.stage_ms[0] = ms(t);
            Read { slot, report, parsed }
        });
        spawn_stage(s, workers, &read_rx, clean_tx, |r: Read| {
            let t = Instant::now();
            let mut c = clean_stage(r, opts);
            c.report.stage_ms[1] = ms(t);
            c
        });
        spawn_stage(s, workers, &clean_rx, done_tx, |c: Cleaned| {
            let Cleaned { slot, mut report, doc } = c;
            let t = Instant::now();
            if let Some(doc) = doc
                && report.ok()
            {
                if let Some(dir) = report.output.parent().filter(|d| !d.as_os_str().is_empty())
                    && let Err(e) = std::fs::create_dir_all(dir)
                {
                    report.error = format!("{}: {e}", dir.display());
                } else if let Err(e) = io.save(&report.output, &doc) {
                    report.error = e;
                }
            }
            report.stage_ms[2] = ms(t);
            progress(&report);
            (slot, report)
        });
        for (slot, report) in done_rx.iter() {
            if let Some(o) = out.get_mut(slot) {
                *o = Some(report);
            }
        }
    });
    out.into_iter()
        .zip(jobs)
        .map(|(r, j)| {
            r.unwrap_or_else(|| FileReport { input: j.input.clone(), output: j.output.clone(), error: "not processed".into(), ..Default::default() })
        })
        .collect()
}

/// `n` workers that take from `rx`, apply `f` and pass the result on to `tx`. The stage's sender
/// is dropped when its last worker ends, which ends the next stage.
fn spawn_stage<'s, 'e: 's, I: Send + 's, O: Send + 's>(
    s: &'s std::thread::Scope<'s, 'e>,
    n: usize,
    rx: &'s Mutex<Receiver<I>>,
    tx: SyncSender<O>,
    f: impl Fn(I) -> O + Send + Sync + 's,
) {
    let f = Arc::new(f);
    for _ in 0..n {
        let (tx, f) = (tx.clone(), Arc::clone(&f));
        s.spawn(move || {
            while let Some(item) = next(rx) {
                if tx.send(f(item)).is_err() {
                    break;
                }
            }
        });
    }
}
