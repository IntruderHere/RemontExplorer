//! Способы удаления: безвозвратно, в корзину, через архив.

use std::fs;
use std::path::PathBuf;

use remont_core::delete::{delete_paths_opts, DeleteMode};
use remont_core::{stdout_sink, Outcome, StopToken};
use tempfile::TempDir;

/// Тесты в файле идут параллельно, а переменные окружения — общие на процесс.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn data_dir(dir: &TempDir) {
    std::env::set_var(remont_core::paths::DATA_DIR_ENV, dir.path());
}

#[test]
fn permanent_deletes_everything() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("work");
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("a.txt"), b"a").unwrap();
    fs::write(root.join("sub").join("b.txt"), b"bb").unwrap();

    let log = stdout_sink();
    let stop = StopToken::new();
    let stats = delete_paths_opts(
        std::slice::from_ref(&root),
        false,
        DeleteMode::Permanent,
        &stop,
        &log,
    );

    assert_eq!(stats.errors, 0);
    assert_eq!(stats.folders, 1);
    assert!(!root.exists(), "папка должна быть удалена");
    assert_eq!(stats.size_bytes, 3);
}

#[test]
fn archive_creates_zip_and_deletes_originals() {
    let _g = env_lock();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("work");
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("a.txt"), b"alpha").unwrap();
    fs::write(root.join("sub").join("b.txt"), b"beta").unwrap();
    data_dir(&tmp);

    let out = tmp.path().join("out");
    remont_core::set_output_dir(Some(out.clone()));

    let log = stdout_sink();
    let stop = StopToken::new();
    let stats = delete_paths_opts(
        std::slice::from_ref(&root),
        false,
        DeleteMode::Archive,
        &stop,
        &log,
    );

    let archive = stats.archive.expect("архив не создан");
    assert!(archive.exists(), "файла архива нет: {}", archive.display());
    assert!(archive.starts_with(&out), "архив не в папке результатов");
    assert!(!root.exists(), "оригиналы должны быть удалены");

    // Содержимое архива сохранено.
    let file = fs::File::open(&archive).unwrap();
    let mut zip = zip::ZipArchive::new(file).unwrap();
    let names: Vec<String> = (0..zip.len())
        .map(|i| zip.by_index(i).unwrap().name().to_string())
        .collect();
    assert!(
        names.iter().any(|n| n.ends_with("a.txt")),
        "в архиве нет a.txt: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.ends_with("sub/b.txt")),
        "в архиве нет sub/b.txt: {names:?}"
    );
    remont_core::set_output_dir(None);
    std::env::remove_var(remont_core::paths::DATA_DIR_ENV);
}

#[test]
fn archive_failure_keeps_files() {
    let _g = env_lock();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("keep");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("важно.txt"), b"data").unwrap();

    // Папка результатов недоступна: на её месте — обычный файл.
    let blocker = tmp.path().join("blocked");
    fs::write(&blocker, b"i am a file").unwrap();
    remont_core::set_output_dir(Some(blocker.join("sub")));
    std::env::set_var(remont_core::paths::DATA_DIR_ENV, tmp.path());

    let log = stdout_sink();
    let stop = StopToken::new();
    let stats = delete_paths_opts(
        std::slice::from_ref(&root),
        false,
        DeleteMode::Archive,
        &stop,
        &log,
    );

    assert!(stats.archive.is_none());
    assert!(stats.errors > 0, "ошибка архивации не зафиксирована");
    assert!(root.exists(), "без архива удалять нельзя");
    assert!(root.join("важно.txt").exists());

    remont_core::set_output_dir(None);
    std::env::remove_var(remont_core::paths::DATA_DIR_ENV);
}

#[test]
#[cfg(not(windows))]
fn recycle_is_unsupported_outside_windows() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("work");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("a.txt"), b"a").unwrap();

    let log = stdout_sink();
    let stop = StopToken::new();
    let stats = delete_paths_opts(
        std::slice::from_ref(&root),
        false,
        DeleteMode::Recycle,
        &stop,
        &log,
    );
    assert!(stats.errors > 0);
    assert!(root.exists(), "вне Windows ничего не удаляется");
}

#[test]
fn mode_labels_are_translated() {
    use remont_core::i18n::Lang;
    assert_eq!(DeleteMode::Recycle.label(Lang::Ru), "В корзину");
    assert_eq!(DeleteMode::Archive.label(Lang::En), "To ZIP archive");
    assert_eq!(DeleteMode::ALL.len(), 3);
}

#[test]
fn history_marks_delete_outcome() {
    let _g = env_lock();
    // Операция удаления попадает в историю с режимом delete.
    let tmp = TempDir::new().unwrap();
    std::env::set_var(remont_core::paths::DATA_DIR_ENV, tmp.path());
    let mut e = remont_core::HistoryEntry::new("delete", "Удаление");
    e.outcome = Outcome::Ok;
    e.record().unwrap();
    let all = remont_core::history::read_all();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].mode, "delete");
    std::env::remove_var(remont_core::paths::DATA_DIR_ENV);
    let _ = PathBuf::new();
}
