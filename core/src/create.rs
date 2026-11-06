//! Создание структуры папок (порт `create_folders`) и упаковка в ZIP
//! (порт `archive_folders`).

use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::pathutil::sweep_empty_parents;
use crate::stop::StopToken;
use crate::{emit_progress, LogSink};
use crate::em;

fn timestamp() -> String {
    chrono::Local::now().format("%Y%m%d_%H%M%S").to_string()
}

/// Создаёт папки из списка (порт `create_folders`).
/// Возвращает все созданные папки ВКЛЮЧАЯ родительские уровни
/// (счётчик честный — фикс из Python-версии сохранён).
pub fn create_folders(paths: &[PathBuf], stop: &StopToken, log: &LogSink) -> Vec<PathBuf> {
    let mut created: Vec<PathBuf> = Vec::new();
    let mut all_created: BTreeSet<PathBuf> = BTreeSet::new();
    for (i, path) in paths.iter().enumerate() {
        emit_progress(log, i, paths.len());
        if stop.is_set() {
            em!(log, "create_aborted");
            break;
        }
        if path.exists() {
            em!(log, "create_exists", path.display());
            continue;
        }
        // Родители, которых ещё не существует (до mkdir)
        let mut missing_parents: Vec<PathBuf> = Vec::new();
        let mut cur = path.parent().map(|x| x.to_path_buf());
        while let Some(c) = cur.take() {
            if !c.exists() {
                missing_parents.push(c.clone());
            }
            let parent = c.parent().map(|x| x.to_path_buf());
            if parent.as_deref() == Some(c.as_path()) {
                break;
            }
            cur = parent;
        }
        match fs::create_dir_all(path) {
            Ok(()) => {
                em!(log, "create_created", path.display());
                created.push(path.clone());
                all_created.extend(missing_parents);
            }
            Err(e) => em!(log, "create_error", path.display(), e.to_string()),
        }
    }
    all_created.extend(created);
    emit_progress(log, paths.len(), paths.len());
    all_created.into_iter().collect()
}

/// Общий корень набора путей (аналог `os.commonpath`).
fn common_base(paths: &[PathBuf]) -> PathBuf {
    let first = match paths.first() {
        Some(p) => p.clone(),
        None => return PathBuf::from("."),
    };
    let mut base = first;
    for p in &paths[1..] {
        let mut common = PathBuf::new();
        for (a, b) in base.components().zip(p.components()) {
            if a == b {
                common.push(a);
            } else {
                break;
            }
        }
        if common.as_os_str().is_empty() {
            return PathBuf::from(".");
        }
        base = common;
    }
    base
}

fn verify_zip(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    for i in 0..archive.len() {
        let mut f = archive.by_index(i)?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?; // чтение сверяет CRC
    }
    Ok(())
}

/// Упаковывает папки в ZIP с общей базой как корнем (порт `archive_folders`).
/// С проверкой целостности; опционально удаляет оригиналы.
/// Возвращает путь архива или `None` при ошибке (оригиналы сохраняются).
pub fn archive_folders(
    folders: &[PathBuf],
    delete_originals: bool,
    stop: &StopToken,
    log: &LogSink,
) -> Option<PathBuf> {
    if folders.is_empty() {
        return None;
    }
    let zip_name = format!("created_folders_{}.zip", timestamp());
    let zip_path = PathBuf::from(&zip_name);
    em!(log, "zip_start", zip_name);

    let resolved: Vec<PathBuf> = folders.iter().filter_map(|p| p.canonicalize().ok()).collect();
    let common = common_base(&resolved);
    em!(log, "zip_root", common.display());

    let file = match File::create(&zip_path) {
        Ok(f) => f,
        Err(e) => {
            em!(log, "zip_create_error", zip_name, e.to_string());
            return None;
        }
    };
    // Быстрый подсчёт элементов для прогресс-бара (только обход, без чтения).
    let mut total_entries = 0usize;
    for folder in folders {
        let Ok(folder_c) = folder.canonicalize() else { continue };
        for entry in walkdir::WalkDir::new(&folder_c).follow_links(false).into_iter() {
            if entry.is_ok() {
                total_entries += 1;
            }
        }
    }
    let mut done_entries = 0usize;
    let mut zip = zip::ZipWriter::new(file);
    let mut written: HashSet<String> = HashSet::new();
    let mut ok = true;

    for folder in folders {
        if stop.is_set() {
            em!(log, "zip_aborted");
            ok = false;
            break;
        }
        let Ok(folder_c) = folder.canonicalize() else { continue };
        for entry in walkdir::WalkDir::new(&folder_c).follow_links(false).into_iter() {
            if stop.is_set() {
                ok = false;
                break;
            }
            let Ok(entry) = entry else { continue };
            done_entries += 1;
            if done_entries.is_multiple_of(200) || done_entries == total_entries {
                emit_progress(log, done_entries, total_entries.max(1));
            }
            let ft = entry.file_type();
            let rel = entry.path().strip_prefix(&common).unwrap_or(entry.path());
            let arcname = rel.to_string_lossy().replace('\\', "/");
            if ft.is_dir() {
                let name = format!("{arcname}/");
                if written.insert(name.clone()) {
                    let opts = zip::write::SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Deflated)
                        .unix_permissions(0o755);
                    if zip.start_file(&name, opts).is_err() {
                        ok = false;
                        break;
                    }
                }
            } else if ft.is_file() {
                if !written.insert(arcname.clone()) {
                    continue;
                }
                let opts = zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated)
                    .unix_permissions(0o644);
                if let Err(e) = zip.start_file(&arcname, opts) {
                    em!(log, "err_originals_preserved", e.to_string());
                    ok = false;
                    break;
                }
                let Ok(mut f) = File::open(entry.path()) else {
                    em!(log, "zip_read_error", entry.path().display());
                    ok = false;
                    break;
                };
                if std::io::copy(&mut f, &mut zip).is_err() {
                    em!(log, "zip_write_fail");
                    ok = false;
                    break;
                }
            }
            if !ok {
                break;
            }
        }
        if !ok {
            break;
        }
    }

    if !ok {
        let _ = zip.finish();
        let _ = fs::remove_file(&zip_path);
        return None;
    }
    if zip.finish().is_err() {
        em!(log, "zip_finish_fail");
        let _ = fs::remove_file(&zip_path);
        return None;
    }

    em!(log, "zip_verify");
    if let Err(e) = verify_zip(&zip_path) {
        em!(log, "err_originals_preserved", e.to_string());
        let _ = fs::remove_file(&zip_path);
        return None;
    }
    em!(log, "zip_verified");

    if delete_originals {
        em!(log, "zip_delete_start");
        let mut sorted = folders.to_vec();
        sorted.sort_by_key(|a| std::cmp::Reverse(a.components().count()));
        let mut deleted_ok: Vec<PathBuf> = Vec::new();
        for folder in sorted {
            if stop.is_set() {
                break;
            }
            if folder.exists() {
                match fs::remove_dir_all(&folder) {
                    Ok(()) => {
                        em!(log, "zip_deleted", folder.display());
                        deleted_ok.push(folder);
                    }
                    Err(e) => em!(log, "zip_delete_fail", folder.display(), e.to_string()),
                }
            }
        }
        // Финальная перепроверка: общий корень (и другие родительские
        // папки), ставшие пустыми после удаления, тоже очищаем.
        if !deleted_ok.is_empty() {
            let n = sweep_empty_parents(&deleted_ok, stop, log);
            if n > 0 {
                em!(log, "zip_extra_cleaned", n);
            }
        }
        em!(log, "zip_originals_deleted");
    }
    Some(zip_path)
}
