//! Имена отчётов не должны затирать друг друга: два запуска в одну секунду
//! дают одинаковое имя с точностью до секунды.

use std::fs;

use remont_core::{resolve_report_path, scan_folder, set_output_dir, Sink, StopToken};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn quiet() -> remont_core::LogSink {
    Arc::new(Mutex::new(Sink::text_only(|_| {})))
}

#[test]
fn unique_path_adds_suffix_instead_of_overwriting() {
    let dir = TempDir::new().unwrap();
    let first = dir.path().join("report.csv");
    fs::write(&first, b"a").unwrap();

    let second = remont_core::unique_path(first.clone());
    assert_eq!(second.file_name().unwrap(), "report_2.csv");
    fs::write(&second, b"b").unwrap();

    let third = remont_core::unique_path(first.clone());
    assert_eq!(third.file_name().unwrap(), "report_3.csv");

    // Свободное имя не меняется.
    let free = dir.path().join("fresh.csv");
    assert_eq!(remont_core::unique_path(free.clone()), free);
}

#[test]
fn two_scans_in_a_row_create_two_reports() {
    let _g = env_lock();
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("a.txt"), b"1").unwrap();
    let out = tmp.path().join("out");
    set_output_dir(Some(out.clone()));

    let stop = StopToken::new();
    let log = quiet();
    let (p1, _) = scan_folder(&src, None, 2, &stop, &log).expect("первый скан");
    let (p2, _) = scan_folder(&src, None, 2, &stop, &log).expect("второй скан");

    assert_ne!(p1, p2, "второй отчёт затёр первый: {}", p1.display());
    assert!(p1.exists() && p2.exists());

    let count = fs::read_dir(&out).unwrap().flatten().count();
    assert_eq!(count, 2, "ожидались два файла отчётов, найдено {count}");

    set_output_dir(None);
}

#[test]
fn report_name_keeps_absolute_and_cyrillic() {
    let _g = env_lock();
    set_output_dir(None);
    let p = resolve_report_path("files_info_ОПС_20261108_101500.csv");
    assert!(p.is_absolute());
    assert!(p.to_string_lossy().contains("ОПС"));
}

/// Если каталог программы недоступен для записи (Program Files), отчёты
/// уходят в `Документы\RemontExplorer\reports`, а не срывают операцию.
#[test]
#[cfg(unix)]
fn read_only_app_dir_falls_back_to_documents() {
    use std::os::unix::fs::PermissionsExt;

    let _g = env_lock();
    let tmp = TempDir::new().unwrap();
    std::env::set_var(remont_core::paths::DATA_DIR_ENV, tmp.path());
    set_output_dir(None);

    let ro = tmp.path().join("ro");
    fs::create_dir_all(&ro).unwrap();
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();

    let old = std::env::current_dir().unwrap();
    std::env::set_current_dir(&ro).unwrap();
    let p = resolve_report_path("files_info_x.csv");
    std::env::set_current_dir(&old).unwrap();
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o755)).unwrap();

    assert!(
        p.starts_with(tmp.path().join("reports")),
        "отчёт не ушёл в резервную папку: {}",
        p.display()
    );
    std::env::remove_var(remont_core::paths::DATA_DIR_ENV);
}

/// В обычном каталоге отчёты пишутся рядом с приложением (как раньше).
#[test]
#[cfg(unix)]
fn writable_app_dir_is_used_by_default() {
    let _g = env_lock();
    let tmp = TempDir::new().unwrap();
    set_output_dir(None);
    let old = std::env::current_dir().unwrap();
    std::env::set_current_dir(tmp.path()).unwrap();
    let p = resolve_report_path("files_info_y.csv");
    std::env::set_current_dir(&old).unwrap();
    // На macOS /var — симлинк на /private/var: сравниваем канонические пути.
    let base = tmp.path().canonicalize().unwrap();
    let got = p.parent().unwrap().canonicalize().unwrap();
    assert_eq!(got, base, "отчёт ушёл не в рабочий каталог: {}", p.display());
}
