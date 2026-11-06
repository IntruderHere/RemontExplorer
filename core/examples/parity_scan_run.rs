//! Одноразовый пробник для cross-language parity: сканирует папку в CSV.
//! Использование: parity_scan_run <root> <output.csv>

use std::path::PathBuf;

use filemanager_core::{scan_folder, stdout_sink, StopToken};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: parity_scan_run <root> <output.csv>");
        std::process::exit(2);
    }
    let root = PathBuf::from(&args[1]);
    let out = PathBuf::from(&args[2]);
    let stop = StopToken::new();
    let log = stdout_sink();
    match scan_folder(&root, Some(&out), 4, &stop, &log) {
        Some((path, stats)) => println!("RUST_SCAN_OK files={} {}", stats.files, path.display()),
        None => {
            eprintln!("RUST_SCAN_FAIL");
            std::process::exit(1);
        }
    }
}
