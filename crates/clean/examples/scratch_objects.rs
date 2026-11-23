fn main() {
    let args: Vec<String> = std::env::args().collect();
    let bytes = std::fs::read(&args[1]).unwrap();
    let model = wordcraft_clean::Model::default();
    let scan = wordcraft_clean::objects::scan(&bytes, None, &model).unwrap();
    println!("pages {:?} unreadable {:?}", scan.pages, scan.unreadable);
    for o in &scan.objects {
        println!("{:6} {:7} {:>4.0}% {:?} g={} bbox={:?} px={:?} {:?} key={} why={:?}", o.id, o.kind.name(), o.score * 100.0, o.decision, o.group, o.bbox.map(|v| v.round()), o.pixels, o.text, o.key, o.reasons);
    }
    if let Some(out) = args.get(2) {
        let ids: Vec<String> = scan.objects.iter().filter(|o| o.decision == wordcraft_clean::Decision::Remove).map(|o| o.id.clone()).collect();
        let (pdf, r) = wordcraft_clean::objects::remove(&bytes, None, &ids).unwrap();
        std::fs::write(out, &pdf).unwrap();
        println!("removed {r:?}, {} -> {} bytes", bytes.len(), pdf.len());
        let again = wordcraft_clean::objects::scan(&pdf, None, &model).unwrap();
        for o in &again.objects { println!("after {:6} {:7} {:>4.0}% {:?}", o.id, o.kind.name(), o.score*100.0, o.text); }
