//! Watermark detection: features of each span, a model that scores them, and learning from the
//! spans a person marks as watermarks (or as real content).
//!
//! The model is a logistic regression over a handful of features that tell a stamp from text:
//! turned off the baseline, repeated on most pages, see-through, pale, big, centred, marked as an
//! artifact, looks like a "downloaded from" link. It starts from hand-set weights that catch the
//! common stamps, and every example a person labels moves the weights (pulled back towards the
//! starting ones, so a few examples can't swing it wildly). Texts a person marked are also
//! remembered: a span that reads like one is very likely the same watermark again.

use std::collections::{HashMap, HashSet};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::extract::{PageText, Span};

/// Names of the features, in vector order (reports print them).
pub const FEATURES: [&str; 12] = [
    "rotated",
    "repeats",
    "transparent",
    "pale",
    "large",
    "centred",
    "artifact",
    "link_like",
    "page_edge",
    "learned_watermark",
    "learned_content",
    "invisible",
];
pub const N: usize = FEATURES.len();

/// The weights a new model starts with (and is pulled back towards while learning).
const PRIOR: [f32; N] = [2.6, 2.2, 2.0, 1.1, 0.9, 0.8, 0.7, 0.6, 0.0, 5.0, -6.0, -3.0];
const PRIOR_BIAS: f32 = -4.2;

/// What to do with a span.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Confident it is a watermark: removed.
    Remove,
    /// Not sure: flagged for a person to look at.
    Review,
    /// Content: kept.
    Keep,
}

/// A labelled example a person gave.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Example {
    pub text: String,
    pub features: [f32; N],
    pub watermark: bool,
}

/// The learned model, saved as JSON between runs.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Model {
    pub version: u32,
    pub weights: [f32; N],
    pub bias: f32,
    /// At or above: removed without asking.
    pub remove_at: f32,
    /// At or above (and below `remove_at`): flagged for review.
    pub review_at: f32,
    pub examples: Vec<Example>,
    /// Normalised texts marked as watermarks.
    pub watermark_texts: Vec<String>,
    /// Normalised texts marked as content.
    pub content_texts: Vec<String>,
}

impl Default for Model {
    fn default() -> Self {
        Model {
            version: 1,
            weights: PRIOR,
            bias: PRIOR_BIAS,
            remove_at: 0.8,
            review_at: 0.3,
            examples: Vec::new(),
            watermark_texts: Vec::new(),
            content_texts: Vec::new(),
        }
    }
}

/// A span with its features, score and decision.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Scored {
    /// 0-based page.
    pub page: usize,
    /// Index of the span on its page.
    pub index: usize,
    pub text: String,
    pub features: [f32; N],
    pub score: f32,
    pub decision: Decision,
    pub bbox: [f32; 4],
    pub angle: f32,
}

/// Text as compared across pages and with learned texts: lower case, digits as `#`, runs of
/// space as one, so "Page 3 of 9" on every page reads the same.
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        if c.is_numeric() {
            if !out.ends_with('#') {
                out.push('#');
            }
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// Similarity of two normalised texts, 0..=1: the share of character trigrams they have in
/// common (Jaccard), or containment when one is a short stamp inside a longer line.
pub fn similarity(a: &str, b: &str) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a == b {
        return 1.0;
    }
    let grams = |s: &str| -> HashSet<[char; 3]> {
        let v: Vec<char> = format!("  {s} ").chars().collect();
        v.windows(3).map(|w| [w[0], w[1], w[2]]).collect()
    };
    let (ga, gb) = (grams(a), grams(b));
    let inter = ga.intersection(&gb).count() as f32;
    let union = ga.union(&gb).count() as f32;
    let jaccard = if union > 0.0 { inter / union } else { 0.0 };
    // A learned stamp "confidential" inside "confidential - do not copy" counts as nearly the same.
    let contain = inter / ga.len().min(gb.len()).max(1) as f32;
    let short = a.chars().count().min(b.chars().count());
    let chars = if short >= 6 { jaccard.max(contain * 0.9) } else { jaccard };
    chars.max(word_overlap(a, b))
}

/// Similarity by words, for stamps that differ in a name or a date ("licensed copy for alice
/// moyo only" and "licensed copy for neema only"): the share of the longer text's words of three
/// letters or more that the other has, when both have at least three such words and are about the
/// same length. Zero otherwise, so a long paragraph that happens to use a stamp's words is not
/// taken for it.
fn word_overlap(a: &str, b: &str) -> f32 {
    let words = |s: &str| -> Vec<String> {
        s.split(|c: char| !c.is_alphanumeric() && c != '#').filter(|w| w.chars().count() >= 3).map(str::to_string).collect()
    };
    let (wa, wb) = (words(a), words(b));
    let (short, long) = if wa.len() <= wb.len() { (&wa, &wb) } else { (&wb, &wa) };
    if short.len() < 3 || long.len() as f32 > short.len() as f32 * 1.6 + 1.0 {
        return 0.0;
    }
    let set: HashSet<&String> = short.iter().collect();
    let common = long.iter().filter(|w| set.contains(w)).count();
    common as f32 / long.len() as f32
}

fn best_match(text: &str, list: &[String]) -> f32 {
    list.iter().map(|t| similarity(text, t)).fold(0.0, f32::max)
}

static LINK_WORDS: &[&str] = &["downloaded from", "download", "www.", "http", ".com", ".org", ".net", "licensed to", "copy of", "printed by"];

fn link_like(norm: &str) -> f32 {
    if crate::urls::contains_url(norm) {
        return 1.0;
    }
    if LINK_WORDS.iter().any(|w| norm.contains(w)) { 0.6 } else { 0.0 }
}

/// Features of every span of a document (pages in parallel).
pub fn features(pages: &[PageText], model: &Model) -> Vec<Vec<[f32; N]>> {
    // On how many pages each normalised text appears.
    let mut seen: HashMap<String, usize> = HashMap::new();
    for p in pages {
        let on_page: HashSet<String> = p.spans.iter().map(|s| normalize(&s.text)).collect();
        for t in on_page {
            *seen.entry(t).or_default() += 1;
        }
    }
    let n_pages = pages.len();
    pages
        .par_iter()
        .map(|p| {
            let body = body_size(p);
            p.spans.iter().map(|s| span_features(s, p, body, &seen, n_pages, model)).collect()
        })
        .collect()
}

/// The page's body text size: the size most characters are drawn at.
fn body_size(p: &PageText) -> f32 {
    let mut by_size: HashMap<i32, usize> = HashMap::new();
    for s in &p.spans {
        *by_size.entry((s.size * 2.0).round() as i32).or_default() += s.text.chars().count();
    }
    by_size.into_iter().max_by_key(|&(_, n)| n).map_or(11.0, |(k, _)| k as f32 / 2.0).max(1.0)
}

fn span_features(s: &Span, p: &PageText, body: f32, seen: &HashMap<String, usize>, n_pages: usize, model: &Model) -> [f32; N] {
    let norm = normalize(&s.text);
    let off_axis = {
        let a = s.angle.rem_euclid(180.0);
        a.min(180.0 - a) // 0 = horizontal, 90 = vertical
    };
    let rotated = (off_axis / 30.0).clamp(0.0, 1.0);
    let repeats = if n_pages >= 2 {
        let n = seen.get(&norm).copied().unwrap_or(1);
        ((n as f32 - 1.0) / (n_pages as f32 - 1.0)).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let transparent = (1.0 - s.alpha).clamp(0.0, 1.0);
    let [r, g, b] = s.rgb.map(|c| f32::from(c) / 255.0);
    let lum = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let pale = ((lum - 0.55) / 0.35).clamp(0.0, 1.0);
    let large = ((s.size / body - 1.0) / 2.0).clamp(0.0, 1.0);
    let (w, h) = (p.width.max(1.0), p.height.max(1.0));
    let cx = (s.bbox[0] + s.bbox[2]) / 2.0 / w - 0.5;
    let cy = (s.bbox[1] + s.bbox[3]) / 2.0 / h - 0.5;
    let centred = (1.0 - 2.0 * (cx * cx + cy * cy).sqrt()).clamp(0.0, 1.0);
    let edge = {
        let top = s.bbox[1] / h;
        let bottom = 1.0 - s.bbox[3] / h;
        if top.min(bottom) < 0.08 { 1.0 } else { 0.0 }
    };
    [
        rotated,
        repeats,
        transparent,
        pale,
        large,
        centred,
        if s.artifact { 1.0 } else { 0.0 },
        link_like(&norm),
        edge,
        best_match(&norm, &model.watermark_texts),
        best_match(&norm, &model.content_texts),
        if s.invisible { 1.0 } else { 0.0 },
    ]
}

fn sigmoid(z: f32) -> f32 {
    1.0 / (1.0 + (-z).exp())
}

impl Model {
    /// Probability that a span with these features is a watermark.
    pub fn score(&self, f: &[f32; N]) -> f32 {
        let z = self.bias + self.weights.iter().zip(f).map(|(w, x)| w * x).sum::<f32>();
        sigmoid(z)
    }

    pub fn decide(&self, score: f32) -> Decision {
        if score >= self.remove_at {
            Decision::Remove
        } else if score >= self.review_at {
            Decision::Review
        } else {
            Decision::Keep
        }
    }

    /// Score and decide every span of a document.
    pub fn classify(&self, pages: &[PageText]) -> Vec<Scored> {
        let feats = features(pages, self);
        pages
            .iter()
            .zip(feats)
            .enumerate()
            .flat_map(|(pi, (p, fs))| {
                p.spans.iter().zip(fs).enumerate().map(move |(i, (s, f))| {
                    let score = self.score(&f);
                    Scored {
                        page: pi,
                        index: i,
                        text: s.text.clone(),
                        features: f,
                        score,
                        decision: self.decide(score),
                        bbox: s.bbox,
                        angle: s.angle,
                    }
                })
            })
            .map(|mut sc| {
                // A text a person marked as content is never removed without asking.
                if sc.features[10] >= 0.95 && sc.decision == Decision::Remove {
                    sc.decision = Decision::Review;
                }
                sc
            })
            .collect()
    }

    /// Add labelled examples (a span's text and features, and whether it is a watermark) and
    /// retrain. Returns how many examples the model now holds.
    pub fn learn(&mut self, labelled: impl IntoIterator<Item = (String, [f32; N], bool)>) -> usize {
        self.learn_with(labelled, std::iter::empty())
    }

    /// [`Model::learn`], plus `unmarked`: spans a person was shown and left unmarked. They train
    /// the weights as content (so one marked example can't make everything look like a
    /// watermark) but their texts are not remembered as content.
    pub fn learn_with(
        &mut self,
        labelled: impl IntoIterator<Item = (String, [f32; N], bool)>,
        unmarked: impl IntoIterator<Item = (String, [f32; N])>,
    ) -> usize {
        for (text, mut features) in unmarked {
            features[9] = 0.0;
            features[10] = 0.0;
            let norm = normalize(&text);
            if self.examples.iter().any(|e| normalize(&e.text) == norm) {
                continue;
            }
            self.examples.push(Example { text, features, watermark: false });
        }
        for (text, mut features, watermark) in labelled {
            let norm = normalize(&text);
            let (add, remove) =
                if watermark { (&mut self.watermark_texts, &mut self.content_texts) } else { (&mut self.content_texts, &mut self.watermark_texts) };
            remove.retain(|t| *t != norm);
            if !norm.is_empty() && !add.contains(&norm) {
                add.push(norm.clone());
            }
            // The text-match features describe the model's memory, not the span: train on the
            // other features so the weights learn what the stamp looks like.
            features[9] = 0.0;
            features[10] = 0.0;
            // A person's answer replaces any earlier example of the same text.
            self.examples.retain(|e| normalize(&e.text) != norm);
            self.examples.push(Example { text, features, watermark });
        }
        self.train();
        self.examples.len()
    }

    /// Gradient descent on the log loss of the examples, with an L2 pull towards the prior.
    fn train(&mut self) {
        if self.examples.is_empty() {
            return;
        }
        let (mut w, mut b) = (self.weights, self.bias);
        let n = self.examples.len() as f32;
        let lambda = 0.5 / n.sqrt();
        let lr = 0.3;
        for _ in 0..600 {
            let mut gw = [0.0f32; N];
            let mut gb = 0.0;
            for e in &self.examples {
                let z = b + w.iter().zip(&e.features).map(|(wi, x)| wi * x).sum::<f32>();
                let err = sigmoid(z) - if e.watermark { 1.0 } else { 0.0 };
                for (g, x) in gw.iter_mut().zip(&e.features) {
                    *g += err * x / n;
                }
                gb += err / n;
            }
            for i in 0..N {
                // The text-match weights stay as set: they are never trained on (see `learn`).
                if i == 9 || i == 10 {
                    continue;
                }
                w[i] -= lr * (gw[i] + lambda * (w[i] - PRIOR[i]));
            }
            b -= lr * (gb + lambda * (b - PRIOR_BIAS));
        }
        if w.iter().chain([&b]).all(|v| v.is_finite()) {
            self.weights = w;
            self.bias = b;
        }
    }

    /// Read a model saved with [`Model::to_json`]; a missing file is a new model.
    pub fn load(path: &std::path::Path) -> Result<Model, String> {
        match std::fs::read(path) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Model::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
}
