//! Spans back into an editable document: lines from spans on one baseline, paragraphs from lines
//! that follow each other closely, headings from text set larger than the body. Each PDF page
//! starts on a new page.

use wordcraft_doc::{CharProps, Document, Paragraph, para_block};

use crate::detect::{Decision, Scored};
use crate::extract::{PageText, Span, angle_diff};
use crate::urls::{UrlAction, strip_urls, tidy_spaces};

/// How the rebuilt document is laid out.
#[derive(Clone, Copy, Debug)]
pub struct RebuildOptions {
    /// Flagged spans are dropped too (otherwise they stay, for a person to look at).
    pub drop_flagged: bool,
    pub urls: UrlAction,
    /// Start each PDF page on a new page.
    pub page_breaks: bool,
}

impl Default for RebuildOptions {
    fn default() -> Self {
        RebuildOptions { drop_flagged: false, urls: UrlAction::Remove, page_breaks: true }
    }
}

/// What rebuilding did.
#[derive(Clone, Debug, Default)]
pub struct RebuildStats {
    pub urls_removed: usize,
    pub paragraphs: usize,
}

struct Line {
    y: f32,
    x0: f32,
    x1: f32,
    size: f32,
    text: String,
}

/// The document `pages` read as, without the spans `scored` says to remove.
pub fn rebuild(pages: &[PageText], scored: &[Scored], opts: &RebuildOptions) -> (Document, RebuildStats) {
    let mut stats = RebuildStats::default();
    let mut body = Vec::new();
    let drop = |d: Decision| d == Decision::Remove || (opts.drop_flagged && d == Decision::Review);
    let body_size = {
        let mut sizes: Vec<(f32, usize)> = Vec::new();
        for p in pages {
            for s in &p.spans {
                let k = (s.size * 2.0).round() / 2.0;
                match sizes.iter_mut().find(|(sz, _)| (*sz - k).abs() < 0.01) {
                    Some((_, n)) => *n += s.text.len(),
                    None => sizes.push((k, s.text.len())),
                }
            }
        }
        sizes.into_iter().max_by_key(|&(_, n)| n).map_or(11.0, |(s, _)| s).max(1.0)
    };
    let mut si = 0;
    for (pi, page) in pages.iter().enumerate() {
        let mut kept: Vec<&Span> = Vec::new();
        for (i, s) in page.spans.iter().enumerate() {
            // `scored` holds every span of every page in order.
            let removed = scored.get(si).filter(|sc| sc.page == pi && sc.index == i).is_some_and(|sc| drop(sc.decision));
            si += 1;
            if !removed {
                kept.push(s);
            }
        }
        let lines = lines_of(&kept);
        let mut first_on_page = pi > 0 && opts.page_breaks;
        for (text, size) in paragraphs_of(&lines) {
            let (text, n) = if opts.urls == UrlAction::Remove { strip_urls(&text) } else { (text, 0) };
            stats.urls_removed += n;
            if text.trim().is_empty() {
                continue;
            }
            let mut p = Paragraph::with_text(&text, CharProps::default());
            if size >= body_size * 1.6 {
                p = p.styled("Heading1");
            } else if size >= body_size * 1.25 {
                p = p.styled("Heading2");
            }
            if first_on_page {
                p.props.page_break_before = Some(true);
                first_on_page = false;
            }
            body.push(para_block(p));
            stats.paragraphs += 1;
        }
    }
    let mut doc = Document::new();
    if !body.is_empty() {
        doc.body = body;
    }
    doc.ensure_nonempty();
    (doc, stats)
}

/// Group spans into lines, top to bottom. Spans turned well off the horizontal (a margin note
/// set sideways) each make a line of their own.
fn lines_of(spans: &[&Span]) -> Vec<Line> {
    let mut flat: Vec<&Span> = spans.iter().copied().filter(|s| angle_diff(s.angle, 0.0) < 10.0).collect();
    flat.sort_by(|a, b| a.origin[1].total_cmp(&b.origin[1]).then(a.origin[0].total_cmp(&b.origin[0])));
    let mut lines: Vec<(f32, f32, Vec<&Span>)> = Vec::new(); // baseline, size, spans
    for s in flat {
        match lines.last_mut() {
            Some((y, size, v)) if (s.origin[1] - *y).abs() < 0.5 * size.min(s.size) => {
                v.push(s);
                *size = size.max(s.size);
            }
            _ => lines.push((s.origin[1], s.size, vec![s])),
        }
    }
    let mut out: Vec<Line> = lines
        .into_iter()
        .map(|(y, size, mut v)| {
            v.sort_by(|a, b| a.bbox[0].total_cmp(&b.bbox[0]));
            let mut text = String::new();
            let mut last_x1: Option<f32> = None;
            for s in &v {
                if let Some(x1) = last_x1
                    && s.bbox[0] - x1 > 0.15 * s.size
                    && !text.ends_with(' ')
                {
                    text.push(' ');
                }
                text.push_str(&s.text);
                last_x1 = Some(s.bbox[2]);
            }
            Line { y, x0: v.first().map_or(0.0, |s| s.bbox[0]), x1: v.iter().map(|s| s.bbox[2]).fold(0.0, f32::max), size, text }
        })
        .collect();
    for s in spans.iter().filter(|s| angle_diff(s.angle, 0.0) >= 10.0) {
        out.push(Line { y: s.origin[1], x0: s.bbox[0], x1: s.bbox[2], size: s.size, text: s.text.clone() });
    }
    out
}

/// Join lines into paragraphs: a paragraph ends at a wider gap than the line spacing, a change of
/// text size, or a line that stops well short of the others.
fn paragraphs_of(lines: &[Line]) -> Vec<(String, f32)> {
    let right = lines.iter().map(|l| l.x1).fold(0.0, f32::max);
    let left = lines.iter().map(|l| l.x0).fold(f32::INFINITY, f32::min);
    let full = (right - left).max(1.0);
    let mut out: Vec<(String, f32)> = Vec::new();
    let mut prev: Option<&Line> = None;
    for l in lines {
        let continues = prev.is_some_and(|p| {
            let gap = l.y - p.y;
            let same_size = (l.size - p.size).abs() <= 0.15 * p.size;
            let short = (p.x1 - left) < 0.75 * full;
            same_size && gap > 0.0 && gap < 1.7 * p.size && !short
        });
        match (continues, out.last_mut()) {
            (true, Some((text, _))) => {
                // A word broken at the line's end with a hyphen is joined back up.
                if text.ends_with('-') && l.text.chars().next().is_some_and(char::is_lowercase) {
                    text.pop();
                } else {
                    text.push(' ');
                }
                text.push_str(&l.text);
            }
            _ => out.push((l.text.clone(), l.size)),
        }
        prev = Some(l);
    }
    out.into_iter().map(|(t, s)| (tidy_spaces(&t), s)).collect()
}
