fn main() {
    let args: Vec<String> = std::env::args().collect();
    let bytes = std::fs::read(&args[1]).unwrap();
    let model = wordcraft_clean::Model::default();
    let scan = wordcraft_clean::objects::scan(&bytes, None, &model).unwrap();
    println!("pages {:?} unreadable {:?}", scan.pages, scan.unreadable);
