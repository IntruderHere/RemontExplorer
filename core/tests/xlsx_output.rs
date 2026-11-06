//! Формат отчётов XLSX: CSV конвертируется в книгу Excel и удаляется.
//! Отдельный тестовый бинарник — глобальный формат не влияет на другие тесты.

use filemanager_core::{scan_folder, set_output_format, OutputFormat, Sink, StopToken};
use std::fs;
use std::io::Read;
use std::sync::{Arc, Mutex};

fn quiet() -> filemanager_core::LogSink {
    Arc::new(Mutex::new(Sink::text_only(|_| {})))
}

#[test]
fn scan_report_is_converted_to_xlsx() {
    let tmp = std::env::temp_dir().join(format!("fm_xlsx_{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    let src = tmp.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("a.txt"), b"hello").unwrap();
    fs::write(src.join("b.txt"), b"world!").unwrap();

    set_output_format(OutputFormat::Xlsx);
    let stop = StopToken::new();
    let log = quiet();
    let (path, stats) = scan_folder(&src, None, 2, &stop, &log).expect("сканирование не удалось");
    set_output_format(OutputFormat::Csv);

    assert_eq!(stats.files, 2);
    assert_eq!(
        path.extension().and_then(|e| e.to_str()),
        Some("xlsx"),
        "отчёт должен быть в XLSX: {}",
        path.display()
    );
    assert!(
        !path.with_extension("csv").exists(),
        "промежуточный CSV должен быть удалён"
    );

    // Книга — это ZIP с XML: проверяем, что данные внутри на месте.
    let data = fs::read(&path).unwrap();
    assert_eq!(&data[..2], b"PK", "файл не похож на xlsx");
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(data)).unwrap();
    let mut text = String::new();
    for name in ["xl/sharedStrings.xml", "xl/worksheets/sheet1.xml"] {
        if let Ok(mut f) = zip.by_name(name) {
            let mut buf = String::new();
            f.read_to_string(&mut buf).unwrap();
            text.push_str(&buf);
        }
    }
    assert!(text.contains("a.txt"), "в книге нет имени файла");
    assert!(text.contains("b.txt"), "в книге нет второго файла");
    assert!(
        text.contains("source_root") || text.contains("full_path"),
        "в книге нет заголовков колонок"
    );

    let _ = fs::remove_dir_all(&tmp);

    csv_stays_csv_by_default();
}

/// Проверяется в том же тесте: глобальный формат — общее состояние,
/// параллельные тесты мешали бы друг другу.
#[allow(dead_code)]
fn csv_stays_csv_by_default() {
    let tmp = std::env::temp_dir().join(format!("fm_csv_{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    let src = tmp.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("c.txt"), b"x").unwrap();

    set_output_format(OutputFormat::Csv);
    let (path, _) = scan_folder(&src, None, 1, &StopToken::new(), &quiet()).expect("скан");
    assert_eq!(
        path.extension().and_then(|e| e.to_str()),
        Some("csv"),
        "по умолчанию отчёт остаётся CSV"
    );
    let _ = fs::remove_dir_all(&tmp);
}
