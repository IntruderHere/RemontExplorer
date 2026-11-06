//! Parity-тест копирования: атомарное копирование, кэш возобновления,
//! пропуск при повторном запуске, повтор после изменения источника.

use std::fs;

use filemanager_core::{copy_files_from_list, stdout_sink, StopToken};
use tempfile::TempDir;

#[test]
fn copy_and_resume_skip() {
    let dir = TempDir::new().unwrap();
    let src_a = dir.path().join("src/a.txt");
    let src_b = dir.path().join("src/b.txt");
    fs::create_dir_all(src_a.parent().unwrap()).unwrap();
    fs::write(&src_a, b"alpha").unwrap();
    fs::write(&src_b, b"beta beta").unwrap();
    let dst_dir = dir.path().join("dst");
    let cfg = dir.path().join("from_to.csv");
    fs::write(
        &cfg,
        // trailing-slash: «to» — директория (порт правил Python стр. 328-333)
        format!(
            "{};{}/\n{};{}/\n",
            src_a.display(),
            dst_dir.display(),
            src_b.display(),
            dst_dir.display()
        ),
    )
    .unwrap();
    let cache = dir.path().join("copy_resume.json");
    let stop = StopToken::new();
    let log = stdout_sink();

    // 1-й запуск: скопированы оба
    let n1 = copy_files_from_list(&cfg, &cache, 4, true, &stop, &log);
    assert_eq!(n1, 2);
    assert_eq!(fs::read(dst_dir.join("a.txt")).unwrap(), b"alpha");
    assert_eq!(fs::read(dst_dir.join("b.txt")).unwrap(), b"beta beta");
    assert!(cache.exists(), "кэш возобновления должен быть создан");

    // 2-й запуск: всё пропущено, ничего не скопировано
    let n2 = copy_files_from_list(&cfg, &cache, 4, true, &stop, &log);
    assert_eq!(n2, 0);

    // Источник изменился -> копируется снова
    fs::write(&src_a, b"alpha v2 longer").unwrap();
    let n3 = copy_files_from_list(&cfg, &cache, 4, true, &stop, &log);
    assert_eq!(n3, 1);
    assert_eq!(
        fs::read(dst_dir.join("a.txt")).unwrap(),
        b"alpha v2 longer"
    );
}

#[test]
fn copy_to_explicit_file_destination() {
    let dir = TempDir::new().unwrap();
    let src = dir.path().join("in.dat");
    fs::write(&src, b"payload").unwrap();
    let dst = dir.path().join("out/renamed.dat");
    let cfg = dir.path().join("pairs.csv");
    // без resume-кэша
    let cache = dir.path().join("cache.json");
    fs::write(&cfg, format!("{};{}\n", src.display(), dst.display())).unwrap();
    let stop = StopToken::new();
    let log = stdout_sink();

    let n = copy_files_from_list(&cfg, &cache, 2, false, &stop, &log);
    assert_eq!(n, 1);
    assert_eq!(fs::read(&dst).unwrap(), b"payload");
    assert!(!cache.exists(), "без resume кэш не пишется");
}
