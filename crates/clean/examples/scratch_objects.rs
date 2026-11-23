fn main() {
    let args: Vec<String> = std::env::args().collect();
    let bytes = std::fs::read(&args[1]).unwrap();
    let model = wordcraft_clean::Model::default();
    let scan = wordcraft_clean::objects::scan(&bytes, None, &model).unwrap();
    println!("pages {:?} unreadable {:?}", scan.pages, scan.unreadable);
    for o in &scan.objects {
        println!("{:6} {:7} {:>4.0}% {:?} g={} bbox={:?} px={:?} {:?} keys={:?} why={:?}", o.id, o.kind.name(), o.score * 100.0, o.decision, o.group, o.bbox.map(|v| v.round()), o.pixels, o.text, o.keys, o.reasons);
    }
    if let Some(out) = args.get(2) {
        let ids: Vec<String> = scan.objects.iter().filter(|o| o.decision == wordcraft_clean::Decision::Remove).map(|o| o.id.clone()).collect();
        let (pdf, r) = wordcraft_clean::objects::remove(&bytes, None, &ids).unwrap();
        std::fs::write(out, &pdf).unwrap();
        println!("removed {r:?}, {} -> {} bytes", bytes.len(), pdf.len());
        let again = wordcraft_clean::objects::scan(&pdf, None, &model).unwrap();
        for o in &again.objects { println!("after {:6} {:7} {:>4.0}% {:?}", o.id, o.kind.name(), o.score*100.0, o.text); }
        for p in 0..again.pages.len() {
            let pic = wordcraft_clean::objects::render_page(&pdf, None, p, 900).unwrap();
            println!("page {} rendered {}x{}", p + 1, pic.width, pic.height);
            let mut ppm = format!("P6 {} {} 255\n", pic.width, pic.height).into_bytes();
            for px in pic.rgba.chunks(4) { ppm.extend_from_slice(&px[..3]); }
            std::fs::write(format!("{out}.p{}.ppm", p + 1), ppm).unwrap();
        }
    }
}
