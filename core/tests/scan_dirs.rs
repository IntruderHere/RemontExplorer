//! Сканирование: режим «только файлы» и «файлы и папки» (включая пустые).

use std::path::{Path, PathBuf};

use std::sync::{Arc, Mutex};

use remont_core::{scan_folder_opts, LogSink, ScanOptions, Sink, StopToken};

fn quiet() -> LogSink {
    Arc::new(Mutex::new(Sink::text_only(|_| {})))
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("scan_dirs_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn temp_out(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("scan_dirs_{}_{}.csv", name, std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

/// Дерево: `a/file1.txt`, пустая `empty`, `sub/` с пустой `sub/deep`.
fn build_tree(root: &Path) {
    std::fs::create_dir_all(root.join("a")).unwrap();
    std::fs::write(root.join("a/file1.txt"), b"12345").unwrap();
    std::fs::create_dir_all(root.join("empty")).unwrap();
    std::fs::create_dir_all(root.join("sub/deep")).unwrap();
}

fn read_report(path: &Path) -> (Vec<String>, Vec<Vec<String>>) {
    let bytes = std::fs::read(path).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let text = text.trim_start_matches('\u{feff}');
    let mut lines = text.lines();
    let header: Vec<String> = lines
        .next()
        .unwrap()
        .split(';')
        .map(|s| s.trim().to_string())
        .collect();
    let rows = lines
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.split(';').map(|s| s.trim().to_string()).collect())
        .collect();
    (header, rows)
}

#[test]
fn scan_files_only_skips_empty_folders() {
    let root = temp_dir("files");
    build_tree(&root);
    // Отчёт — вне сканируемого дерева, иначе он сам попадёт в отчёт.
    let out = temp_out("files");
    let opts = ScanOptions::default();

    let (report, stats) =
        scan_folder_opts(&root, Some(&out), 2, &StopToken::new(), &quiet(), &opts)
            .expect("скан не удался");

    let (header, rows) = read_report(&report);
    assert_eq!(header.len(), 7, "лишние колонки в режиме «только файлы»");
    assert!(!header.contains(&"type".to_string()));
    assert_eq!(stats.files, 1, "должен быть один файл");
    assert_eq!(stats.dirs, 0);
    assert_eq!(rows.len(), 1, "пустые папки не должны попадать в отчёт");
    assert!(rows[0][1].ends_with("file1.txt"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn scan_with_dirs_includes_empty_folders() {
    let root = temp_dir("dirs");
    build_tree(&root);
    let out = temp_out("dirs");
    let opts = ScanOptions { include_dirs: true };

    let (report, stats) =
        scan_folder_opts(&root, Some(&out), 2, &StopToken::new(), &quiet(), &opts)
            .expect("скан не удался");

    let (header, rows) = read_report(&report);
    let type_col = header
        .iter()
        .position(|h| h == "type")
        .expect("нет колонки type");
    assert_eq!(header.len(), 8);
    assert_eq!(stats.files, 1);
    assert!(stats.dirs >= 3, "ожидались папки a, empty, sub, deep");

    let dirs: Vec<&str> = rows
        .iter()
        .filter(|r| r[type_col] == "dir")
        .map(|r| r[3].as_str())
        .collect();
    let files: Vec<&str> = rows
        .iter()
        .filter(|r| r[type_col] == "file")
        .map(|r| r[3].as_str())
        .collect();

    assert_eq!(files, vec!["file1.txt"]);
    for expected in ["empty", "deep", "a", "sub"] {
        assert!(dirs.contains(&expected), "нет папки {expected} в отчёте");
    }
    // Пустая папка хранится без размера и расширения.
    let empty = rows
        .iter()
        .find(|r| r[type_col] == "dir" && r[3] == "empty")
        .expect("нет строки пустой папки");
    assert_eq!(empty[4], "");
    assert_eq!(empty[5], "0.00");
    let _ = std::fs::remove_dir_all(&root);
}
