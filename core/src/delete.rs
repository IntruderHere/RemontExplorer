//! Удаление файлов/папок из списка.
//!
//! Семантика: файлы и папки, опциональная очистка пустых родительских папок.
//!
//! 1. Поштучная очистка: после удаления каждой папки пустые
//!    родители очищаются вверх, но не выше верхнего из указанных
//!    пользователем путей (и их прямого родителя).
//! 2. Финальный проход (добавлен по требованию, осознанное отклонение от
//!    финальная проверка): после обработки ВСЕХ пунктов списка родительские цепочки
//!    перепроверяются — папки, ставшие пустыми именно в процессе удаления
//!    (сестринские папки удалялись по очереди), удаляются также; единственная
//!    защита — корень диска/ФС (D:\ никогда не удаляется).
//!
//! Битые символические ссылки тоже удаляются: проверка существования
//! для них не проходит, но удалить их безопасно.

use std::fs;
use std::path::{Path, PathBuf};

use crate::pathutil::{delete_empty_parents, sweep_empty_parents, topmost_listed_ancestor};
use crate::stop::StopToken;
use crate::{emit_progress, LogSink};
use crate::em;

#[derive(Debug, Clone, Copy, Default)]
pub struct DeleteStats {
    pub folders: usize,
    pub files: usize,
    pub size_bytes: u64,
    pub parents: usize,
}

/// Размер директории (рекурсивно, быстрая обходная версия `_fast_dir_size`).
fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_file() {
                total += e.metadata().map(|m| m.len()).unwrap_or(0);
            } else if ft.is_dir() {
                stack.push(e.path());
            }
        }
    }
    total
}

fn delete_one(
    path: &Path,
    recursive_parents: bool,
    list: &[PathBuf],
    stats: &mut DeleteStats,
) -> std::io::Result<()> {
    // symlink_metadata: существует как симлинк даже если битый
    let Some(md) = fs::symlink_metadata(path).ok() else {
        return Ok(()); // не существует — пропускаем
    };
    let ft = md.file_type();
    if ft.is_file() || ft.is_symlink() {
        stats.size_bytes += fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        fs::remove_file(path)?;
        stats.files += 1;
    } else if ft.is_dir() {
        stats.size_bytes += dir_size(path);
        fs::remove_dir_all(path)?;
        stats.folders += 1;
        if recursive_parents {
            if let Some(parent) = path.parent() {
                // Граница: верхний указанный пользователем предок; для верхнего
                // элемента списка — собственный родитель (не поднимаемся выше).
                let boundary = topmost_listed_ancestor(path, list).unwrap_or_else(|| parent.to_path_buf());
                stats.parents += delete_empty_parents(parent, Some(&boundary));
            }
        }
    }
    Ok(())
}

/// Удаляет пути из списка. Возвращает статистику.
///
/// После основного цикла — финальный проход (см. модуль): родительские
/// цепочки удалённых путей перепроверяются на пустоту, и папки, ставшие
/// пустыми в процессе удаления (сестринские), удаляются дополнительно.
pub fn delete_paths(
    paths: &[PathBuf],
    recursive_parents: bool,
    stop: &StopToken,
    log: &LogSink,
) -> DeleteStats {
    let mut stats = DeleteStats::default();
    let mut deleted_ok: Vec<PathBuf> = Vec::new();
    for (i, path) in paths.iter().enumerate() {
        emit_progress(log, i, paths.len());
        if stop.is_set() {
            em!(log, "delete_aborted");
            break;
        }
        match delete_one(path, recursive_parents, paths, &mut stats) {
            Ok(()) => deleted_ok.push(path.clone()),
            Err(e) => em!(log, "delete_error", path.display(), e.to_string()),
        }
    }
    // Финальная перепроверка: родители, опустевшие только после удаления
    // всех пунктов списка (сестринские папки), тоже удаляем.
    if recursive_parents && !deleted_ok.is_empty() {
        stats.parents += sweep_empty_parents(&deleted_ok, stop, log);
    }
    emit_progress(log, paths.len(), paths.len());
    stats
}
