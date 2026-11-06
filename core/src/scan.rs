//! Сканирование папки в CSV (порт `scan_folder` из Python-версии).
//!
//! Формат отчёта (парность с Python): UTF-8 с BOM, разделитель `;`
//! (Excel RU), колонки:
//! `source_root;full_path;parent_dir;name;extension;size_mb;index_ops`.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;

use crate::pathutil::modified_secs;
use crate::stop::StopToken;
use crate::{emit_progress, LogSink};
use crate::em;

/// Эквивалент Python-регулярки `(?<!\d)\d{6}(?!\d)` + `.search`
/// (regex crate look-around не поддерживает): первое вхождение
/// ровно 6 цифр, не входящих в более длинную последовательность.
fn extract_index_ops(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i - start == 6 {
                return s[start..i].to_string();
            }
        } else {
            i += 1;
        }
    }
    String::new()
}

fn timestamp() -> String {
    chrono::Local::now().format("%Y%m%d_%H%M%S").to_string()
}

#[derive(Debug, Clone, Default)]
pub struct ExtStat {
    pub count: usize,
    pub size_mb: f64,
}

#[derive(Debug, Clone, Default)]
pub struct ScanStats {
    pub files: usize,
    pub size_mb: f64,
    pub by_ext: BTreeMap<String, ExtStat>,
    /// Файлы, для которых не удалось прочитать метаданные (нет прав и т. п.).
    pub meta_errors: usize,
}

struct FileRow {
    full_path: PathBuf,
    parent_dir: PathBuf,
    name: String,
    extension: String,
    size_mb: f64,
    index_ops: String,
}

enum Task {
    /// Топ-подпапка (рекурсивно).
    Dir(PathBuf),
    /// Собственные файлы корневой папки.
    RootFiles(Vec<PathBuf>),
}

fn file_row(path: &Path, size: u64) -> Option<FileRow> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    // Парность с Python: `os.path.splitext(name)[1]` — расширение С ТОЧКОЙ
    // (".txt"); пустое -> "no_extension".
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
        .unwrap_or_else(|| "no_extension".into());
    let index_ops = extract_index_ops(&path.to_string_lossy());
    Some(FileRow {
        full_path: path.to_path_buf(),
        parent_dir: path.parent().map(|p| p.to_path_buf()).unwrap_or_default(),
        name,
        extension: ext,
        size_mb: size as f64 / (1024.0 * 1024.0),
        index_ops,
    })
}

fn scan_task(
    task: &Task,
    stop: &StopToken,
    counter: &AtomicUsize,
    total: usize,
    log: &LogSink,
) -> (Vec<FileRow>, ScanStats) {
    let mut rows = Vec::new();
    let mut stats = ScanStats::default();
    let mut visit = |path: &Path, size: Option<u64>| {
        if stop.is_set() {
            return;
        }
        if size.is_none() {
            stats.meta_errors += 1;
        }
        let Some(row) = file_row(path, size.unwrap_or(0)) else {
            return;
        };
        let done = counter.fetch_add(1, Ordering::Relaxed) + 1;
        if done.is_multiple_of(500) || done == total {
            emit_progress(log, done, total);
        }
        let ext = row.extension.clone();
        stats.files += 1;
        stats.size_mb += row.size_mb;
        let es = stats.by_ext.entry(ext).or_default();
        es.count += 1;
        es.size_mb += row.size_mb;
        rows.push(row);
    };
    match task {
        Task::Dir(dir) => {
            for entry in walkdir::WalkDir::new(dir)
                .follow_links(false)
                .into_iter()
                .filter_entry(|_e| !stop.is_set())
            {
                let Ok(entry) = entry else { continue };
                let ft = entry.file_type();
                if !ft.is_file() {
                    continue;
                }
                visit(entry.path(), entry.metadata().map(|m| m.len()).ok());
            }
        }
        Task::RootFiles(files) => {
            for path in files {
                visit(path, std::fs::metadata(path).map(|m| m.len()).ok());
            }
        }
    }
    (rows, stats)
}

/// Подсчёт файлов в задаче (только обход, без метаданных) — для прогресс-бара.
/// С поддержкой стоп-сигнала: после нажатия «Остановить» обход каждого
/// задания обрывается на следующем же входе, не дожидаясь конца дерева.
fn count_task(task: &Task, stop: &StopToken) -> usize {
    match task {
        Task::Dir(dir) => walkdir::WalkDir::new(dir)
            .follow_links(false)
            .into_iter()
            .filter_entry(|_e| !stop.is_set())
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .count(),
        Task::RootFiles(files) => files.len(),
    }
}

/// Обе фазы сканирования на ТЕКУЩЕМ пуле rayon:
/// 1) параллельный предподсчёт общего числа файлов (для прогресс-бара);
/// 2) само сканирование.
///
/// Обе фазы проверяют стоп-сигнал: если он задан во время предподсчёта,
/// фаза сканирования не начинается вовсе.
///
/// Вызывается либо внутри `pool.install` (пользовательский пул),
/// либо напрямую (глобальный пул — фолбэк).
fn run_scan_tasks(
    tasks: &[Task],
    stop: &StopToken,
    counter: &AtomicUsize,
    log: &LogSink,
) -> (usize, Vec<(Vec<FileRow>, ScanStats)>) {
    let total_files: usize = tasks.par_iter().map(|t| count_task(t, stop)).sum();
    emit_progress(log, 0, total_files);
    if stop.is_set() {
        // Стоп получен во время предподсчёта — скан не начинаем.
        return (total_files, Vec::new());
    }
    let per_task: Vec<(Vec<FileRow>, ScanStats)> = tasks
        .par_iter()
        .map(|t| scan_task(t, stop, counter, total_files, log))
        .collect();
    (total_files, per_task)
}

/// Сканирует `root` в CSV-отчёт (порт `scan_folder`).
///
/// `master_file` — общий файл пакетного сканирования (append); если `None`,
/// создаётся новый `files_info_<имя>_<timestamp>.csv`.
/// `max_threads` — размер пула потоков для обхода дерева (1..=512);
/// при 0 или ошибке создания пула используется глобальный пул rayon.
/// Возвращает (путь отчёта, статистика).
pub fn scan_folder(
    root: &Path,
    master_file: Option<&Path>,
    max_threads: usize,
    stop: &StopToken,
    log: &LogSink,
) -> Option<(PathBuf, ScanStats)> {
    let root = match root.canonicalize() {
        Ok(r) => r,
        Err(e) => {
            em!(log, "scan_not_found", root.display(), e.to_string());
            return None;
        }
    };
    if !root.is_dir() {
        em!(log, "scan_not_dir", root.display());
        return None;
    }
    let safe_name: String = root
        .file_name()
        .map(|n| {
            n.to_string_lossy()
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-' || *c == '_')
                .collect::<String>()
        })
        .unwrap_or_default();
    let safe_name = safe_name.replace(' ', "_");
    let ts = timestamp();
    let output_file = match master_file {
        Some(m) => m.to_path_buf(),
        None => crate::resolve_report_path(format!("files_info_{safe_name}_{ts}.csv")),
    };
    let is_master = master_file.is_some();
    let new_file = output_file.metadata().map(|m| m.len() == 0).unwrap_or(true);

    let mut file = match File::options()
        .create(true)
        .append(is_master)
        .write(!is_master)
        .open(&output_file)
    {
        Ok(f) => f,
        Err(e) => {
            em!(log, "scan_report_create_fail", output_file.display(), e.to_string());
            return None;
        }
    };
    if new_file {
        let _ = file.write_all(b"\xEF\xBB\xBF"); // UTF-8 BOM (utf-8-sig, как в Python)
    }
    // CRLF — как lineterminator по умолчанию у csv-модуля Python (byte-parity)
    let mut writer = csv::WriterBuilder::new()
        .delimiter(b';')
        .terminator(csv::Terminator::CRLF)
        .from_writer(BufWriter::new(file));
    if !is_master || new_file {
        let _ = writer.write_record([
            "source_root", "full_path", "parent_dir", "name", "extension", "size_mb", "index_ops",
        ]);
    }

    // Задачи: топ-подпапки + собственные файлы корня (как в Python).
    let mut tasks: Vec<Task> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&root) {
        for entry in rd.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                tasks.push(Task::Dir(entry.path()));
            }
        }
    }
    let root_files: Vec<PathBuf> = std::fs::read_dir(&root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    if !root_files.is_empty() {
        tasks.push(Task::RootFiles(root_files));
    }

    em!(log, "scan_start", root.display());

    // Пул потоков по настройке. Предподсчёт (для прогресс-бара) и само
    // сканирование идут в ЭТОМ пуле параллельно по топ-подпапкам —
    // на сетевых ресурсах это заметно быстрее одиночного предподсчёта.
    let counter = AtomicUsize::new(0);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(max_threads.clamp(1, 512))
        .build();
    let (total_files, per_task) = match pool {
        Ok(ref p) => p.install(|| run_scan_tasks(&tasks, stop, &counter, log)),
        Err(_) => run_scan_tasks(&tasks, stop, &counter, log),
    };

    let mut total = ScanStats::default();
    let mut done = 0usize;
    let mut prev_k = 0usize;
    for (rows, stats) in per_task {
        for r in &rows {
            let _ = writer.write_record([
                root.to_string_lossy().as_ref(),
                r.full_path.to_string_lossy().as_ref(),
                r.parent_dir.to_string_lossy().as_ref(),
                r.name.as_str(),
                r.extension.as_str(),
                &format!("{:.2}", r.size_mb),
                r.index_ops.as_str(),
            ]);
        }
        let _ = writer.flush();
        done += rows.len();
        total.files += stats.files;
        total.size_mb += stats.size_mb;
        total.meta_errors += stats.meta_errors;
        for (ext, es) in stats.by_ext {
            let t = total.by_ext.entry(ext).or_default();
            t.count += es.count;
            t.size_mb += es.size_mb;
        }
        if done / 1000 > prev_k {
            em!(log, "scan_progress", done);
            prev_k = done / 1000;
        }
    }
    emit_progress(log, total.files, total_files);
    if let Err(e) = writer.flush() {
        em!(log, "scan_report_finish_fail", output_file.display(), e.to_string());
    }
    if total.meta_errors > 0 {
        em!(log, "scan_meta_warn", total.meta_errors);
    }
    // Формат отчёта: при XLSX CSV конвертируется и удаляется. Пакетный файл
    // (is_master) конвертируется один раз — после обхода всех папок.
    let output_file = if is_master {
        output_file
    } else {
        crate::finalize_report_logged(output_file, Some(log))
    };
    if !is_master {
        if stop.is_set() {
            // Стоп нажат в процессе: отчёт частичный (написано то, что
            // успело просканироваться до сигнала остановки).
            em!(log, "scan_stop", total.files,
                    output_file.display());
        } else {
            em!(log, "scan_done", total.files,
                    output_file.display());
        }
    }
    Some((output_file, total))
}

#[allow(dead_code)]
pub(crate) fn mtime_secs_meta(meta: &std::fs::Metadata) -> f64 {
    modified_secs(meta)
}

#[cfg(test)]
mod tests {
    use super::extract_index_ops;

    #[test]
    fn index_ops_rule() {
        // ровно 6 цифр -> токен
        assert_eq!(extract_index_ops("abc123456def"), "123456");
        // 8-значная дата (как 20241015) -> ложных 6 цифр нет
        assert_eq!(extract_index_ops("date_20241015.bin"), "");
        // 7 цифр -> нет
        assert_eq!(extract_index_ops("x1234567y"), "");
        // 5 цифр -> нет
        assert_eq!(extract_index_ops("x12345y"), "");
        // первое вхождение побеждает
        assert_eq!(extract_index_ops("123456-789012"), "123456");
        // длинная последовательность блокирует начало 6-цифрового окна
        assert_eq!(extract_index_ops("a9999999b123456"), "123456");
        // 6 цифр тоже могут быть «первым» вхождением
        assert_eq!(extract_index_ops("a999999b123456"), "999999");
        assert_eq!(extract_index_ops("1234567890"), "");
        // нет цифр
        assert_eq!(extract_index_ops("no_digits_here"), "");
    }

    #[test]
    fn scan_folder_honors_stop_before_start() {
        use crate::{scan_folder, stdout_sink, StopToken};
        let dir = tempfile::TempDir::new().unwrap();
        for i in 0..20 {
            let d = dir.path().join(format!("d{i}"));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join(format!("f{i}.txt")), b"x").unwrap();
        }
        let stop = StopToken::new();
        stop.stop(); // стоп задан ДО начала: и предподсчёт, и скан должны сработать
        let log = stdout_sink();
        let res = scan_folder(dir.path(), None, 4, &stop, &log);
        let (path, stats) = res.expect("report created (empty)");
        assert_eq!(stats.files, 0, "nothing scanned — stop was set");
        let _ = std::fs::remove_file(path);
    }
}
