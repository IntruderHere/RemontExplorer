//! Удаление файлов/папок из списка.
//!
//! Семантика: файлы и папки, опциональная очистка пустых родительских папок.
//!
//! 1. Поштучная очистка: после удаления каждой папки пустые
//!    родители очищаются вверх, но не выше верхнего из указанных
//!    пользователем путей (и их прямого родителя).
//! 2. Финальный проход: после обработки ВСЕХ пунктов списка родительские
//!    цепочки перепроверяются — папки, ставшие пустыми именно в процессе
//!    удаления (сестринские папки удалялись по очереди), удаляются также;
//!    единственная защита — корень диска/ФС (D:\ никогда не удаляется).
//!
//! Способы удаления ([`DeleteMode`]):
//! - `Permanent` — безвозвратно;
//! - `Recycle` — в корзину Windows (можно восстановить штатными средствами);
//! - `Archive` — сначала ZIP-архив всех удаляемых элементов, затем удаление.
//!
//! При `Recycle`/`Archive` безвозвратного удаления не происходит, если
//! подготовительный шаг не удался: безопаснее не удалить, чем потерять.
//!
//! Битые символические ссылки тоже удаляются: проверка существования
//! для них не проходит, но удалить их безопасно.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use crate::pathutil::{delete_empty_parents, sweep_empty_parents, topmost_listed_ancestor};
use crate::recycle;
use crate::stop::StopToken;
use crate::em;
use crate::{emit_progress, resolve_report_path, LogSink};

/// Как именно удалять.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeleteMode {
    /// Безвозвратно.
    #[default]
    Permanent,
    /// В корзину Windows (только Windows).
    Recycle,
    /// Сначала архив ZIP, потом безвозвратное удаление.
    Archive,
}

impl DeleteMode {
    /// Все режимы — для выпадающего списка настроек.
    pub const ALL: [DeleteMode; 3] = [
        DeleteMode::Permanent,
        DeleteMode::Recycle,
        DeleteMode::Archive,
    ];

    /// Подпись для интерфейса.
    pub fn label(self, lang: crate::i18n::Lang) -> &'static str {
        use crate::i18n::Lang;
        match (self, lang) {
            (DeleteMode::Permanent, Lang::Ru) => "Безвозвратно",
            (DeleteMode::Recycle, Lang::Ru) => "В корзину",
            (DeleteMode::Archive, Lang::Ru) => "В архив ZIP",
            (DeleteMode::Permanent, Lang::En) => "Permanently",
            (DeleteMode::Recycle, Lang::En) => "To Recycle Bin",
            (DeleteMode::Archive, Lang::En) => "To ZIP archive",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct DeleteStats {
    pub folders: usize,
    pub files: usize,
    pub size_bytes: u64,
    pub parents: usize,
    /// Сколько элементов отправлено в корзину.
    pub recycled: usize,
    /// Созданный перед удалением архив.
    pub archive: Option<PathBuf>,
    /// Ошибки, из-за которых элементы не удалены.
    pub errors: usize,
}

/// Размер директории (рекурсивно, быстрая обходная версия).
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

fn timestamp() -> String {
    chrono::Local::now().format("%Y%m%d_%H%M%S").to_string()
}

/// Упаковать пути в ZIP (имена — относительно общего родителя).
///
/// Возвращает число записанных файлов. Архив по умолчанию кладётся в папку
/// результатов (`deleted_<время>.zip`).
pub fn archive_paths(paths: &[PathBuf], zip_path: &Path, log: &LogSink) -> io::Result<usize> {
    if let Some(dir) = zip_path.parent() {
        let _ = fs::create_dir_all(dir);
        // Место под архив проверяем до создания файла.
        let need = crate::disk::total_size(paths);
        if let Err((need, free)) = crate::disk::check_space(need, dir) {
            em!(
                log,
                "copy_space_fail",
                dir.display(),
                crate::disk::human(need),
                crate::disk::human(free)
            );
            return Err(io::Error::other("недостаточно места для архива"));
        }
    }
    let file = File::create(zip_path)?;
    let mut zip = zip::ZipWriter::new(file);
    let mut written = 0usize;
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    for path in paths {
        // symlink_metadata: битые ссылки тоже попадают в архив как записи
        let Ok(md) = fs::symlink_metadata(path) else { continue };
        let base = path.parent().unwrap_or(path);
        if md.is_dir() {
            for entry in walkdir::WalkDir::new(path).follow_links(false).into_iter().flatten() {
                let ep = entry.path();
                let Ok(rel) = ep.strip_prefix(base) else { continue };
                let name = rel.to_string_lossy().replace('\\', "/");
                if entry.file_type().is_dir() {
                    zip.add_directory(&name, opts)?;
                } else if entry.file_type().is_file() {
                    zip.start_file(&name, opts)?;
                    let mut f = File::open(ep)?;
                    io::copy(&mut f, &mut zip)?;
                    written += 1;
                    if written.is_multiple_of(25) {
                        // Точное число файлов заранее неизвестно — доля не считается.
                        emit_progress(log, written, 0);
                    }
                }
            }
        } else if md.is_file() {
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            zip.start_file(&name, opts)?;
            let mut f = File::open(path)?;
            io::copy(&mut f, &mut zip)?;
            written += 1;
        }
    }
    zip.finish()?;
    Ok(written)
}

fn delete_one(
    path: &Path,
    recursive_parents: bool,
    list: &[PathBuf],
    stats: &mut DeleteStats,
) -> io::Result<()> {
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
                let boundary = topmost_listed_ancestor(path, list)
                    .unwrap_or_else(|| parent.to_path_buf());
                stats.parents += delete_empty_parents(parent, Some(&boundary));
            }
        }
    }
    Ok(())
}

/// Удаляет пути безвозвратно (совместимая обёртка над [`delete_paths_opts`]).
pub fn delete_paths(
    paths: &[PathBuf],
    recursive_parents: bool,
    stop: &StopToken,
    log: &LogSink,
) -> DeleteStats {
    delete_paths_opts(paths, recursive_parents, DeleteMode::Permanent, stop, log)
}

/// Удаляет пути выбранным способом.
///
/// Для `Permanent` после основного цикла выполняется финальный проход по
/// родительским цепочкам (см. модуль).
pub fn delete_paths_opts(
    paths: &[PathBuf],
    recursive_parents: bool,
    mode: DeleteMode,
    stop: &StopToken,
    log: &LogSink,
) -> DeleteStats {
    let mut stats = DeleteStats::default();
    let existing: Vec<PathBuf> = paths
        .iter()
        .filter(|p| fs::symlink_metadata(p).is_ok())
        .cloned()
        .collect();
    if existing.is_empty() {
        emit_progress(log, 0, 0);
        return stats;
    }

    // 1. Архив перед удалением.
    if mode == DeleteMode::Archive {
        let zip_path = resolve_report_path(format!("deleted_{}.zip", timestamp()));
        em!(log, "delete_archive_start", zip_path.display());
        match archive_paths(&existing, &zip_path, log) {
            Ok(n) => {
                em!(log, "delete_archive_ok", zip_path.display(), n);
                stats.archive = Some(zip_path);
            }
            Err(e) => {
                let _ = fs::remove_file(&zip_path);
                em!(log, "delete_archive_failed", e.to_string());
                stats.errors += 1;
                return stats;
            }
        }
    }

    // 2. Корзина: одна операция оболочки на весь список.
    if mode == DeleteMode::Recycle {
        if !recycle::is_supported() {
            em!(log, "delete_recycle_unsupported");
            stats.errors += 1;
            return stats;
        }
        // Размеры считаем до перемещения — после пути уже нет.
        for p in &existing {
            if let Ok(md) = fs::symlink_metadata(p) {
                if md.is_dir() {
                    stats.folders += 1;
                    stats.size_bytes += dir_size(p);
                } else {
                    stats.files += 1;
                    stats.size_bytes += md.len();
                }
            }
        }
        em!(log, "delete_recycle_start", existing.len());
        match recycle::move_to_recycle_bin(&existing) {
            Ok(n) => {
                stats.recycled = n;
                em!(log, "delete_recycle_ok", n);
                if recursive_parents {
                    stats.parents += sweep_empty_parents(&existing, stop, log);
                }
                emit_progress(log, existing.len(), existing.len());
                return stats;
            }
            Err(e) => {
                em!(log, "delete_recycle_failed", e.to_string());
                stats.errors += 1;
                return stats;
            }
        }
    }

    // 3. Безвозвратное удаление (Permanent и Archive после успешного архива).
    let mut deleted_ok: Vec<PathBuf> = Vec::new();
    for (i, path) in existing.iter().enumerate() {
        emit_progress(log, i, existing.len());
        if stop.is_set() {
            em!(log, "delete_aborted");
            break;
        }
        match delete_one(path, recursive_parents, paths, &mut stats) {
            Ok(()) => deleted_ok.push(path.clone()),
            Err(e) => {
                stats.errors += 1;
                em!(log, "delete_error", path.display(), e.to_string());
            }
        }
    }
    // Финальная перепроверка: родители, опустевшие только после удаления
    // всех пунктов списка (сестринские папки), тоже удаляем.
    if recursive_parents && !deleted_ok.is_empty() {
        stats.parents += sweep_empty_parents(&deleted_ok, stop, log);
    }
    emit_progress(log, existing.len(), existing.len());
    stats
}
