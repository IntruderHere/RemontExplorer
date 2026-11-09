//! Тест сканирования: формат отчёта обязан совпадать с
//! контрактом отчёта (UTF-8 BOM, разделитель `;`, те же колонки,
//! формат размера `%.2f`, семантика index_ops).

use std::fs;
use std::path::Path;

use remont_core::{scan_folder, stdout_sink, StopToken};
use tempfile::TempDir;

fn setup_tree(root: &Path) {
    fs::create_dir_all(root.join("sub1")).unwrap();
    fs::create_dir_all(root.join("sub2/deep")).unwrap();
    fs::write(root.join("root.txt"), b"hello").unwrap();
    fs::write(root.join("sub1/6digit_123456.txt"), vec![b'x'; 2048]).unwrap();
    fs::write(root.join("sub2/deep/date_20241015.bin"), vec![b'y'; 4096]).unwrap();
}

/// Независимая референция правила токена: первое вхождение
/// ровно 6 цифр, не входящих в более длинную последовательность.
fn first_six_digit_run(s: &str) -> String {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i - start == 6 {
                return s[start..i].to_string();
            }
        } else {
            i += 1;
        }
    }
    String::new()
}

#[test]
fn scan_report_format_is_stable() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("My Data");
    setup_tree(&root);
    let stop = StopToken::new();
    let log = stdout_sink();
    let out = dir.path().join("report.csv");

    let (path, stats) = scan_folder(&root, Some(&out), 4, &stop, &log).expect("сканирование");
    assert_eq!(path, out);
    assert_eq!(stats.files, 3);
    // (5 + 2048 + 4096) байт
    assert!((stats.size_mb - 6149.0 / (1024.0 * 1024.0)).abs() < 1e-4);

    let bytes = fs::read(&path).unwrap();
    assert!(bytes.starts_with(b"\xEF\xBB\xBF"), "нужен UTF-8 BOM (utf-8-sig)");
    let text = String::from_utf8_lossy(&bytes[3..]).to_string();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0],
        "source_root;full_path;parent_dir;name;extension;size_mb;index_ops"
    );
    assert_eq!(lines.len(), 4, "шапка + 3 файла");

    // Колонка index_ops = первое 6-цифровое вхождение во ВСЁМ full_path
    // (ровно 6 цифр подряд). Путь tempdir может содержать свои
    // 6-цифровые последовательности — поэтому ждём референцию от full_path.
    for line in lines.iter().skip(1) {
        let cols: Vec<&str> = line.split(';').collect();
        assert_eq!(cols.len(), 7, "7 колонок: {line}");
        let full_path = cols[1];
        let token = cols[6];
        assert_eq!(token, &first_six_digit_run(full_path), "строка: {line}");
    }

    // Конкретика: в имени 6digit_123456.txt есть токен 123456; если в
    // префиксе пути 6 цифр нет — именно он извлечён.
    let row6 = lines.iter().find(|l| l.contains("6digit_123456")).unwrap();
    let prefix = &row6[..row6.find("6digit_123456").unwrap()];
    if first_six_digit_run(prefix).is_empty() {
        assert!(row6.ends_with(";123456"), "строка: {row6}");
    }
    // 8-значная дата в имени никогда не даёт токена ИЗ ИМЕНИ: если префикс
    // чист, то в строке date_20241015 токен пуст.
    let rowdate = lines.iter().find(|l| l.contains("date_20241015")).unwrap();
    let prefix_d = &rowdate[..rowdate.find("date_20241015").unwrap()];
    if first_six_digit_run(prefix_d).is_empty() {
        assert!(rowdate.ends_with(";"), "8-значная дата не даёт index_ops: {rowdate}");
    }

    // размер в формате %.2f
    let rootrow = lines.iter().find(|l| l.contains("root.txt")).unwrap();
    assert!(rootrow.contains(";0.00;"), "строка: {rootrow}");
}

#[test]
fn scan_non_master_creates_timestamped_file() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("data");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("a.bin"), b"12345").unwrap();
    let stop = StopToken::new();
    let log = stdout_sink();

    let (path, stats) = scan_folder(&root, None, 4, &stop, &log).expect("сканирование");
    // Путь абсолютный (иначе файл не находится при открытии из другого CWD),
    // проверяем именно имя файла.
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    assert!(name.starts_with("files_info_data_"), "имя: {name}");
    assert!(path.is_absolute(), "путь не абсолютный: {}", path.display());
    assert!(path.exists());
    assert_eq!(stats.files, 1);
    let _ = fs::remove_file(&path); // не засоряем CWD
}
