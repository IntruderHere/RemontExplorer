//! Поиск дубликатов.
//!
//! Алгоритм: группировка по размеру -> хеширование кандидатов (rayon) ->
//! группировка по хешу. Отчёт: UTF-8 BOM, разделитель `;`.

use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;

use crate::hashing::{hash_file, HashAlgo};
use crate::stop::StopToken;
use crate::{emit, emit_progress, LogSink, CoreError};
use crate::em;

fn timestamp() -> String {
    chrono::Local::now().format("%Y%m%d_%H%M%S").to_string()
}

struct GroupItem {
    path: PathBuf,
    size: u64,
    hash: String,
}

/// Пакетный поиск дубликатов в наборе папок.
/// Возвращает группы дубликатов (пути).
pub fn find_duplicates_multi(
    paths: &[PathBuf],
    algo: HashAlgo,
    output_csv: Option<&Path>,
    stop: &StopToken,
    log: &LogSink,
) -> Result<Vec<Vec<PathBuf>>, CoreError> {
    stop.reset();
    em!(log, "dup_header");
    em!(log, "dup_folders", paths.len());
    for p in paths {
        emit(log, format!("  - {}", p.display()));
    }
    em!(log, "dup_collect");

    let stop_r = stop.clone();
    let log_r = log.clone();
    let col_done = std::sync::Arc::new(AtomicUsize::new(0));
    let col_done_r = std::sync::Arc::clone(&col_done);
    let n_paths = paths.len();
    let collected: Vec<Vec<(PathBuf, u64)>> = paths.par_iter().map(|p| {
            let mut out = Vec::new();
            if !stop_r.is_set() {
                if !p.is_dir() {
                    em!(&log_r, "dup_skip_nondir", p.display());
                } else {
                    em!(&log_r, "dup_scanning", p.display());
                    for entry in walkdir::WalkDir::new(p).follow_links(false).into_iter().filter_entry(|_e| !stop_r.is_set())
                    {
                        let Ok(entry) = entry else { continue };
                        let ft = entry.file_type();
                        if !ft.is_file() {
                            continue;
                        }
                        let Ok(st) = entry.metadata() else { continue };
                        out.push((entry.path().to_path_buf(), st.len()));
                    }
                }
            }
            let done = col_done_r.fetch_add(1, Ordering::Relaxed) + 1;
            emit_progress(&log_r, done, n_paths);
            out
        }).collect();
    let files: Vec<(PathBuf, u64)> = collected.into_iter().flatten().collect();

    if files.is_empty() {
        em!(log, "dup_no_files");
        return Ok(Vec::new());
    }
    em!(log, "dup_found_files", files.len());

    let mut by_size: std::collections::BTreeMap<u64, Vec<(PathBuf, u64)>> =
        std::collections::BTreeMap::new();
    for f in files {
        by_size.entry(f.1).or_default().push(f);
    }
    let candidates: Vec<Vec<(PathBuf, u64)>> =
        by_size.into_values().filter(|v| v.len() > 1).collect();
    let total_to_hash: usize = candidates.iter().map(|v| v.len()).sum();
    em!(log, "dup_potential", total_to_hash, candidates.len());
    if candidates.is_empty() {
        em!(log, "dup_none");
        return Ok(Vec::new());
    }
    em!(log, "dup_hashing", algo.name());

    let paths_to_hash: Vec<PathBuf> = candidates.into_iter().flatten().map(|(p, _)| p).collect();
    let stop_r = stop.clone();
    let log_r = log.clone();
    let hash_done = std::sync::Arc::new(AtomicUsize::new(0));
    let hash_done_r = std::sync::Arc::clone(&hash_done);
    emit_progress(log, 0, total_to_hash);
    let hashes: Vec<(PathBuf, Option<String>)> = paths_to_hash.par_iter().map(|p| {
            let h = if stop_r.is_set() {
                None
            } else {
                hash_file(p, algo)
            };
            let done = hash_done_r.fetch_add(1, Ordering::Relaxed) + 1;
            if done.is_multiple_of(100) || done == total_to_hash {
                emit_progress(&log_r, done, total_to_hash);
            }
            (p.clone(), h)
        }).collect();

    let mut by_hash: std::collections::BTreeMap<String, Vec<GroupItem>> =
        std::collections::BTreeMap::new();
    let mut processed = 0;
    for (p, h) in &hashes {
        processed += 1;
        match h {
            Some(h) => {
                let size = fs::metadata(p).map(|m| m.len()).unwrap_or(0);
                by_hash.entry(h.clone()).or_default().push(GroupItem {
                    path: p.clone(),
                    size,
                    hash: h.clone(),
                });
            }
            None => em!(log, "dup_hash_fail", p.display()),
        }
        if processed % 100 == 0 {
            em!(log, "dup_processed", processed, total_to_hash);
        }
    }

    let groups: Vec<Vec<GroupItem>> = by_hash.into_values().filter(|v| v.len() > 1).collect();
    emit_progress(log, total_to_hash, total_to_hash);
    em!(log, "dup_groups", groups.len());
    for (i, g) in groups.iter().enumerate() {
        em!(log, "dup_group", i + 1, g.len(), g[0].size);
        for item in g {
            emit(log, format!("  {}", item.path.display()));
        }
    }

    let out = output_csv.map(|p| p.to_path_buf()).unwrap_or_else(|| crate::resolve_report_path(format!("duplicates_{}.csv", timestamp())));
    let write_report = |out: &Path, groups: &[Vec<GroupItem>]| -> Result<(), String> {
        let mut file = fs::File::create(out).map_err(|e| e.to_string())?;
        file.write_all(b"\xEF\xBB\xBF").map_err(|e| e.to_string())?;
        let mut w = csv::WriterBuilder::new().delimiter(b';').terminator(csv::Terminator::CRLF).from_writer(BufWriter::new(file));
        w.write_record(["group_id", "full_path", "size_bytes", "hash"]).map_err(|e| e.to_string())?;
        for (idx, group) in groups.iter().enumerate() {
            for item in group {
                w.write_record([
                    &(idx + 1).to_string(),
                    item.path.to_string_lossy().as_ref(),
                    &item.size.to_string(),
                    item.hash.as_str(),
                ]).map_err(|e| e.to_string())?;
            }
        }
        w.flush().map_err(|e| e.to_string())
    };
    match write_report(&out, &groups) {
        Ok(()) => {
            let final_path = crate::finalize_report_logged(out, Some(log));
            em!(log, "report_saved", final_path.display());
        }
        Err(e) => em!(log, "report_write_error", e),
    }

    Ok(groups.into_iter().map(|g| g.into_iter().map(|i| i.path).collect()).collect())
}
