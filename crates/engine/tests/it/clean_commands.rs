//! The `clean.*` commands the Remove Watermarks pane runs: picks apply to the PDF opened, and
//! once learned they apply to the next PDF from the same source. PDFs are written here (our own
//! fixtures: text in the standard Helvetica font).

use std::path::PathBuf;

use serde_json::{Value, json};
use wordcraft_doc::{Document, StoryRef};
use wordcraft_engine::Session;

/// A PDF whose pages each have body text, a see-through diagonal "DRAFT COPY" stamp and a plain
/// licence line naming `who` (the stamp a person has to point out).
fn pdf(who: &str) -> Vec<u8> {
    let page = |n: usize| {
        format!(
            "BT /F1 11 Tf 72 700 Td (Page {n} body text describes the survey in ordinary words.) Tj ET\n\
             BT /F1 11 Tf 72 686 Td (A second line of content continues the paragraph here.) Tj ET\n\
             q /GS1 gs 0.7 0.7 0.7 rg BT /F1 60 Tf 0.7071 0.7071 -0.7071 0.7071 150 250 Tm (DRAFT COPY) Tj ET Q\n\
             BT /F1 9 Tf 72 40 Td (Licensed copy for {who} only) Tj ET\n"
        )
    };
    let pages: Vec<String> = (1..=3).map(page).collect();
    let mut objs: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        format!("<< /Type /Pages /Kids [5 0 R 7 0 R 9 0 R] /Count {} >>", pages.len()),
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

fn scratch(test: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("wordcraft-clean-{test}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write_pdf(dir: &std::path::Path, name: &str, who: &str) -> String {
    let p = dir.join(name);
    std::fs::write(&p, pdf(who)).unwrap();
    p.to_string_lossy().into_owned()
}

fn text(s: &Session) -> String {
    s.doc.plain_text(StoryRef::Body)
}

fn candidate<'a>(scan: &'a Value, starts: &str) -> &'a Value {
    scan["candidates"].as_array().unwrap().iter().find(|c| c["text"].as_str().unwrap().starts_with(starts)).expect("listed")
}

#[test]
fn picks_apply_to_the_pdf_opened_and_once_learned_to_the_next_one() {
    let dir = scratch("picks");
    let first = write_pdf(&dir, "first.pdf", "Alice Moyo");
    let second = write_pdf(&dir, "second.pdf", "Baraka Juma");
    let model = dir.join("model.json").to_string_lossy().into_owned();
    let mut s = Session::new(Document::new());

    let scan = s.run("clean.scan", &json!({"path": first})).unwrap();
    assert_eq!(candidate(&scan, "DRAFT COPY")["decision"], "remove", "the stamp is found without help");
    assert_ne!(candidate(&scan, "Licensed copy")["decision"], "remove", "the plain licence line needs pointing out");

    // Without a model file: the ticked line goes from the PDF opened, the body stays.
    let licence = candidate(&scan, "Licensed copy")["text"].clone();
    s.run("clean.openPdf", &json!({"path": first, "watermarks": [licence]})).unwrap();
    let opened = text(&s);
    assert!(!opened.contains("Licensed copy") && !opened.contains("DRAFT COPY"), "{opened}");
    assert!(opened.contains("ordinary words"), "content kept: {opened}");

    // Learned and kept: the next PDF from the source loses its own licence line, another name in it.
    s.run("clean.learn", &json!({"path": first, "model": model, "watermarks": [licence]})).unwrap();
    s.run("clean.openPdf", &json!({"path": second, "model": model})).unwrap();
    let next = text(&s);
    assert!(!next.contains("Licensed copy"), "{next}");
    assert!(next.contains("ordinary words"), "{next}");

    // Unticking the stamp says it is content: it stays.
    s.run("clean.openPdf", &json!({"path": second, "content": ["DRAFT COPY"]})).unwrap();
    assert!(text(&s).contains("DRAFT COPY"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn naming_a_text_the_pdf_does_not_have_fails_instead_of_learning_nothing() {
    let dir = scratch("missing");
    let path = write_pdf(&dir, "only.pdf", "Alice");
    let model = dir.join("never.json");
    let mut s = Session::new(Document::new());
    let r = s.run("clean.learn", &json!({"path": path, "model": model, "watermarks": ["Nothing like this is in it"]}));
    assert!(r.is_err());
    assert!(!model.exists(), "no model was written");
    let _ = std::fs::remove_dir_all(dir);
}

/// The ids of `scan`'s objects whose text starts with `starts`, by page.
fn object_ids(scan: &Value, starts: &str) -> Vec<(u64, String)> {
    scan["objects"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|o| o["text"].as_str().is_some_and(|t| t.starts_with(starts)))
        .map(|o| (o["page"].as_u64().unwrap(), o["id"].as_str().unwrap().to_string()))
        .collect()
}

#[test]
fn a_saved_pdf_loses_what_was_picked_and_keeps_what_was_kept() {
    let dir = scratch("save");
    let input = write_pdf(&dir, "in.pdf", "Alice Moyo");
    let out = dir.join("out.pdf").to_string_lossy().into_owned();
    let mut s = Session::new(Document::new());
    let scan = s.run("clean.scan", &json!({"path": input})).unwrap();
    let stamps = object_ids(&scan, "DRAFT COPY");
    let licences = object_ids(&scan, "Licensed copy");
    assert_eq!((stamps.len(), licences.len()), (3, 3), "{scan:#}");
    let on = |ids: &[(u64, String)], page: u64| ids.iter().find(|(p, _)| *p == page).map(|(_, id)| id.clone()).unwrap();

    // Page 1's stamp is kept against the model; page 2's licence line is removed though the
    // model would keep it.
    let saved = s.run("clean.savePdf", &json!({"path": input, "out": out, "keep": [on(&stamps, 1)], "remove": [on(&licences, 2)]})).unwrap();
    assert_eq!(saved["removed"], 3, "{saved:#}");

    let after = s.run("clean.scan", &json!({"path": out})).unwrap();
    let pages = |ids: Vec<(u64, String)>| ids.into_iter().map(|(p, _)| p).collect::<Vec<_>>();
    assert_eq!(pages(object_ids(&after, "DRAFT COPY")), [1]);
    assert_eq!(pages(object_ids(&after, "Licensed copy")), [1, 3]);
    assert!(pages(object_ids(&after, "Page ")).len() == 3, "the body text stays on every page");
}

#[test]
fn each_pdf_in_a_folder_tree_gets_its_clean_copy_beside_it_and_a_second_run_leaves_the_copies_alone() {
    let base = scratch("folder");
    let dir = base.join("papers");
    std::fs::create_dir_all(dir.join("term 2")).unwrap();
    let a = write_pdf(&dir, "a.pdf", "Alice Moyo");
    write_pdf(&dir.join("term 2"), "b.PDF", "Baraka Juma");
    std::fs::write(dir.join("notes.txt"), "not a pdf").unwrap();
    let dir_s = dir.to_string_lossy().into_owned();
    let mut s = Session::new(Document::new());

    let r = s.run("clean.folder", &json!({"dir": dir_s})).unwrap();
    assert_eq!((r["pdfs"].as_u64(), r["failed"].as_u64()), (Some(2), Some(0)), "{r:#}");
    let out = std::path::PathBuf::from(r["out"].as_str().unwrap());
    assert_ne!(out, dir, "the copies don't overwrite the originals");
    for written in [out.join("a.pdf"), out.join("term 2").join("b.PDF")] {
        let scan = s.run("clean.scan", &json!({"path": written.to_string_lossy()})).unwrap();
        assert!(object_ids(&scan, "DRAFT COPY").is_empty(), "{} still has the stamp", written.display());
        assert_eq!(object_ids(&scan, "Page ").len(), 3, "the body text stays");
    }
    let original = s.run("clean.scan", &json!({"path": a})).unwrap();
    assert_eq!(object_ids(&original, "DRAFT COPY").len(), 3, "the original is untouched");

    assert!(s.run("clean.folder", &json!({"dir": dir_s, "out": dir_s})).is_err(), "writing over the originals is refused");
}
