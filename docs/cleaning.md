# Cleaning watermarks and web addresses

`wordcraft-cli clean` turns a batch of PDFs and documents into editable files without their
watermarks, links and web addresses. The work is in `crates/clean` (`wordcraft-clean`).

```sh
# Clean a folder of PDFs and Word files into editable .docx files.
wordcraft-cli clean ./inbox --out ./clean

# Teach the detector what this source's watermarks look like, from one sample.
wordcraft-cli clean-learn ./inbox/sample.pdf

# Decide what the detector was unsure of; it learns from the answers and cleans those files again.
wordcraft-cli clean-review ./clean/clean-report.json
```

## In the app

Design › Watermark › **Find and Remove Watermarks…** opens the Remove Watermarks pane:

1. **Choose PDF…** reads the PDF and finds what is drawn over its pages: pictures (a site's
   logo pasted over a scanned page), text (a scanner app's name in the corner), link areas and
   watermark or stamp annotations.
2. **Check the pages** shows each page with a box round everything found. A crossed box is
   removed, a dashed box is one the detector is not sure of (kept unless ticked), a thin box is
   kept. Previous Page and Next Page move through the file.
3. **Tick what to remove.** Things that are the same on several pages (a footer on every page)
   are one row: "95% watermark · Picture, bottom right · pages 1–3". Ticking a row shows its
   page. The page's own picture (the scan) and ordinary body text are not listed.
4. **Save Clean PDF…** writes a new PDF without the ticked items. Nothing else changes: the
   scan, fonts and layout of every page stay as they were. The file is written in full, so the
   removed items are not left behind in an older revision of it, and pictures no page draws any
   more are left out. **Learn Only** keeps the picks and scans again.

What you tick is remembered, and so is a dashed box you looked at and left unticked (as
content, so the next file doesn't ask about it again). It is all kept in `watermark-model.json` in the app's settings folder, the same file
format `--model` takes. Pictures are remembered by their pixels and by where they sit on the
page, links by their site, text by what it reads, so the next PDF from the same source is marked
without asking, even when its logo is a different picture file.

**Or clean a whole folder:** **Choose Folder…** picks a folder of PDFs, and **Start Cleaning**
cleans every PDF in it (and in its subfolders, unless that is unticked) with what has been
learned. Each clean copy is saved next to its PDF, wherever it is in the folder tree, named
`<name>-clean.pdf` (the originals are never written over, and a later run skips the `-clean`
copies). A bar shows how
far it has got, and **Stop** stops it after the files under way. What the detector is sure of is
removed; a file with things it is not sure of is written with them kept and listed with a
**Check** button, which opens it in the pane with its pages marked, and **Save to Clean Folder**
writes its clean copy again with your picks (which are learned, so **Clean Again** gets further).
When the run ends a notification with a sound says how many files need a check, and the Dock
icon bounces (macOS: Notification Centre; Linux and the BSDs: `notify-send`; Windows and the web
app only flash the window). Cleaning a folder needs the desktop app.

**Open as Editable Document** instead rebuilds the PDF's text as paragraphs, without the ticked
text, with web addresses removed, unlinked or kept as chosen. Pictures are not carried over, so a
scanned PDF comes out empty: save a clean PDF for those. **Clean Document1** (the button names the document) cleans the
document open in the editor (not a PDF): its watermark, learned watermark texts, links and web addresses. Undo puts
them back.

Agents get the same results through the commands the pane runs:

| Command | Does |
| --- | --- |
| `clean.scan {path}` | lists the PDF's texts (`candidates`) and the objects drawn on its pages (`objects`: `id`, `page`, `kind`, `bbox`, `text`, `group`, `score`, `decision`, `reasons`), with `pageSizes` |
| `clean.learn {path, model, watermarks?, content?, watermarkObjects?, contentObjects?}` | teaches the model texts and objects (by id) and saves it |
| `clean.savePdf {path, out, remove?, keep?}` | writes the PDF without what the model removes, plus the `remove` ids, less the `keep` ids |
| `clean.folder {dir, out?, subfolders?}` | cleans every PDF in `dir` into `out` (default `<dir>-clean` beside it); each of `files` has `removed`, `unsure` (kept for a person to check) and `error` |
| `clean.openPdf {path, model?, watermarks?, content?, urls?, dropFlagged?}` | opens the PDF rebuilt without its watermarks |
| `clean.document {model?, urls?}` | cleans the open document |

`ui.dialog {"name": "removeWatermarks", "path": "file.pdf"}` opens the pane already scanning a
PDF; `{"name": "removeWatermarks", "folder": "dir", "out"?: "dir", "start"?: true}` sets the
folder to clean (and starts it). Confined automation (`--automation-read-root`/`--automation-write-root`) reaches only files
inside its roots.

## What happens to a file

| Input | Steps |
| --- | --- |
| PDF | Its text is read glyph by glyph, with position, size, angle, colour, opacity and marked-content tags. Glyphs on one baseline are grouped into spans. Each span is scored as a watermark or not. The kept spans are rebuilt into paragraphs and headings, and each PDF page starts a new page. |
| .docx, .doc, .odt, .rtf, .html, .md… | The document's own watermark (Design › Watermark) is removed, along with paragraphs and phrases that read like a watermark someone marked, then links and web addresses. Body, tables, headers, footers and notes are all cleaned. |

The output format is `--format`: docx by default, or odt, rtf, md, html, txt, pdf or json.

## How a page object is judged

`clean.scan`'s `objects` are scored from these signals (each object's `reasons` names the ones
that applied):

| Signal | Means |
| --- | --- |
| drawn over the page picture | a picture drawn after, and on top of, a picture that covers the page (a logo pasted on a scan). Text over such a picture only leans this way: a designed page's real text often sits on a background picture |
| on most pages | the same thing in the same spot on at least half the pages |
| page margin | in the top or bottom tenth, or the outer side strips |
| under a link | overlaps a link area |
| web link | a link to a web address (links within the document are kept) |
| see-through, turned, pale, large, middle of the page | as for text spans below |
| page picture | (against) covers 40% of the page or more: the page itself |
| invisible text | (against) an OCR layer |
| marked as a watermark / content before | what the model learned |

## How a span is judged

The detector is a logistic regression over these features of a span:

| Feature | Watermarks tend to… |
| --- | --- |
| `rotated` | be turned off the baseline (diagonal stamps, sideways margin marks) |
| `repeats` | appear on most pages (digits are ignored, so "Page 3" matches "Page 4") |
| `transparent` | be drawn see-through |
| `pale` | be drawn in a light colour |
| `large` | be set much larger than the body text |
| `centred` | sit near the middle of the page |
| `artifact` | be marked as an `/Artifact` (not content) |
| `link_like` | contain a web address, or "downloaded from", "licensed to"… |
| `page_edge` | sit in the top or bottom margin (learned; neutral at first) |
| `learned_watermark` | read like a text someone marked as a watermark |
| `learned_content` | (against) read like a text someone marked as content |
| `invisible` | (against) be invisible text, usually the OCR layer of a scan |

A new model starts from hand-set weights that catch common stamps: diagonal "CONFIDENTIAL" or
"DRAFT", pale repeated footers. Running headers and page numbers are kept. Each span gets one of
three decisions:

- **remove**, at a score of 0.8 or more;
- **flagged**, from 0.3 to 0.8. A flagged span stays in the output (unless you pass
  `--drop-flagged`) and is listed in the report for `clean-review`;
- **keep**, below 0.3.

## Teaching it

`clean-learn FILE` lists the file's most watermark-like texts and asks which are real watermarks
and which are not. You can also pass them on the command line: `--mark "Licensed to"`,
`--not "Page 1"`. Each answer is a labelled example:

- The weights are retrained on all the examples, pulled back towards the starting weights so a
  few answers can't swing it wildly. The model learns what this source's stamps look like.
- The text is remembered, so a span that reads alike is recognised even with a different name or
  date in it. "Licensed copy for Alice" teaches "Licensed copy for Baraka".
- A text marked as content is never removed without asking.

`clean-review` asks about every flagged text once, however many pages or files it appears on.
It learns from the answers, cleans those files again and updates the report. The model is a JSON
file: `--model`, default `./watermark-model.json`.

## Speed

`clean` runs a batch as three stages at the same time: read (load, parse the PDF's pages), clean
(score, rebuild) and write. Bounded queues join them, and each stage has `--jobs` workers (default:
one per CPU), so reading, computing and writing different files overlap. Within a file, the
features of its pages are computed in parallel.

## Limits

- A scanned page with no text layer is a picture. Its words, and a watermark baked into the
  picture's pixels, are not read. The report lists those pages under `needs_ocr`, and `clean`
  warns about them. Run OCR on the file first; a scan with an OCR layer is read through that layer.
- Saving a clean PDF removes whole objects. A watermark baked into a scan's pixels, or drawn
  with vector paths, is not found; pictures and text inside a form XObject are removed with
  their form, as one object.
- A rebuilt PDF keeps its text, paragraphs and headings. Pictures, tables (read as lines of
  text), columns (read across, line by line) and fonts are not carried over yet.
- Links in a PDF are not carried into the rebuilt document. Web addresses in its text are removed
  with `--urls remove` (the default) and kept otherwise.
- Pages the reader could not finish are listed under `incomplete_pages`.
