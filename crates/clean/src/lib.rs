//! Cleaning documents of watermarks and web addresses, one file or a folder at a time.
//!
//! - [`extract`] reads a PDF's text with where, how big, at what angle and how opaque each run
//!   of glyphs is drawn ([`extract::Span`]);
//! - [`detect`] scores each span as a watermark or not with a model a person can teach: mark a
//!   few real watermarks (and a few look-alikes that are content) and it learns what this
//!   source's stamps look like. Spans it is sure of are removed; ones it isn't are flagged for
//!   review;
//! - [`rebuild`] turns the kept spans back into paragraphs and headings in an editable document;
//! - [`doc_clean`] cleans a document that is already editable (.docx, .doc, .odt…);
//! - [`urls`] finds and removes web addresses and links;
//! - [`pipeline`] runs a batch as three concurrent stages (read, clean, write).
//!
//! Scanned pages without a text layer are pictures: their words (and a watermark baked into the
//! picture) are not read. Reports list them ([`pipeline::FileReport::needs_ocr`]) instead of
//! passing them off as cleaned.

pub mod detect;
pub mod doc_clean;
pub mod extract;
pub mod pipeline;
pub mod rebuild;
pub mod urls;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub use detect::{Decision, Model};
pub use extract::{PageText, Span, extract_pdf};
pub use pipeline::{DocIo, FileReport, Job, Options, run};
pub use rebuild::RebuildOptions;
pub use urls::UrlAction;

/// Spans that read the same, gathered across a document: what a person picks from when
/// teaching the model.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    /// The text as first seen.
    pub text: String,
    /// 1-based pages it appears on.
    pub pages: Vec<usize>,
    /// The highest score of its spans.
    pub score: f32,
    pub angle: f32,
    /// Features of its highest-scoring span (what is learned from).
    pub features: [f32; detect::N],
}

/// Distinct span texts of `pages`, most watermark-like first, at most `limit`.
pub fn candidates(pages: &[PageText], model: &Model, limit: usize) -> Vec<Candidate> {
    let mut by_text: HashMap<String, Candidate> = HashMap::new();
    for s in model.classify(pages) {
        let key = detect::normalize(&s.text);
        if key.chars().count() < 2 {
            continue;
        }
        let c = by_text.entry(key).or_insert_with(|| Candidate {
            text: s.text.clone(),
            pages: Vec::new(),
            score: s.score,
            angle: s.angle,
            features: s.features,
        });
        if !c.pages.contains(&(s.page + 1)) {
            c.pages.push(s.page + 1);
        }
        if s.score > c.score {
            c.score = s.score;
            c.angle = s.angle;
            c.features = s.features;
        }
    }
    let mut v: Vec<Candidate> = by_text.into_values().collect();
    v.sort_by(|a, b| b.score.total_cmp(&a.score).then(b.pages.len().cmp(&a.pages.len())));
    v.truncate(limit);
    v
}
