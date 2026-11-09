//! Консольный режим: разбор аргументов и реальное выполнение операций.

use std::fs;

use remont_core::cli::{parse, run, Action};
use tempfile::TempDir;

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn args(s: &[&str]) -> Vec<String> {
    s.iter().map(|x| x.to_string()).collect()
}

#[test]
fn scan_produces_report_and_history_entry() {
    let _g = env_lock();
    let tmp = TempDir::new().unwrap();
    std::env::set_var(remont_core::paths::DATA_DIR_ENV, tmp.path());

    let data = tmp.path().join("данные");
    fs::create_dir_all(data.join("sub")).unwrap();
    fs::write(data.join("a.txt"), b"hello").unwrap();
    fs::write(data.join("sub").join("b.bin"), b"12345").unwrap();

    let out = tmp.path().join("отчёты");
    let opts = parse(&args(&[
        "--cli",
        "--scan",
        data.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
        "--quiet",
    ]))
    .unwrap()
    .expect("режим не разобран");

    assert_eq!(run(&opts), 0);

    let reports: Vec<_> = fs::read_dir(&out)
        .expect("нет папки результатов")
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .map(|e| e == "csv" || e == "xlsx")
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(reports.len(), 1, "ожидался один отчёт: {reports:?}");

    // История: одна запись с режимом scan и ссылкой на отчёт.
    let hist = remont_core::history::read_all();
    assert_eq!(hist.len(), 1, "история: {hist:?}");
    assert_eq!(hist[0].mode, "scan");
    assert_eq!(hist[0].source, "cli");
    assert!(hist[0].report.is_some());

    remont_core::set_output_dir(None);
    std::env::remove_var(remont_core::paths::DATA_DIR_ENV);
}

#[test]
fn diff_reports_from_cli_creates_file() {
    let _g = env_lock();
    let tmp = TempDir::new().unwrap();
    std::env::set_var(remont_core::paths::DATA_DIR_ENV, tmp.path());

    let old = tmp.path().join("old.csv");
    let new = tmp.path().join("new.csv");
    let header = "source_root;full_path;parent_dir;name;extension;size_mb;index_ops\r\n";
    fs::write(
        &old,
        format!("{header}D:\\X;D:\\X\\a.txt;D:\\X;a.txt;.txt;1.00;\r\n"),
    )
    .unwrap();
    fs::write(
        &new,
        format!(
            "{header}D:\\X;D:\\X\\a.txt;D:\\X;a.txt;.txt;2.00;\r\nD:\\X;D:\\X\\b.txt;D:\\X;b.txt;.txt;1.00;\r\n"
        ),
    )
    .unwrap();

    let out = tmp.path().join("out");
    remont_core::set_output_dir(Some(out.clone()));
    let opts = parse(&args(&[
        "--cli",
        "--diff",
        old.to_str().unwrap(),
        new.to_str().unwrap(),
    ]))
    .unwrap()
    .unwrap();
    assert_eq!(run(&opts), 0);

    let created: Vec<_> = fs::read_dir(&out)
        .expect("нет папки результатов")
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(created.len(), 1, "{created:?}");
    let text = fs::read_to_string(&created[0]).unwrap();
    assert!(text.contains("Добавлен"), "{text}");
    assert!(text.contains("Изменён"), "{text}");

    remont_core::set_output_dir(None);
    std::env::remove_var(remont_core::paths::DATA_DIR_ENV);
}

#[test]
fn diff_with_missing_file_returns_error_code() {
    let _g = env_lock();
    let tmp = TempDir::new().unwrap();
    std::env::set_var(remont_core::paths::DATA_DIR_ENV, tmp.path());
    let opts = parse(&args(&[
        "--cli",
        "--diff",
        tmp.path().join("нет1.csv").to_str().unwrap(),
        tmp.path().join("нет2.csv").to_str().unwrap(),
    ]))
    .unwrap()
    .unwrap();
    assert_eq!(run(&opts), 1);
    std::env::remove_var(remont_core::paths::DATA_DIR_ENV);
}

#[test]
fn history_command_lists_records() {
    let _g = env_lock();
    let tmp = TempDir::new().unwrap();
    std::env::set_var(remont_core::paths::DATA_DIR_ENV, tmp.path());
    remont_core::HistoryEntry::new("scan", "Сканирование")
        .record()
        .unwrap();

    let opts = parse(&args(&["--cli", "--history"])).unwrap().unwrap();
    assert_eq!(run(&opts), 0);

    let export = tmp.path().join("hist.csv");
    let opts = parse(&args(&[
        "--cli",
        "--history",
        "--export",
        export.to_str().unwrap(),
        "--clear",
    ]))
    .unwrap()
    .unwrap();
    assert!(matches!(opts.action, Action::History { .. }));
    assert_eq!(run(&opts), 0);
    assert!(export.exists(), "экспорт не создан");
    assert!(remont_core::history::read_all().is_empty(), "история не очищена");

    std::env::remove_var(remont_core::paths::DATA_DIR_ENV);
}

#[test]
fn parse_errors_are_reported() {
    assert!(parse(&args(&["--cli", "--scan"])).is_err());
    assert!(parse(&args(&["--cli", "--scan", "x", "--format", "pdf"])).is_err());
    assert!(parse(&args(&["--cli", "--unknown"])).is_err());
}

#[test]
fn gui_mode_is_not_cli() {
    // Аргумент контекстного меню не должен уводить приложение в консоль.
    assert!(parse(&args(&["--scan", "D:\\Папка"])).unwrap().is_none());
    assert!(parse(&args(&[])).unwrap().is_none());
}
