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
