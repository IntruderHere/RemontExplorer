//! Тесты удаления: финальный проход по родительским папкам (папки, ставшие
//! пустыми после удаления «сестринских» пунктов списка, удаляются) и
//! сохранение непустых родителей.

use std::fs;

use remont_core::{delete_paths, stdout_sink, StopToken};
use tempfile::TempDir;

#[test]
fn final_sweep_removes_parents_emptied_by_sibling_deletion() {
    // Сценарий пользователя: в списке «сестринские» P/A и P/B;
    // P опустевает только после удаления последнего.
    let dir = TempDir::new().unwrap();
    // Файл-страхование: чтобы финальный проход остановился на P и не
    // зашёл выше (к родительскому каталогу тестового tempdir).
    fs::write(dir.path().join("guard.txt"), b"keep").unwrap();
    let p = dir.path().join("P");
    fs::create_dir_all(p.join("A")).unwrap();
    fs::create_dir_all(p.join("B")).unwrap();

    let stop = StopToken::new();
    let log = stdout_sink();
    let stats = delete_paths(&[p.join("A"), p.join("B")], true, &stop, &log);

    assert_eq!(stats.folders, 2, "удалены A и B");
    assert_eq!(
        stats.parents, 1,
        "P удалена финальным проходом (опустела после удаления обоих детей)"
    );
    assert!(!p.join("A").exists());
    assert!(!p.join("B").exists());
    assert!(!p.exists(), "P должна быть очищена финальным проходом");
}

#[test]
fn delete_top_level_item_does_not_touch_its_parent() {
    let dir = TempDir::new().unwrap();
    let l = dir.path().join("L");
    let inner = l.join("solo");
    fs::create_dir_all(&inner).unwrap();
    fs::write(l.join("keep.txt"), b"keep").unwrap();

    let stop = StopToken::new();
    let log = stdout_sink();
    let stats = delete_paths(std::slice::from_ref(&inner), true, &stop, &log);

    assert_eq!(stats.folders, 1);
    assert_eq!(stats.parents, 0, "верхний элемент: родители не трогаются");
    assert!(l.exists());
    assert!(fs::read(l.join("keep.txt")).unwrap() == b"keep");
}

#[test]
fn delete_files_counts_stats() {
    let dir = TempDir::new().unwrap();
    let f1 = dir.path().join("f1.txt");
    let f2 = dir.path().join("sub/f2.txt");
    fs::create_dir_all(f2.parent().unwrap()).unwrap();
    fs::write(&f1, b"12345").unwrap();
    fs::write(&f2, b"67").unwrap();
    let missing = dir.path().join("nope.txt");

    let stop = StopToken::new();
    let log = stdout_sink();
    let stats = delete_paths(&[f1.clone(), f2.clone(), missing], false, &stop, &log);

    assert_eq!(stats.files, 2);
    assert_eq!(stats.size_bytes, 7);
    assert!(!f1.exists() && !f2.exists());
}
