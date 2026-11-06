//! Сравнение папок по именам файлов.
//!
//! Методы: размер+дата (по умолчанию), побайтовое, хеш SHA-256/MD5.
//! Глубокое сравнение — параллельно (rayon), стоп — между задачами.
//! Отчёт: UTF-8 с BOM, разделитель `;`, колонки фиксированы.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::UNIX_EPOCH;

use rayon::prelude::*;

use crate::hashing::{files_differ_binary, hash_file, HashAlgo};
use crate::pathutil::modified_secs;
use crate::stop::StopToken;
use crate::{emit_progress, LogSink, CoreError};
use crate::em;

fn timestamp() -> String {
    chrono::Local::now().format("%Y%m%d_%H%M%S").to_string()
}

/// Метод сравнения (выбирается в диалоге).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareMethod {
    /// Размер и дата (по умолчанию; порог различия по mtime — 1 с).
    SizeDate,
    /// Побайтовое сравнение.
    Binary,
    /// Сравнение хешей.
    Hash(HashAlgo),
}

struct FileInfo {
    path: PathBuf,
    size: u64,
    mtime: f64,
}

fn mtime_iso(meta: &fs::Metadata) -> String {
    let Ok(t) = meta.modified() else {
        return String::new();
    };
    let Ok(d) = t.duration_since(UNIX_EPOCH) else {
        return String::new();
    };
    let Some(dt) = chrono::DateTime::from_timestamp(d.as_secs() as i64, d.subsec_nanos()) else {
        return String::new();
    };
    let local = chrono::DateTime::<chrono::Local>::from(dt);
    if d.subsec_nanos() == 0 {
        local.format("%Y-%m-%dT%H:%M:%S").to_string()
    } else {
        local.format("%Y-%m-%dT%H:%M:%S.%6f").to_string()
    }
}

/// Читает пары папок из конфига:
/// строки вида `A;B` либо поочерёдные строки A, B, A, B...
fn read_compare_pairs(path: &Path) -> Vec<(PathBuf, PathBuf)> {
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    let text = crate::config::decode_config(&bytes);
    let raw: Vec<String> = text.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
    if raw.is_empty() {
        return Vec::new();
    }
    let has_semicolon = raw.iter().any(|l| l.contains(';'));
    let mut pairs = Vec::new();
    if has_semicolon {
        for line in &raw {
            let mut it = line.split(';');
            let a = it.next().unwrap_or("").trim();
            let b = it.next().unwrap_or("").trim();
            if !a.is_empty() && !b.is_empty() {
                pairs.push((a.into(), b.into()));
            }
        }
    } else {
        let mut i = 0;
        while i + 1 < raw.len() {
            if !raw[i].is_empty() && !raw[i + 1].is_empty() {
                pairs.push((raw[i].clone().into(), raw[i + 1].clone().into()));
            }
            i += 2;
        }
    }
    pairs
}

/// Сбор файлов папки по именам:
/// имя -> {путь, размер, mtime}. Корневые файлы имеют приоритет
/// (сначала собираются в основном потоке), далее —
/// топ-подпапки параллельно; первое вхождение имени побеждает.
fn collect_files_by_name(root: &Path, stop: &StopToken) -> BTreeMap<String, FileInfo> {
    let mut result: BTreeMap<String, FileInfo> = BTreeMap::new();
    if let Ok(rd) = fs::read_dir(root) {
        for e in rd.flatten() {
            if stop.is_set() {
                break;
            }
            let Ok(ft) = e.file_type() else { continue };
            if !ft.is_file() {
                continue;
            }
            let Ok(st) = e.metadata() else { continue };
            result.entry(e.file_name().to_string_lossy().into_owned()).or_insert_with(|| {
                FileInfo {
                    path: e.path(),
                    size: st.len(),
                    mtime: modified_secs(&st),
                }
            });
        }
    }
    let subdirs: Vec<PathBuf> = fs::read_dir(root).into_iter().flatten().flatten().filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false)).map(|e| e.path()).collect();
    let stop_r = stop.clone();
    let maps: Vec<BTreeMap<String, FileInfo>> = subdirs.into_par_iter().map(|dir| {
            let mut local = BTreeMap::new();
            for entry in walkdir::WalkDir::new(&dir).follow_links(false).into_iter().filter_entry(|_e| !stop_r.is_set())
            {
                let Ok(entry) = entry else { continue };
                let ft = entry.file_type();
                if !ft.is_file() {
                    continue;
                }
                let Ok(st) = entry.metadata() else { continue };
                local.entry(entry.file_name().to_string_lossy().into_owned()).or_insert_with(|| FileInfo {
                        path: entry.path().to_path_buf(),
                        size: st.len(),
                        mtime: modified_secs(&st),
                    });
            }
            local
        }).collect();
    for m in maps {
        for (name, info) in m {
            result.entry(name).or_insert(info);
        }
    }
    result
}

struct Row {
    file_name: String,
    path_in_a: String,
    path_in_b: String,
    size_a_mb: String,
    size_b_mb: String,
    modified_a: String,
    modified_b: String,
    different: String,
    deep: Option<usize>,
}

/// Сравнивает пары папок из конфига.
/// Возвращает пути сохранённых отчётов.
pub fn compare_folders(
    config: &Path,
    method: CompareMethod,
    output_csv: Option<&Path>,
    stop: &StopToken,
    log: &LogSink,
) -> Result<Vec<PathBuf>, CoreError> {
    stop.reset();
    if !config.exists() {
        return Err(CoreError::InvalidPath(
            crate::i18n::fill(
                crate::i18n::tr(crate::i18n::global(), "cmp_config_missing"),
                &[config.display().to_string()],
            )
        ));
    }
    let pairs = read_compare_pairs(config);
    if pairs.is_empty() {
        em!(log, "cmp_no_valid_pairs");
        return Ok(Vec::new());
    }

    let mut reports = Vec::new();
    // Прогресс: 2 единицы на пару (скан + отчёт) + по одной на файл deep-сравнения
    // (total расширяется по мере того, как становятся известны deep-задачи).
    let mut total_units = pairs.len() * 2;
    let mut completed = 0usize;
    for (path_a, path_b) in &pairs {
        if stop.is_set() {
            em!(log, "cmp_aborted");
            break;
        }
        emit_progress(log, completed, total_units);
        if !path_a.is_dir() {
            em!(log, "cmp_a_missing", path_a.display());
            continue;
        }
        if !path_b.is_dir() {
            em!(log, "cmp_b_missing", path_b.display());
            continue;
        }
        let out_path = match output_csv {
            Some(o) if pairs.len() == 1 => o.to_path_buf(),
            _ => crate::resolve_report_path(format!(
                "compare_{}_vs_{}_{}.csv",
                path_a.file_name().and_then(|n| n.to_str()).unwrap_or("A"),
                path_b.file_name().and_then(|n| n.to_str()).unwrap_or("B"),
                timestamp()
            )),
        };
        em!(log, "cmp_header", path_a.display(), path_b.display());
        em!(log, "cmp_scanning2");
        let files_a = collect_files_by_name(path_a, stop);
        if stop.is_set() {
            return Ok(reports);
        }
        let files_b = collect_files_by_name(path_b, stop);
        if stop.is_set() {
            return Ok(reports);
        }
        completed += 1;
        emit_progress(log, completed, total_units);
        let all_names: BTreeSet<String> =
            files_a.keys().cloned().chain(files_b.keys().cloned()).collect();
        em!(log, "cmp_unique_names", all_names.len());

        struct DeepTask {
            a: PathBuf,
            b: PathBuf,
        }
        let mut rows: Vec<Row> = Vec::new();
        let mut deep_tasks: Vec<DeepTask> = Vec::new();

        for name in &all_names {
            let a = files_a.get(name);
            let b = files_b.get(name);
            let mut different = String::new();
            let mut deep: Option<usize> = None;
            match (a, b) {
                (Some(a), Some(b)) => {
                    if a.size != b.size {
                        different = "Yes".into();
                    } else {
                        match method {
                            CompareMethod::Hash(algo) => {
                                deep = Some(deep_tasks.len());
                                deep_tasks.push(DeepTask {
                                    a: a.path.clone(),
                                    b: b.path.clone(),
                                });
                                // алгоритм запоминается в методе (один на запуск)
                                let _ = algo;
                            }
                            CompareMethod::Binary => {
                                deep = Some(deep_tasks.len());
                                deep_tasks.push(DeepTask {
                                    a: a.path.clone(),
                                    b: b.path.clone(),
                                });
                            }
                            CompareMethod::SizeDate => {
                                different = if (a.mtime - b.mtime).abs() > 1.0 {
                                    "Yes".into()
                                } else {
                                    "No".into()
                                };
                            }
                        }
                    }
                }
                _ => different = "Yes".into(),
            }
            rows.push(Row {
                file_name: name.clone(),
                path_in_a: a.map(|i| i.path.to_string_lossy().into_owned()).unwrap_or_else(|| "N/A".into()),
                path_in_b: b.map(|i| i.path.to_string_lossy().into_owned()).unwrap_or_else(|| "N/A".into()),
                size_a_mb: a.map(|i| format!("{:.2}", i.size as f64 / (1024.0 * 1024.0))).unwrap_or_default(),
                size_b_mb: b.map(|i| format!("{:.2}", i.size as f64 / (1024.0 * 1024.0))).unwrap_or_default(),
                modified_a: a.and_then(|i| fs::metadata(&i.path).ok()).map(|m| mtime_iso(&m)).unwrap_or_default(),
                modified_b: b.and_then(|i| fs::metadata(&i.path).ok()).map(|m| mtime_iso(&m)).unwrap_or_default(),
                different,
                deep,
            });
        }

        if !deep_tasks.is_empty() {
            em!(log, "cmp_deep_tasks", deep_tasks.len());
            total_units += deep_tasks.len();
            emit_progress(log, completed, total_units);
            let method_r = method;
            let stop_r = stop.clone();
            let log_r = log.clone();
            let deep_done = std::sync::Arc::new(AtomicUsize::new(0));
            let deep_done_r = std::sync::Arc::clone(&deep_done);
            let base = completed;
            let deep_total = deep_tasks.len();
            let results: Vec<bool> = deep_tasks.par_iter().map(|t| {
                    let r = if stop_r.is_set() {
                        false
                    } else {
                        match method_r {
                            CompareMethod::Hash(algo) => match (hash_file(&t.a, algo), hash_file(&t.b, algo)) {
                                (Some(ha), Some(hb)) => ha != hb,
                                _ => true, // ошибка чтения = «различаются»
                            },
                            CompareMethod::Binary => files_differ_binary(&t.a, &t.b),
                            CompareMethod::SizeDate => false,
                        }
                    };
                    let done = deep_done_r.fetch_add(1, Ordering::Relaxed) + 1;
                    if done.is_multiple_of(100) || done == deep_total {
                        emit_progress(&log_r, base + done, total_units);
                    }
                    r
                }).collect();
            completed += deep_total;
            for row in rows.iter_mut() {
                if let Some(d) = row.deep.take() {
                    if d < results.len() {
                        row.different = if results[d] { "Yes" } else { "No" }.into();
                    }
                }
            }
        }

        // Отчёт: UTF-8 BOM, разделитель ';' (Excel RU)
        let write_report = |out_path: &Path, rows: &[Row]| -> Result<(), String> {
            let mut file = File::create(out_path).map_err(|e| e.to_string())?;
            file.write_all(b"\xEF\xBB\xBF").map_err(|e| e.to_string())?;
            let mut w = csv::WriterBuilder::new().delimiter(b';').terminator(csv::Terminator::CRLF).from_writer(BufWriter::new(file));
            w.write_record([
                "file_name", "path_in_a", "path_in_b", "size_a_mb", "size_b_mb",
                "modified_a", "modified_b", "different",
            ]).map_err(|e| e.to_string())?;
            for r in rows {
                w.write_record([
                    r.file_name.as_str(), r.path_in_a.as_str(), r.path_in_b.as_str(),
                    r.size_a_mb.as_str(), r.size_b_mb.as_str(), r.modified_a.as_str(),
                    r.modified_b.as_str(), r.different.as_str(),
                ]).map_err(|e| e.to_string())?;
            }
            w.flush().map_err(|e| e.to_string())
        };

        match write_report(&out_path, &rows) {
            Ok(()) => {
                let diff_count = rows.iter().filter(|r| r.different == "Yes").count();
                em!(log, "report_saved", out_path.display());
                em!(log, "cmp_stats", rows.len(), diff_count);
                let final_path = crate::finalize_report_logged(out_path, Some(log));
                reports.push(final_path);
            }
            Err(e) => em!(log, "report_write_error", e),
        }
        completed += 1;
        emit_progress(log, completed, total_units);
    }
    emit_progress(log, total_units, total_units);
    Ok(reports)
}
