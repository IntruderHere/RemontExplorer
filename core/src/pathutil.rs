//! Утилиты путей: длинные пути Windows, «мягкий» resolve, границы удаления.
//!
//! Rust std на Windows использует wide-API, но НЕ добавляет префикс `\\?\`
//! и НЕ снимает 260-символьный лимит (без LongPathsEnabled) — поэтому
//! логика префикса портирована явно (как в Python `normalize_long_path`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::stop::StopToken;
use crate::{LogSink};
use crate::em;

/// Преобразует путь для поддержки длинных имён (>260) на Windows.
/// - `C:\...` -> `\\?\C:\...`
/// - `\\server\share` -> `\\?\UNC\server\share`
/// На других ОС — no-op.
#[cfg(windows)]
pub fn normalize_long_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy().into_owned();
    if s.starts_with(r"\\?\") {
        return PathBuf::from(s);
    }
    if let Some(rest) = s.strip_prefix(r"\\") {
        return PathBuf::from(format!(r"\\?\UNC\{rest}"));
    }
    if s.len() >= 3 && s.as_bytes()[1] == b':' {
        return PathBuf::from(format!(r"\\?\{s}"));
    }
    PathBuf::from(s)
}

#[cfg(not(windows))]
pub fn normalize_long_path(p: &Path) -> PathBuf {
    p.to_path_buf()
}

/// «Мягкий» resolve (аналог `Path.resolve()` Python, non-strict):
/// канонизирует настолько, насколько путь существует. Направление может
/// ещё не существовать.
pub fn resolve_loose(p: &Path) -> PathBuf {
    if let Ok(c) = p.canonicalize() {
        return c;
    }
    let mut out = PathBuf::new();
    for comp in p.components() {
        out.push(comp.as_os_str());
        if let Ok(c) = out.canonicalize() {
            out = c;
        }
    }
    out
}

/// mtime в секундах от UNIX-epoch (0.0, если недоступен).
pub fn modified_secs(meta: &std::fs::Metadata) -> f64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Самый верхний предок `path`, присутствующий в списке путей пользователя.
/// Граница для удаления пустых родителей: папка, явно указанная пользователем
/// (и всё выше неё), удаляться cleanup'ом не должна.
pub fn topmost_listed_ancestor(path: &Path, paths: &[PathBuf]) -> Option<PathBuf> {
    let path_res = resolve_loose(path);
    let mut best: Option<PathBuf> = None;
    for candidate in paths {
        if candidate == path {
            continue;
        }
        let cand_res = resolve_loose(candidate);
        let is_ancestor = path_res.ancestors().any(|a| a == cand_res);
        if is_ancestor {
            best = Some(match best {
                Some(b) if b.components().count() <= cand_res.components().count() => b,
                _ => cand_res,
            });
        }
    }
    best
}

/// Удаляет цепочку пустых родителей вверх от `path`, не заходя за `boundary`.
/// Возвращает число удалённых папок (порт `_delete_empty_parents`).
/// Оба пути канонизируются — иначе сравнение с границей ломается на
/// симлинках (например, macOS /tmp -> /private/tmp).
pub fn delete_empty_parents(path: &Path, boundary: Option<&Path>) -> usize {
    let boundary_res = boundary.map(resolve_loose);
    let mut count = 0;
    let mut current = Some(resolve_loose(path));
    while let Some(cur) = current.take() {
        if !cur.exists() {
            break;
        }
        let parent = cur.parent().map(|p| p.to_path_buf());
        if parent.as_deref() == Some(cur.as_path()) {
            break; // корень файловой системы
        }
        if let Some(b) = &boundary_res {
            if cur == *b {
                break;
            }
        }
        let empty = fs_read_dir_is_empty(&cur);
        if !empty {
            break;
        }
        match std::fs::remove_dir(&cur) {
            Ok(()) => {
                count += 1;
                current = parent;
            }
            Err(_) => break,
        }
    }
    count
}

fn fs_read_dir_is_empty(dir: &Path) -> bool {
    match std::fs::read_dir(dir) {
        Ok(mut d) => d.next().is_none(),
        Err(_) => false,
    }
}

/// Финальная перепроверка родительских папок (по требованию пользователя):
/// после удаления ВСЕХ пунктов списка родитель может опустеть только в самом
/// конце — например, в списке были «сестринские» D:\X\A и D:\X\B, и D:\X
/// опустел лишь после удаления последней. Для каждого удалённого пути идём
/// вверх по цепочке: пока папка существует, пуста и не является корнем
/// диска/ФС (D:\ или \ являются своим собственным родителем) — удаляем.
/// Непустые папки никогда не трогаем. Возвращает число удалённых папок.
pub fn sweep_empty_parents(deleted: &[PathBuf], stop: &StopToken, log: &LogSink) -> usize {
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut count = 0usize;
    for path in deleted {
        if stop.is_set() {
            break;
        }
        let mut cur = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => continue,
        };
        while let Some(parent) = cur.parent().map(|p| p.to_path_buf()) {
            // Корень диска/ФС: D:\ (или \) является своим собственным родителем.
            if parent == cur {
                break;
            }
            if !fs_read_dir_is_empty(&cur) {
                break; // не существует, не директория или непуста
            }
            match std::fs::remove_dir(&cur) {
                Ok(()) => {
                    if seen.insert(cur.clone()) {
                        count += 1;
                        em!(log, "parent_deleted", cur.display());
                    }
                    cur = parent;
                }
                Err(_) => break,
            }
        }
    }
    count
}
