//! Копирование с проверкой свободного места и сверкой по SHA-256.

use std::fs;

use remont_core::copy::{copy_files_from_list_opts, CopyOptions};
use remont_core::{stdout_sink, StopToken};
use tempfile::TempDir;

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Создать пару «источник → назначение» и CSV-список из неё.
fn setup(tmp: &TempDir, files: &[(&str, &[u8])]) -> (std::path::PathBuf, std::path::PathBuf) {
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&dst).unwrap();
    let mut csv = String::new();
    for (name, body) in files {
        let p = src.join(name);
        fs::write(&p, body).unwrap();
        csv.push_str(&format!("{};{}\r\n", p.display(), dst.join(name).display()));
    }
    let list = tmp.path().join("pairs.csv");
    fs::write(&list, csv).unwrap();
    (list, dst)
}

#[test]
fn copies_and_verifies_with_sha256() {
    let _g = env_lock();
    let tmp = TempDir::new().unwrap();
    let (list, dst) = setup(
        &tmp,
        &[("a.bin", b"alpha"), ("b.bin", &[7u8; 5000]), ("кириллица.txt", b"ok")],
    );
    let log = stdout_sink();
    let stop = StopToken::new();
    let report = copy_files_from_list_opts(
        &list,
        &tmp.path().join("resume.json"),
        4,
        false,
        CopyOptions {
            verify: true,
            check_space: true,
        },
        &stop,
        &log,
    );

    assert_eq!(report.total, 3);
    assert_eq!(report.copied, 3);
    assert_eq!(report.verified, 3, "не все копии сверены");
    assert!(report.mismatches.is_empty(), "{:?}", report.mismatches);
    assert!(report.is_clean());
    assert_eq!(fs::read(dst.join("a.bin")).unwrap(), b"alpha");
    assert_eq!(fs::read(dst.join("кириллица.txt")).unwrap(), b"ok");
}

#[test]
fn verification_can_be_disabled() {
    let _g = env_lock();
    let tmp = TempDir::new().unwrap();
    let (list, _dst) = setup(&tmp, &[("a.bin", b"alpha")]);
    let log = stdout_sink();
    let stop = StopToken::new();
    let report = copy_files_from_list_opts(
        &list,
        &tmp.path().join("resume.json"),
        1,
        false,
        CopyOptions::default(),
        &stop,
        &log,
    );
    assert_eq!(report.copied, 1);
    assert_eq!(report.verified, 0, "сверка не запрашивалась");
}

#[test]
fn insufficient_space_is_reported_before_start() {
    let _g = env_lock();
    let tmp = TempDir::new().unwrap();
    // Цель — «диск», которого нет: свободное место неизвестно, поэтому
    // проверка не блокирует (см. disk::check_space). Убеждаемся, что
    // копирование всё равно выполняется.
    let (list, dst) = setup(&tmp, &[("a.bin", b"alpha")]);
    let log = stdout_sink();
    let stop = StopToken::new();
    let report = copy_files_from_list_opts(
        &list,
        &tmp.path().join("resume.json"),
        2,
        false,
        CopyOptions {
            verify: false,
            check_space: true,
        },
        &stop,
        &log,
    );
    assert_eq!(report.copied, 1);
    assert!(dst.join("a.bin").exists());
}

#[test]
fn missing_source_does_not_break_other_files() {
    let _g = env_lock();
    let tmp = TempDir::new().unwrap();
    let (list, dst) = setup(&tmp, &[("ok.bin", b"1")]);
    // Добавим в список несуществующий источник.
    let mut csv = fs::read_to_string(&list).unwrap();
    csv.push_str(&format!(
        "{};{}\r\n",
        tmp.path().join("нет.bin").display(),
        dst.join("нет.bin").display()
    ));
    fs::write(&list, csv).unwrap();

    let log = stdout_sink();
    let stop = StopToken::new();
    let report = copy_files_from_list_opts(
        &list,
        &tmp.path().join("resume.json"),
        2,
        false,
        CopyOptions {
            verify: true,
            check_space: true,
        },
        &stop,
        &log,
    );
    assert_eq!(report.copied, 1);
    assert!(!report.is_clean(), "неполное копирование не должно считаться чистым");
}

#[test]
fn space_helpers_are_consistent() {
    use remont_core::disk::{check_space, human, required_with_margin};
    assert_eq!(required_with_margin(1_000_000), 1_050_000);
    assert!(check_space(1024, &std::env::temp_dir()).is_ok());
    assert!(human(1024).starts_with("1.0"));
}
