//! Cleaning a document that is already editable (opened from .docx, .doc, .odt, .rtf…): its
//! watermark, text a person taught the model is a watermark, links and web addresses, in the body,
//! tables, headers, footers and notes.

use std::sync::Arc;

use wordcraft_doc::{Block, Blocks, Document, Paragraph};

use crate::detect::{Model, normalize, similarity};
use crate::urls::{UrlAction, find_urls};

/// What cleaning an editable document did.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct DocCleanStats {
    /// The document's own watermark (Design › Watermark) was removed.
    pub native_watermark: bool,
    /// Paragraphs or parts of paragraphs removed as learned watermark text.
    pub watermark_text_removed: usize,
    pub links_unlinked: usize,
    pub urls_removed: usize,
}

/// Remove watermarks and links from `doc` as `model` and `urls` say.
pub fn clean_document(doc: &mut Document, model: &Model, urls: UrlAction) -> DocCleanStats {
    let mut stats = DocCleanStats { native_watermark: doc.settings.watermark.take().is_some(), ..Default::default() };
    // The raw texts a person marked, for finding them inside longer paragraphs.
    let mut marked: Vec<String> =
        model.examples.iter().filter(|e| e.watermark).map(|e| e.text.trim().to_lowercase()).filter(|t| t.chars().count() >= 4).collect();
    marked.sort_by_key(|t| std::cmp::Reverse(t.len()));
    marked.dedup();
    let cx = Ctx { model, urls, marked: &marked };
    clean_blocks(&mut doc.body, &cx, &mut stats);
    for part in doc.parts.values_mut() {
        clean_blocks(&mut part.blocks, &cx, &mut stats);
    }
    doc.ensure_nonempty();
    stats
}

struct Ctx<'a> {
    model: &'a Model,
    urls: UrlAction,
    marked: &'a [String],
}

fn clean_blocks(blocks: &mut Blocks, cx: &Ctx<'_>, stats: &mut DocCleanStats) {
    for b in blocks.iter_mut() {
        // Look before copying: copy-on-write blocks that need nothing stay shared.
        if !needs_work(b, cx) {
            continue;
        }
        match Arc::make_mut(b) {
            Block::Para(p) => clean_para(p, cx, stats),
            Block::Table(t) => {
                for row in &mut t.rows {
                    for cell in &mut row.cells {
                        clean_blocks(&mut cell.blocks, cx, stats);
                    }
                }
            }
        }
    }
}

fn needs_work(b: &Block, cx: &Ctx<'_>) -> bool {
    match b {
        Block::Para(p) => para_needs_work(p, cx),
        Block::Table(t) => t.rows.iter().any(|r| r.cells.iter().any(|c| c.blocks.iter().any(|b| needs_work(b, cx)))),
    }
}

fn para_needs_work(p: &Paragraph, cx: &Ctx<'_>) -> bool {
    if p.text.is_empty() {
        return false;
    }
    let has_link = p.runs.iter().any(|r| r.props.link.is_some());
    (cx.urls != UrlAction::Keep && has_link)
        || (cx.urls == UrlAction::Remove && crate::urls::contains_url(&p.text))
        || !cx.model.watermark_texts.is_empty()
}

fn clean_para(p: &mut Paragraph, cx: &Ctx<'_>, stats: &mut DocCleanStats) {
    // The whole paragraph reads like a learned watermark (and not like learned content).
    let norm = normalize(&p.text);
    let wm = cx.model.watermark_texts.iter().map(|t| similarity(&norm, t)).fold(0.0, f32::max);
    let content = cx.model.content_texts.iter().map(|t| similarity(&norm, t)).fold(0.0, f32::max);
    if wm >= 0.6 && content < wm {
        let keep = p.object_offsets();
        if p.delete_except(0, p.text.len(), &keep).is_ok() {
            stats.watermark_text_removed += 1;
        }
        return;
    }
    // A learned watermark inside a longer paragraph.
    if !cx.marked.is_empty() {
        let mut ranges = Vec::new();
        let lower = p.text.to_lowercase();
        // Lower-casing can change byte lengths; only search when it didn't.
        if lower.len() == p.text.len() {
            for m in cx.marked {
                let mut from = 0;
                while let Some(i) = lower[from..].find(m.as_str()) {
                    ranges.push(from + i..from + i + m.len());
                    from += i + m.len();
                }
            }
        }
        stats.watermark_text_removed += delete_ranges(p, ranges);
    }
    if cx.urls != UrlAction::Keep {
        let linked: Vec<(usize, usize)> = p.run_ranges().filter(|(_, c)| c.link.is_some()).map(|(r, _)| (r.start, r.end)).collect();
        for &(a, b) in &linked {
            if p.format(a, b, &|c| *c = c.unlinked()).is_ok() {
                stats.links_unlinked += 1;
            }
        }
    }
    if cx.urls == UrlAction::Remove {
        let found = find_urls(&p.text);
        stats.urls_removed += delete_ranges(p, found);
    }
}

/// Delete byte ranges (any order, may overlap) from the end backwards, with the space before a
/// deleted range when it would leave two. Returns how many were deleted.
fn delete_ranges(p: &mut Paragraph, mut ranges: Vec<std::ops::Range<usize>>) -> usize {
    ranges.sort_by_key(|r| std::cmp::Reverse(r.start));
    let mut n = 0;
    let mut floor = usize::MAX;
    for r in ranges {
        let end = r.end.min(floor);
        if r.start >= end {
            continue;
        }
        let bytes = p.text.as_bytes();
        let mut start = r.start;
        let before_space = start > 0 && bytes.get(start - 1) == Some(&b' ');
        let after_space_or_end = end >= p.text.len() || bytes.get(end) == Some(&b' ');
        if before_space && after_space_or_end {
            start -= 1;
        }
        let keep: Vec<usize> = p.object_offsets().into_iter().filter(|o| (start..end).contains(o)).collect();
        if p.delete_except(start, end, &keep).is_ok() {
            n += 1;
            floor = start;
        }
    }
    n
}
