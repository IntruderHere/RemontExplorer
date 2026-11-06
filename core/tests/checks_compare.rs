//! Тест сравнения: классификация различий и формат отчёта.

use std::fs;

use remont_core::{compare_folders, stdout_sink, CompareMethod, StopToken};
use tempfile::TempDir;

#[test]
fn compare_detects_differences() {
    let dir = TempDir::new().unwrap();
    let a = dir.path().join("A");
    let b = dir.path().join("B");
    fs::create_dir_all(a.join("s")).unwrap();
    fs::create_dir_all(b.join("s")).unwrap();
    fs::write(a.join("same.txt"), b"identical").unwrap();
    fs::write(b.join("same.txt"), b"identical").unwrap();
    fs::write(a.join("diff_size.txt"), b"123").unwrap();
    fs::write(b.join("diff_size.txt"), b"12345").unwrap();
    fs::write(a.join("only_a.txt"), b"x").unwrap();

    let cfg = dir.path().join("pairs.txt");
    fs::write(&cfg, format!("{};{}\n", a.display(), b.display())).unwrap();
    let out = dir.path().join("compare_out.csv");

    let stop = StopToken::new();
    let log = stdout_sink();
    let reports = compare_folders(&cfg, CompareMethod::SizeDate, Some(&out), &stop, &log).expect("сравнение");
    assert_eq!(reports.len(), 1);

    let text = fs::read_to_string(&out).unwrap();
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0],
        "file_name;path_in_a;path_in_b;size_a_mb;size_b_mb;modified_a;modified_b;different"
    );
    assert_eq!(lines.len(), 4, "шапка + 3 имени");

    let row_same = lines.iter().find(|l| l.starts_with("same.txt;")).unwrap();
    assert!(row_same.ends_with(";No"), "строка: {row_same}");
    let row_diff = lines.iter().find(|l| l.starts_with("diff_size.txt;")).unwrap();
    assert!(row_diff.ends_with(";Yes"), "строка: {row_diff}");
    let row_only = lines.iter().find(|l| l.starts_with("only_a.txt;")).unwrap();
    assert!(row_only.ends_with(";Yes"), "строка: {row_only}");
}

#[test]
fn compare_hash_method_detects_content_diff() {
    let dir = TempDir::new().unwrap();
    let a = dir.path().join("A");
    let b = dir.path().join("B");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    // одинаковый размер, РАЗНОЕ содержимое — только хеш это поймает
    fs::write(a.join("f.bin"), b"AAAA").unwrap();
    fs::write(b.join("f.bin"), b"BBBB").unwrap();

    let cfg = dir.path().join("pairs.txt");
    fs::write(&cfg, format!("{};{}\n", a.display(), b.display())).unwrap();
    let out = dir.path().join("compare_hash.csv");

    let stop = StopToken::new();
    let log = stdout_sink();
    let reports = compare_folders(
        &cfg,
        CompareMethod::Hash(remont_core::HashAlgo::Sha256),
        Some(&out),
        &stop,
        &log,
    ).expect("сравнение");
    assert_eq!(reports.len(), 1);

    let text = fs::read_to_string(&out).unwrap();
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let row = text.lines().find(|l| l.starts_with("f.bin;")).unwrap();
    assert!(row.ends_with(";Yes"), "разное содержимое при равном размере: {row}");
}

/// Регрессия: файл, прошедший глубокое сравнение с ИДЕНТИЧНЫМ содержимым,
/// обязан получить different=No (раньше guard `i < results.len()` сравнивал
/// индекс СТРОКИ с числом задач, и у поздних строк поле оставалось пустым).
#[test]
fn compare_hash_method_identical_content_marked_no() {
    let dir = TempDir::new().unwrap();
    let a = dir.path().join("A");
    let b = dir.path().join("B");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    // две строки с разным размером ИДУТ ПЕРЕЗ deep-строкой по алфавиту —
    // без фикса индекс строки z.bin (2) >= результатов (1) и поле пустело
    fs::write(a.join("m1.txt"), b"12345").unwrap();
    fs::write(b.join("m1.txt"), b"x").unwrap();
    fs::write(a.join("m2.txt"), b"abcdef").unwrap();
    fs::write(b.join("m2.txt"), b"yz").unwrap();
    fs::write(a.join("z.bin"), b"AAAA").unwrap();
    fs::write(b.join("z.bin"), b"AAAA").unwrap();

    let cfg = dir.path().join("pairs.txt");
    fs::write(&cfg, format!("{};{}\n", a.display(), b.display())).unwrap();
    let out = dir.path().join("compare_hash_no.csv");

    let stop = StopToken::new();
    let log = stdout_sink();
    compare_folders(
        &cfg,
        CompareMethod::Hash(remont_core::HashAlgo::Sha256),
        Some(&out),
        &stop,
        &log,
    ).expect("сравнение");

    let text = fs::read_to_string(&out).unwrap();
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let row = text.lines().find(|l| l.starts_with("z.bin;")).unwrap();
    assert!(row.ends_with(";No"), "идентичное содержимое: {row}");
    let row_m1 = text.lines().find(|l| l.starts_with("m1.txt;")).unwrap();
    assert!(row_m1.ends_with(";Yes"), "размер отличается: {row_m1}");
}
