//! Parity-тесты: дубликаты и очистка пустых папок.

use std::fs;

use filemanager_core::{
    cleanup_empty_folders, find_duplicates_multi, stdout_sink, HashAlgo, StopToken,
};
use tempfile::TempDir;

#[test]
fn duplicates_finds_same_content() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("sub")).unwrap();
    fs::write(dir.path().join("f1.bin"), b"same content here").unwrap();
    fs::write(dir.path().join("sub/f2.bin"), b"same content here").unwrap();
    fs::write(dir.path().join("unique.bin"), b"other bytes").unwrap();
    let out = dir.path().join("dups.csv");

    let stop = StopToken::new();
    let log = stdout_sink();
    let groups = find_duplicates_multi(
        &[dir.path().to_path_buf()],
        HashAlgo::Sha256,
        Some(&out),
        &stop,
        &log,
    )
    .expect("дубликаты");

    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].len(), 2);

    let text = fs::read_to_string(&out).unwrap();
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "group_id;full_path;size_bytes;hash");
    assert_eq!(lines.len(), 3, "шапка + 2 файла группы");
    assert!(lines[1].starts_with("1;"));
    assert!(lines[2].starts_with("1;"));
}

#[test]
fn cleanup_removes_empty_folders_single_pass() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("root");
    fs::create_dir_all(root.join("empty1")).unwrap();
    fs::create_dir_all(root.join("empty2/nested_empty")).unwrap();
    fs::write(root.join("keep.txt"), b"1").unwrap();

    let stop = StopToken::new();
    let log = stdout_sink();
    let n = cleanup_empty_folders(&root, &stop, &log);

    // один проход: empty1 и nested_empty (empty2 НЕ пуст в момент обхода)
    assert_eq!(n, 2);
    assert!(!root.join("empty1").exists());
    assert!(!root.join("empty2/nested_empty").exists());
    assert!(root.join("empty2").exists(), "empty2 освобождается только во втором проходе");
    assert!(root.join("keep.txt").exists());
}
