//! Тест ZIP-архивации созданных папок (`archive_folders`): структура внутри
//! архива, общая база как корень, проверка целостности, удаление оригиналов.

use std::fs;
use std::path::PathBuf;

use filemanager_core::{archive_folders, stdout_sink, StopToken};
use tempfile::TempDir;

// Оба теста пишут `created_folders_<ts>.zip` в CWD (ts — секунды): без
// сериализации они затирают друг друга (cargo test = параллельные треды).
static ZIP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn archive_folders_zips_structure() {
    let _g = ZIP_LOCK.lock().unwrap();
    let dir = TempDir::new().unwrap();
    // две ветки от общего корня base
    let base = dir.path().join("base");
    let p1 = base.join("A").join("a1.txt");
    let p2 = base.join("B").join("sub").join("b2.txt");
    fs::create_dir_all(p1.parent().unwrap()).unwrap();
    fs::create_dir_all(p2.parent().unwrap()).unwrap();
    fs::write(&p1, b"content-A").unwrap();
    fs::write(&p2, b"content-B").unwrap();

    let stop = StopToken::new();
    let log = stdout_sink();
    let folders = vec![
        dir.path().join("base/A").to_path_buf(),
        dir.path().join("base/B").to_path_buf(),
    ];
    // архив создаётся в CWD — уберём за собой
    let zip_path = archive_folders(&folders, false, &stop, &log).expect("архив создан");
    assert!(zip_path.exists());
    assert!(zip_path.to_string_lossy().starts_with("created_folders_"));

    // содержимое: имена относительны ОБЩЕЙ БАЗЫ (как Python relative_to(common_base)):
    // A/, A/a1.txt, B/, B/sub/, B/sub/b2.txt
    let mut names = Vec::new();
    {
        let f = fs::File::open(&zip_path).unwrap();
        let mut zip = zip::ZipArchive::new(f).unwrap();
        for i in 0..zip.len() {
            let entry = zip.by_index(i).unwrap();
            names.push(entry.name().to_string());
        }
    }
    let names: Vec<_> = names.iter().map(|s| s.as_str()).collect();
    assert!(names.contains(&"A/a1.txt"), "нет a1: {names:?}");
    assert!(names.contains(&"B/sub/b2.txt"), "нет b2: {names:?}");

    let _ = fs::remove_file(&zip_path);
    // оригиналы на месте (delete_originals=false)
    assert!(p1.exists() && p2.exists());
}

#[test]
fn archive_folders_delete_originals() {
    let _g = ZIP_LOCK.lock().unwrap();
    let dir = TempDir::new().unwrap();
    // Страховки: чтобы финальная остановка обхода была на base и выше,
    // а не на родительском каталоге тестового tempdir.
    fs::write(dir.path().join("guard.txt"), b"keep").unwrap();
    let base = dir.path().join("base");
    let p1 = base.join("A").join("x.txt");
    fs::create_dir_all(p1.parent().unwrap()).unwrap();
    fs::write(&p1, b"data").unwrap();
    // В base остаётся чужое содержимое — после удаления A она НЕ опустеет.
    fs::write(base.join("keep.txt"), b"keep").unwrap();

    let stop = StopToken::new();
    let log = stdout_sink();
    let folders = vec![base.join("A").to_path_buf()];
    let zip_path = archive_folders(&folders, true, &stop, &log).expect("архив создан");

    // оригиналы удалены
    assert!(!base.join("A").exists(), "A должна быть удалена");
    // base НЕ удаляется (не в списке и не опустела)
    assert!(base.exists());
    let _ = fs::remove_file(&zip_path);
}

#[test]
fn archive_folders_delete_originals_sweeps_empty_base() {
    let _g = ZIP_LOCK.lock().unwrap();
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("guard.txt"), b"keep").unwrap();
    let base = dir.path().join("base");
    let p1 = base.join("A").join("x.txt");
    fs::create_dir_all(p1.parent().unwrap()).unwrap();
    fs::write(&p1, b"data").unwrap();

    let stop = StopToken::new();
    let log = stdout_sink();
    let folders = vec![base.join("A").to_path_buf()];
    let zip_path = archive_folders(&folders, true, &stop, &log).expect("архив создан");

    // A удалена; base опустела — финальная перепроверка её тоже очищает.
    assert!(!base.join("A").exists(), "A должна быть удалена");
    assert!(!base.exists(), "base опустела — финальный проход должен её очистить");
    let _ = fs::remove_file(&zip_path);
}

#[test]
fn archive_folders_empty_list() {
    let stop = StopToken::new();
    let log = stdout_sink();
    let folders: Vec<PathBuf> = Vec::new();
    assert!(archive_folders(&folders, false, &stop, &log).is_none());
}
