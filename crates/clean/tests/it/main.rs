//! Cleaning behaviour, on PDFs written here (our own fixtures: text in the standard Helvetica
//! font, no embedded data).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use wordcraft_clean::detect::{Decision, Model};
use wordcraft_clean::doc_clean::clean_document;
use wordcraft_clean::pipeline::{DocIo, Job, Options, clean_pdf, run};
use wordcraft_clean::{FileReport, RebuildOptions, UrlAction, candidates, extract_pdf};
use wordcraft_doc::{CharProps, Document, Paragraph, StoryRef, Watermark, para_block};

/// A PDF with one Helvetica font, a 30%-opacity graphics state and these page content streams.
fn pdf(pages: &[String]) -> Vec<u8> {
    let n = pages.len();
    // 1 catalog, 2 pages, 3 font, 4 gstate, then page/content pairs.
    let mut objs: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        format!("<< /Type /Pages /Kids [{}] /Count {n} >>", (0..n).map(|i| format!("{} 0 R", 5 + 2 * i)).collect::<Vec<_>>().join(" ")),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".into(),
        "<< /Type /ExtGState /ca 0.3 /CA 0.3 >>".into(),
    ];
    for (i, c) in pages.iter().enumerate() {
        objs.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> /ExtGState << /GS1 4 0 R >> >> /Contents {} 0 R >>",
            6 + 2 * i
        ));
        objs.push(format!("<< /Length {} >>\nstream\n{c}\nendstream", c.len() + 1));
    }
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes());
    out
}

fn text(x: f32, y: f32, size: f32, s: &str) -> String {
    format!("BT /F1 {size} Tf {x} {y} Td ({s}) Tj ET\n")
}

/// A report page: two body lines (the second with a web address), a running header, a page
/// number, a diagonal see-through "CONFIDENTIAL" stamp and a small grey download footer, plus
/// `extra` (more content).
fn page(n: usize, extra: &str) -> String {
    let mut c = String::new();
    c += &text(72.0, 700.0, 11.0, &format!("Body line one of page {n} with several ordinary words."));
    c += &text(72.0, 686.0, 11.0, "The second line says see https://foo.org/x for details today.");
    c += &text(72.0, 760.0, 10.0, "ACME Quarterly Report");
    c += &text(290.0, 40.0, 9.0, &format!("Page {n}"));
    c += "q /GS1 gs 0.75 0.75 0.75 rg BT /F1 64 Tf 0.7071 0.7071 -0.7071 0.7071 160 260 Tm (CONFIDENTIAL) Tj ET Q\n";
    c += "q 0.6 0.6 0.6 rg BT /F1 7 Tf 72 20 Td (Downloaded from www.example-library.com) Tj ET Q\n";
    c += extra;
    c
}

fn report_pdf(extra: &str) -> Vec<u8> {
    pdf(&(1..=3).map(|n| page(n, extra)).collect::<Vec<_>>())
}

fn body_text(doc: &Document) -> String {
    doc.plain_text(StoryRef::Body)
}

#[test]
fn spans_carry_angle_opacity_and_text() {
    let pages = extract_pdf(&report_pdf(""), None).expect("reads");
    assert_eq!(pages.len(), 3);
    let spans = &pages[0].spans;
    let stamp = spans.iter().find(|s| s.text.contains("CONFIDENTIAL")).expect("stamp read");
    let body = spans.iter().find(|s| s.text.starts_with("Body line one")).expect("body read");
    assert!(stamp.angle > 30.0 && stamp.angle < 60.0, "the stamp is turned about 45° ({})", stamp.angle);
    assert!(body.angle.abs() < 1.0, "body text is level");
    assert!(stamp.alpha < body.alpha, "the stamp is see-through and the body is not");
    assert!(stamp.size > body.size * 3.0, "the stamp is set much larger");
    assert!(body.text.contains("several ordinary words"), "word spaces survive: {:?}", body.text);
}

#[test]
fn default_model_removes_the_stamp_and_keeps_content() {
    let pages = extract_pdf(&report_pdf(""), None).expect("reads");
    let mut report = FileReport::default();
    let doc = clean_pdf(&pages, &Model::default(), &RebuildOptions::default(), &mut report);
    let t = body_text(&doc);
    assert!(!t.contains("CONFIDENTIAL"), "stamp removed: {t}");
    assert!(t.contains("Body line one of page 2"), "body kept: {t}");
    assert!(t.contains("ACME Quarterly Report"), "running header is content: {t}");
    assert!(!t.contains("foo.org"), "web address removed from the text: {t}");
    assert!(t.contains("for details today"), "the sentence around it stays: {t}");
    assert!(report.removed.iter().all(|i| !i.text.contains("Body line")), "no body line was removed");
    assert_eq!(report.removed.iter().filter(|i| i.text.contains("CONFIDENTIAL")).count(), 3, "one stamp per page");
}

#[test]
fn urls_can_be_kept() {
    let pages = extract_pdf(&report_pdf(""), None).expect("reads");
    let opts = RebuildOptions { urls: UrlAction::Keep, ..Default::default() };
    let doc = clean_pdf(&pages, &Model::default(), &opts, &mut FileReport::default());
    assert!(body_text(&doc).contains("https://foo.org/x"));
}

#[test]
fn learning_from_marked_examples_catches_the_same_stamp_elsewhere() {
    // A plain, level, black licence line on every page: nothing marks it as a stamp, so the
    // starting model keeps it.
    let licence = |who: &str| text(200.0, 60.0, 9.0, &format!("Licensed copy for {who} only"));
    let first = extract_pdf(&report_pdf(&licence("Alice Moyo")), None).expect("reads");
    let untrained = Model::default();
    let before = untrained.classify(&first);
    let is_licence = |t: &str| t.starts_with("Licensed copy");
    assert!(before.iter().filter(|s| is_licence(&s.text)).all(|s| s.decision != Decision::Remove), "not removed before learning");

    // The person marks the licence line as a watermark and the page number as content.
    let mut model = Model::default();
    let picks = candidates(&first, &model, 50);
    let labelled: Vec<_> = picks
        .iter()
        .filter(|c| is_licence(&c.text) || c.text.starts_with("Page"))
        .map(|c| (c.text.clone(), c.features, is_licence(&c.text)))
        .collect();
    assert_eq!(labelled.len(), 2);
    model.learn(labelled);

    // Another file from the same source, licensed to someone else.
    let second = extract_pdf(&report_pdf(&licence("Baraka Juma")), None).expect("reads");
    let after = model.classify(&second);
    let lic: Vec<_> = after.iter().filter(|s| is_licence(&s.text)).collect();
    assert!(!lic.is_empty());
    assert!(lic.iter().all(|s| s.decision == Decision::Remove), "learned: {:?}", lic.iter().map(|s| s.score).collect::<Vec<_>>());
    assert!(after.iter().filter(|s| s.text.starts_with("Page")).all(|s| s.decision == Decision::Keep), "marked content stays");
    assert!(after.iter().filter(|s| s.text.starts_with("Body line")).all(|s| s.decision == Decision::Keep), "body still kept");

    // The model survives a save and load.
    let reloaded: Model = serde_json::from_str(&model.to_json()).expect("parses");
    assert_eq!(
        reloaded.classify(&second).iter().filter(|s| s.decision == Decision::Remove).count(),
        after.iter().filter(|s| s.decision == Decision::Remove).count()
    );
}

#[test]
fn unsure_spans_are_flagged_not_removed() {
    // Level, grey, small text in the middle of the page on every page: some stamp signals,
    // not enough to be sure.
    let extra = "q 0.7 0.7 0.7 rg BT /F1 12 Tf 230 400 Td (Internal use) Tj ET Q\n";
    let pages = extract_pdf(&report_pdf(extra), None).expect("reads");
    let model = Model::default();
    let scored = model.classify(&pages);
    let s = scored.iter().find(|s| s.text == "Internal use").expect("read");
    assert_eq!(s.decision, Decision::Review, "score {}", s.score);
    let mut report = FileReport::default();
    let doc = clean_pdf(&pages, &model, &RebuildOptions::default(), &mut report);
    assert!(body_text(&doc).contains("Internal use"), "flagged text stays until a person decides");
    assert!(report.flagged.iter().any(|i| i.text == "Internal use"));
    let dropped = clean_pdf(&pages, &model, &RebuildOptions { drop_flagged: true, ..Default::default() }, &mut FileReport::default());
    assert!(!body_text(&dropped).contains("Internal use"));
}

#[test]
fn editable_documents_lose_watermark_links_and_learned_text() {
    let mut doc = Document::new();
    doc.settings.watermark = Some(Watermark::default());
    let mut linked = Paragraph::with_text("Visit our portal or https://tracker.example.net/a?b=1 now.", CharProps::default());
    linked.format(6, 16, &|c| c.link = Some("https://portal.example.com".into())).expect("in range");
    let stamp = Paragraph::with_text("Licensed copy for Neema only", CharProps::default());
    let keep = Paragraph::with_text("An ordinary sentence stays.", CharProps::default());
    doc.body = vec![para_block(linked), para_block(stamp), para_block(keep)];
    let mut model = Model::default();
    model.learn([("Licensed copy for Alice Moyo only".to_string(), [0.0; wordcraft_clean::detect::N], true)]);

    let st = clean_document(&mut doc, &model, UrlAction::Remove);
    assert!(st.native_watermark && doc.settings.watermark.is_none());
    let t = body_text(&doc);
    assert!(!t.contains("tracker.example"), "{t}");
    assert!(t.contains("Visit our portal"), "{t}");
    assert!(!t.contains("Licensed copy"), "{t}");
    assert!(t.contains("An ordinary sentence stays."));
    let links = doc.body.iter().filter_map(|b| b.as_para()).flat_map(|p| p.runs.iter()).filter(|r| r.props.link.is_some()).count();
    assert_eq!(links, 0);
}

/// Saves documents as their plain text; opens `.txt`.
struct TextIo;
impl DocIo for TextIo {
    fn open(&self, _: &Path, bytes: &[u8]) -> Result<Document, String> {
        Ok(Document::from_text(&String::from_utf8_lossy(bytes)))
    }
    fn save(&self, path: &Path, doc: &Document) -> Result<(), String> {
        std::fs::write(path, body_text(doc)).map_err(|e| e.to_string())
    }
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("wordcraft-clean-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("temp dir");
    d
}

#[test]
fn batch_runs_every_file_and_reports_in_order() {
    let dir = scratch("batch");
    let mut jobs = Vec::new();
    for i in 0..7 {
        let input = dir.join(format!("in{i}.pdf"));
        std::fs::write(&input, report_pdf("")).expect("write");
        jobs.push(Job { input, output: dir.join("out").join(format!("in{i}.txt")) });
    }
    let txt = dir.join("note.txt");
    std::fs::write(&txt, "Read www.spam-site.com today").expect("write");
    jobs.push(Job { input: txt, output: dir.join("out").join("note.txt") });
    jobs.push(Job { input: dir.join("missing.pdf"), output: dir.join("out").join("missing.txt") });

    let opts = Options { model: Arc::new(Model::default()), rebuild: RebuildOptions::default(), password: None, workers: 3 };
    let seen = std::sync::atomic::AtomicUsize::new(0);
    let reports = run(&jobs, &opts, &TextIo, &|_| {
        seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    });
    assert_eq!(reports.len(), jobs.len());
    assert_eq!(seen.into_inner(), jobs.len());
    for (r, j) in reports.iter().zip(&jobs) {
        assert_eq!(r.input, j.input, "reports come back in job order");
    }
    for r in &reports[..7] {
        assert!(r.ok(), "{}", r.error);
        let out = std::fs::read_to_string(&r.output).expect("written");
        assert!(out.contains("Body line one") && !out.contains("CONFIDENTIAL"));
    }
    let note = std::fs::read_to_string(&reports[7].output).expect("written");
    assert!(!note.contains("spam-site") && note.contains("today"), "{note}");
    assert!(!reports[8].ok(), "a missing file is an error, not a success");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_scan_without_text_is_reported_for_ocr() {
    // A page that only paints an image covering it.
    let img = "q 612 0 0 792 0 0 cm BI /W 2 /H 2 /CS /G /BPC 8 ID abcd EI Q";
    let pages = extract_pdf(&pdf(&[img.to_string()]), None).expect("reads");
    let mut report = FileReport::default();
    clean_pdf(&pages, &Model::default(), &RebuildOptions::default(), &mut report);
    assert_eq!(report.needs_ocr, vec![1]);
}
