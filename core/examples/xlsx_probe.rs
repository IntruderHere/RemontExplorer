fn main() {
    let a: Vec<String> = std::env::args().collect();
    filemanager_core::csv_to_xlsx(std::path::Path::new(&a[1]), std::path::Path::new(&a[2])).unwrap();
    println!("ok");
}
