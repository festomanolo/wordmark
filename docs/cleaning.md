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

## What happens to a file

| Input | Steps |
| --- | --- |
| PDF | Its text is read glyph by glyph, with position, size, angle, colour, opacity and marked-content tags. Glyphs on one baseline are grouped into spans. Each span is scored as a watermark or not. The kept spans are rebuilt into paragraphs and headings, and each PDF page starts a new page. |
| .docx, .doc, .odt, .rtf, .html, .md… | The document's own watermark (Design › Watermark) is removed, along with paragraphs and phrases that read like a watermark someone marked, then links and web addresses. Body, tables, headers, footers and notes are all cleaned. |

The output format is `--format`: docx by default, or odt, rtf, md, html, txt, pdf or json.

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
- A rebuilt PDF keeps its text, paragraphs and headings. Pictures, tables (read as lines of
  text), columns (read across, line by line) and fonts are not carried over yet.
- Links in a PDF are not carried into the rebuilt document. Web addresses in its text are removed
  with `--urls remove` (the default) and kept otherwise.
- Pages the reader could not finish are listed under `incomplete_pages`.
