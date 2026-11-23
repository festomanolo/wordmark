//! Objects drawn over a PDF's pages (`wordcraft_clean::objects`), on scanner-app style PDFs
//! written here (our own fixtures: tiny flat-colour pictures, standard Helvetica text).

use wordcraft_clean::detect::{Decision, Model};
use wordcraft_clean::objects::{Kind, PageObject, Scan, keys_of, remove, scan};

/// A 2×2 RGB picture of one colour, as an image XObject body.
fn picture(colour: u8) -> String {
    let px = char::from(colour).to_string().repeat(12);
    format!("<< /Type /XObject /Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Length 12 >>\nstream\n{px}\nendstream")
}

/// A page like a scanner app's: the scan (a full-page picture), the app's name in the bottom
/// right corner under a link to its site, a line of text, and `extra` content drawn after the
/// scan. Each page has its own pictures, as such apps write them.
fn scanned(pages: &[&str]) -> Vec<u8> {
    scanned_with(&pages.iter().map(|p| ("", *p)).collect::<Vec<_>>())
}

/// As [`scanned`], with each page's `(before, after)` content drawn before and after the scan.
fn scanned_with(pages: &[(&str, &str)]) -> Vec<u8> {
    let n = pages.len();
    // 1 catalog, 2 pages, 3 font, then per page: page, content, scan picture, stamp picture.
    let mut objs: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        format!("<< /Type /Pages /Kids [{}] /Count {n} >>", (0..n).map(|i| format!("{} 0 R", 4 + 4 * i)).collect::<Vec<_>>().join(" ")),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
    ];
    for (i, (before, extra)) in pages.iter().enumerate() {
        let o = 4 + 4 * i;
        objs.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Contents {} 0 R /Resources << /Font << /F1 3 0 R >> /XObject << /Scan {} 0 R /Stamp {} 0 R >> >> \
             /Annots [<< /Type /Annot /Subtype /Link /Rect [485 0 595 45] /A << /S /URI /URI (https://scanapp.example/get) >> >>] >>",
            o + 1,
            o + 2,
            o + 3
        ));
        let c = format!(
            "BT /F1 13 Tf 510 15 Td (ScanApp) Tj ET\n{before}\nq 550 0 0 777 22 45 cm /Scan Do Q\nBT /F1 11 Tf 72 {} Td ({}) Tj ET\n{extra}\n",
            700 - 40 * i,
            LINES[i % LINES.len()]
        );
        objs.push(format!("<< /Length {} >>\nstream\n{c}\nendstream", c.len() + 1));
        objs.push(picture(b'a' + i as u8));
        objs.push(picture(b'P' + i as u8));
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

/// Each page's own line of text (pages that read alike are not what is tested here).
const LINES: [&str; 3] = ["Answer all questions", "Show your working clearly", "Calculators may be used"];

/// A logo pasted over the middle of the scan.
const LOGO: &str = "q 224 0 0 31 186 377 cm /Stamp Do Q";

fn find(s: &Scan, page: usize, kind: Kind, f: impl Fn(&PageObject) -> bool) -> &PageObject {
    s.objects.iter().find(|o| o.page == page && o.kind == kind && f(o)).unwrap_or_else(|| panic!("no {kind:?} on page {page}: {:#?}", s.objects))
}

/// The scan is the page's content: big, and not drawn over anything.
fn is_scan(o: &PageObject) -> bool {
    o.bbox[2] - o.bbox[0] > 400.0
}

#[test]
fn a_logo_over_the_scan_and_the_apps_footer_and_link_are_watermarks_and_the_scan_is_not() {
    let s = scan(&scanned(&[LOGO, "", ""]), None, &Model::default()).expect("reads");
    let logo = find(&s, 1, Kind::Image, |o| !is_scan(o));
    let footer = find(&s, 2, Kind::Text, |o| o.text == "ScanApp");
    let link = find(&s, 3, Kind::Link, |_| true);
    for o in [logo, footer, link] {
        assert_eq!(o.decision, Decision::Remove, "{o:#?}");
    }
    let page_scan = find(&s, 2, Kind::Image, is_scan);
    let question = find(&s, 2, Kind::Text, |o| o.text == LINES[1]);
    for o in [page_scan, question] {
        assert_eq!(o.decision, Decision::Keep, "{o:#?}");
        assert!(o.score < logo.score && o.score < footer.score, "{o:#?}");
    }
    // The footer on every page is one thing to tick.
    let groups: Vec<&str> = s.objects.iter().filter(|o| o.text == "ScanApp").map(|o| o.group.as_str()).collect();
    assert!(groups.len() == 3 && groups.iter().all(|g| *g == groups[0]), "{groups:?}");
}

#[test]
fn removing_takes_out_only_what_was_picked_and_keeps_the_rest_of_the_page() {
    let input = scanned(&[LOGO, ""]);
    let before = scan(&input, None, &Model::default()).expect("reads");
    let ids: Vec<String> = before.objects.iter().filter(|o| o.decision == Decision::Remove).map(|o| o.id.clone()).collect();
    let (output, done) = remove(&input, None, &ids).expect("writes");
    assert_eq!(done.total(), ids.len());
    assert!(done.not_found.is_empty(), "{done:?}");

    let after = scan(&output, None, &Model::default()).expect("the output reads");
    assert!(after.objects.iter().all(|o| o.decision != Decision::Remove), "{:#?}", after.objects);
    assert!(!after.objects.iter().any(|o| o.kind == Kind::Link || o.text == "ScanApp"), "{:#?}", after.objects);
    for page in 1..=2 {
        find(&after, page, Kind::Image, is_scan);
        find(&after, page, Kind::Text, |o| o.text == LINES[page - 1]);
    }
    // The pasted logo is gone from the file, not only from the page: no older revision keeps it.
    assert!(!output.windows(12).any(|w| w == "PPPPPPPPPPPP".as_bytes()), "the logo's pixels are still in the file");
}

#[test]
fn a_picture_marked_once_is_recognised_where_it_sits_in_the_next_file_from_that_source() {
    // A small mark in the corner beside the scan: nothing about it says watermark yet.
    let corner = "q 18 0 0 18 0 0 cm /Stamp Do Q";
    let first = scanned(&[corner]);
    let mut model = Model::default();
    let s = scan(&first, None, &model).expect("reads");
    let mark = find(&s, 1, Kind::Image, |o| !is_scan(o));
    assert_ne!(mark.decision, Decision::Remove, "{mark:#?}");

    model.learn_objects(&keys_of(&s, &[mark.id.clone()]), &[]);
    // The next file has a different picture (other pixels) in the same place.
    let next = scanned(&["", corner]);
    let s = scan(&next, None, &model).expect("reads");
    let again = find(&s, 2, Kind::Image, |o| !is_scan(o));
    assert_eq!(again.decision, Decision::Remove, "{again:#?}");
}

#[test]
fn a_mark_in_the_same_spot_on_every_page_gets_one_verdict_though_only_some_pages_draw_it_over_the_scan() {
    let mark = "q 130 0 0 20 232 800 cm /Stamp Do Q";
    let pdf = scanned_with(&[("", mark), (mark, ""), ("", mark)]);
    let s = scan(&pdf, None, &Model::default()).unwrap();
    let marks: Vec<&PageObject> = s.objects.iter().filter(|o| o.kind == Kind::Image && o.bbox[3] < 60.0).collect();
    assert_eq!(marks.len(), 3, "{marks:#?}");
    assert!(marks.iter().all(|m| m.decision == Decision::Remove), "{marks:#?}");
}
