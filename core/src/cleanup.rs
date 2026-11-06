//! Очистка папок от пустых подпапок (порт `cleanup_empty_folders`).
//!
//! Семантика (парность с Python): один проход снизу вверх; удаляются
//! папки, НЕ содержавшие файлов/подпапок на момент обхода (родитель,
//! освободившийся после удаления детей, в этом же проходе НЕ удаляется —
//! как в Python `os.walk`).

use std::fs;
use std::path::{Path, PathBuf};

use crate::stop::StopToken;
use crate::{emit_progress, LogSink};
use crate::em;

/// Очищает `root` от пустых папок (порт `cleanup_empty_folders`).
/// Возвращает число удалённых папок.
pub fn cleanup_empty_folders(root: &Path, stop: &StopToken, log: &LogSink) -> usize {
    stop.reset();
    let mut empty: Vec<PathBuf> = Vec::new();
    let mut unreadable = 0usize;
    emit_progress(log, 0, 0); // поиск пустых папок — доля не известна
    {
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            if stop.is_set() {
                break;
            }
            let Ok(entries) = fs::read_dir(&dir) else {
                unreadable += 1;
                continue;
            };
            let mut subdirs = Vec::new();
            let mut count = 0usize;
            for e in entries.flatten() {
                count += 1;
                if let Ok(ft) = e.file_type() {
                    if ft.is_dir() {
                        subdirs.push(e.path());
                    }
                }
            }
            if count == 0 {
                empty.push(dir);
            }
            stack.extend(subdirs);
        }
    }
    if unreadable > 0 {
        em!(log, "cleanup_unreadable", unreadable);
    }
    if empty.is_empty() {
        em!(log, "cleanup_none_found", root.display());
        return 0;
    }
    em!(log, "cleanup_found", empty.len());

    let mut deleted = 0;
    for (i, folder) in empty.iter().enumerate() {
        emit_progress(log, i + 1, empty.len());
        if stop.is_set() {
            em!(log, "cleanup_aborted");
            break;
        }
        match fs::remove_dir(folder) {
            Ok(()) => {
                deleted += 1;
                em!(log, "cleanup_deleted", folder.display());
            }
            Err(e) => em!(log, "delete_error", folder.display(), e.to_string()),
        }
    }
    deleted
}
