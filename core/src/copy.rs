//! Копирование файлов по списку.
//!
//! Возможности:
//! - возобновление по кэшу `copy_resume.json` (size + mtime источника);
//! - атомарное копирование: `.имя.tmp_*` + rename;
//! - длинные пути Windows через префикс `\\?\` (см. [`crate::pathutil`]);
//! - 3 попытки с задержкой 1.5 с на I/O-ошибки;
//! - проверка сети: DNS + TCP/SMB порт 445;
//! - проверка свободного места на целевом диске до начала (по желанию);
//! - сверка копий по SHA-256 после переноса (по желанию) с отчётом;
//! - стоп: воркеры выходят между задачами, «висящие» копирования
//!   завершаются (пул не сливается).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::read_pairs_csv;
use crate::disk;
use crate::hashing::{hash_file, HashAlgo};
use crate::netcheck::{check_server, CheckResult};
use crate::pathutil::{modified_secs, normalize_long_path, resolve_loose};
use crate::stop::StopToken;
use crate::{emit_progress, LogSink};
use crate::em;

/// Число потоков по умолчанию.
pub const DEFAULT_WORKERS: usize = 16;

/// Дополнительные проверки при копировании.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CopyOptions {
    /// Сверять копию с источником по SHA-256.
    pub verify: bool,
    /// Проверять свободное место на целевом диске до начала.
    pub check_space: bool,
}

/// Итог копирования (для журнала, истории и уведомления).
#[derive(Debug, Clone, Default)]
pub struct CopyReport {
    /// Скопировано в этом запуске.
    pub copied: usize,
    /// Всего задач.
    pub total: usize,
    /// Сколько файлов сверено (при `verify`).
    pub verified: usize,
    /// Пары, содержимое которых не совпало.
    pub mismatches: Vec<(PathBuf, PathBuf)>,
    /// Источники, которых не оказалось на месте.
    pub missing: usize,
}

impl CopyReport {
    /// Всё ли прошло чисто (нет ни потерь, ни расхождений).
    pub fn is_clean(&self) -> bool {
        self.copied == self.total && self.mismatches.is_empty() && self.missing == 0
    }
}
const RETRIES: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_millis(1500);

#[derive(Serialize, Deserialize, Clone, Copy)]
struct CacheEntry {
    size: u64,
    mtime: f64,
}

type Cache = std::collections::BTreeMap<String, CacheEntry>;

fn cache_key(src: &Path, dst: &Path) -> String {
    format!(
        "{}\u{0}{}",
        resolve_loose(src).display(),
        resolve_loose(dst).display()
    )
}

fn load_cache(path: &Path) -> Cache {
    fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_cache(path: &Path, cache: &Cache, log: &LogSink) {
    let Ok(json) = serde_json::to_string_pretty(cache) else {
        return;
    };
    if let Err(e) = fs::write(path, json) {
        em!(log, "copy_cache_save_warn", path.display(), e.to_string());
    }
}

/// Проверка «уже скопировано».
fn is_already_copied(src: &Path, dst: &Path, cache: &Cache) -> bool {
    let key = cache_key(src, dst);
    let Some(entry) = cache.get(&key) else {
        return false;
    };
    let Ok(st) = fs::metadata(src) else {
        return false;
    };
    if st.len() != entry.size || (modified_secs(&st) - entry.mtime).abs() > 1.0 {
        return false;
    }
    fs::metadata(dst).is_ok_and(|d| d.len() == entry.size)
}

/// Атомарное копирование с ретраями.
fn robust_copy(src: &Path, dst: &Path) -> Result<(), std::io::Error> {
    let src = normalize_long_path(src);
    let mut dst = normalize_long_path(dst);
    if dst.is_dir() {
        let name = src.file_name().ok_or_else(|| std::io::Error::other(crate::i18n::tr(crate::i18n::global(), "copy_empty_name")))?;
        dst = normalize_long_path(&dst.join(name));
    }
    let name = dst.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    static COPY_SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = COPY_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dst.parent().unwrap_or_else(|| Path::new(".")).join(format!(
            ".{name}.tmp_{}_{}_{ts}",
            std::process::id(),
            seq
        ));
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut last_err = std::io::Error::other(crate::i18n::tr(crate::i18n::global(), "copy_no_attempt"));
    for attempt in 1..=RETRIES {
        let res = fs::copy(&src, &tmp).and_then(|_| fs::rename(&tmp, &dst));
        match res {
            Ok(()) => {
                // сохраняем метаданные (mtime) вместе с содержимым
                if let Ok(s) = fs::metadata(&src) {
                    let _ = filetime::set_file_mtime(
                        &dst,
                        filetime::FileTime::from_system_time(
                            s.modified().unwrap_or(SystemTime::now()),
                        ),
                    );
                }
                return Ok(());
            }
            Err(e) => {
                last_err = e;
                let _ = fs::remove_file(&tmp);
                if attempt < RETRIES {
                    std::thread::sleep(RETRY_DELAY);
                }
            }
        }
    }
    Err(last_err)
}

/// Копирует файлы из CSV-списка.
/// Возвращает число успешно скопированных в этом запуске.
pub fn copy_files_from_list(
    config_file: &Path,
    resume_log: &Path,
    max_workers: usize,
    resume: bool,
    stop: &StopToken,
    log: &LogSink,
) -> usize {
    copy_files_from_list_opts(
        config_file,
        resume_log,
        max_workers,
        resume,
        CopyOptions::default(),
        stop,
        log,
    )
    .copied
}

/// Копирует файлы из CSV-списка с дополнительными проверками.
///
/// `opts.check_space` — до старта сравнивает суммарный объём источников с
/// свободным местом на каждом целевом диске (с запасом 5%). При нехватке
/// копирование не начинается: иначе половина файлов уже перенесена, а
/// операцию нужно разбирать вручную.
///
/// `opts.verify` — после копирования сверяет SHA-256 источника и копии.
pub fn copy_files_from_list_opts(
    config_file: &Path,
    resume_log: &Path,
    max_workers: usize,
    resume: bool,
    opts: CopyOptions,
    stop: &StopToken,
    log: &LogSink,
) -> CopyReport {
    stop.reset();
    let cache: Cache = if resume {
        let cache = load_cache(resume_log);
        // Кэш существует, но не распарсился — предупреждаем, а не молчим.
        if cache.is_empty() && resume_log.exists() && fs::read(resume_log).is_ok_and(|b| !b.is_empty())
        {
            em!(log, "copy_resume_corrupt");
        }
        cache
    } else {
        Cache::new()
    };
    let pairs = read_pairs_csv(config_file);
    if pairs.is_empty() {
        em!(log, "copy_config_missing", config_file.display());
        return CopyReport::default();
    }

    let mut tasks: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut servers = std::collections::BTreeSet::new();
    let mut skipped = 0usize;
    let mut missing = 0usize;

    for pair in &pairs {
        let from = &pair.from;
        let to = &pair.to;
        if !from.exists() {
            em!(log, "copy_source_missing", from.display());
            missing += 1;
            continue;
        }
        let to_s = to.to_string_lossy().into_owned();
        let dst = if to.exists() && to.is_dir() {
            to.join(from.file_name().unwrap_or_default())
        } else if to_s.ends_with(['\\', '/']) {
            PathBuf::from(to_s.trim_end_matches(['\\', '/'])).join(from.file_name().unwrap_or_default())
        } else {
            to.clone()
        };
        if resume && is_already_copied(from, &dst, &cache) {
            skipped += 1;
            continue;
        }
        let from_s = from.to_string_lossy().into_owned();
        if let Some(rest) = from_s.strip_prefix(r"\\") {
            let server = rest.split(['\\', '/']).next().unwrap_or("").to_string();
            if !server.is_empty() {
                servers.insert(server);
            }
        }
        tasks.push((from.clone(), dst));
    }

    if skipped > 0 {
        em!(log, "copy_skipped", skipped);
    }
    if tasks.is_empty() {
        em!(log, "copy_no_tasks");
        return CopyReport {
            total: missing,
            missing,
            ..Default::default()
        };
    }

    // Свободное место: суммарный объём по каждому целевому диску.
    if opts.check_space {
        let mut per_target: std::collections::HashMap<PathBuf, u64> = std::collections::HashMap::new();
        for (src, dst) in &tasks {
            let size = fs::metadata(src).map(|m| m.len()).unwrap_or(0);
            let root = disk_root(dst);
            *per_target.entry(root).or_insert(0) += size;
        }
        for (root, bytes) in per_target {
            match disk::check_space(bytes, &root) {
                Ok(free) => em!(
                    log,
                    "copy_space_ok",
                    root.display(),
                    disk::human(bytes),
                    disk::human(free)
                ),
                Err((need, free)) => {
                    em!(
                        log,
                        "copy_space_fail",
                        root.display(),
                        disk::human(need),
                        disk::human(free)
                    );
                    return CopyReport {
                        total: tasks.len(),
                        ..Default::default()
                    };
                }
            }
        }
    }

    if !servers.is_empty() {
        em!(log, "copy_netcheck_start");
        let mut unreachable = Vec::new();
        for server in &servers {
            match check_server(server) {
                CheckResult::Reachable => {
                    em!(log, "copy_server_ok", server)
                }
                CheckResult::DnsFailed => {
                    em!(log, "copy_server_dns_fail", server);
                    unreachable.push(server.clone());
                }
                CheckResult::Unreachable => {
                    em!(log, "copy_server_tcp_fail", server);
                    unreachable.push(server.clone());
                }
            }
        }
        if !unreachable.is_empty() {
            em!(log, "copy_net_unavailable", unreachable.join(", "));
            em!(log, "copy_net_warn_continue");
        }
    } else {
        em!(log, "copy_all_local");
    }

    let total = tasks.len() + missing;
    em!(log, "copy_start", total, max_workers);
    emit_progress(log, 0, total);

    let (tx, rx) = mpsc::channel::<(PathBuf, PathBuf)>();
    for t in tasks {
        if tx.send(t).is_err() {
            break;
        }
    }
    drop(tx);

    let rx = Arc::new(Mutex::new(rx));
    let cache = Arc::new(Mutex::new(cache));
    let ok_count = Arc::new(AtomicUsize::new(0));
    let processed = Arc::new(AtomicUsize::new(0));
    let verified_count = Arc::new(AtomicUsize::new(0));
    let mismatches: Arc<Mutex<Vec<(PathBuf, PathBuf)>>> = Arc::new(Mutex::new(Vec::new()));
    let resume_log_path = resume_log.to_path_buf();

    let mut handles = Vec::new();
    for _ in 0..max_workers {
        let stop = stop.clone();
        let rx = Arc::clone(&rx);
        let cache = Arc::clone(&cache);
        let log = log.clone();
        let ok_count = Arc::clone(&ok_count);
        let processed = Arc::clone(&processed);
        let verified_count = Arc::clone(&verified_count);
        let mismatches = Arc::clone(&mismatches);
        let resume_log_path = resume_log_path.clone();
        handles.push(std::thread::spawn(move || {
            loop {
                let item = {
                    let Ok(guard) = rx.lock() else { break };
                    match guard.recv_timeout(Duration::from_millis(200)) {
                        Ok(it) => Some(it),
                        Err(mpsc::RecvTimeoutError::Timeout) => None,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                };
                if stop.is_set() {
                    break;
                }
                let Some((src, dst)) = item else { break };
                let ok = match robust_copy(&src, &dst) {
                    Ok(()) => {
                        // Сверка содержимого: SHA-256 источника и копии.
                        if opts.verify {
                            let a = hash_file(&src, HashAlgo::Sha256);
                            let b = hash_file(&dst, HashAlgo::Sha256);
                            verified_count.fetch_add(1, Ordering::Relaxed);
                            let same = matches!((&a, &b), (Some(x), Some(y)) if x == y);
                            if !same {
                                em!(
                                    &log,
                                    "copy_verify_mismatch",
                                    src.display(),
                                    dst.display()
                                );
                                if let Ok(mut m) = mismatches.lock() {
                                    m.push((src.clone(), dst.clone()));
                                }
                            }
                        }
                        if resume {
                            if let Ok(st) = fs::metadata(&src) {
                                let key = cache_key(&src, &dst);
                                if let Ok(mut c) = cache.lock() {
                                    c.insert(
                                        key,
                                        CacheEntry {
                                            size: st.len(),
                                            mtime: modified_secs(&st),
                                        },
                                    );
                                }
                            }
                        }
                        true
                    }
                    Err(e) => {
                        em!(&log, "copy_file_error", src.display(), dst.display(), e.to_string());
                        false
                    }
                };
                let done = processed.fetch_add(1, Ordering::Relaxed) + 1;
                if ok {
                    ok_count.fetch_add(1, Ordering::Relaxed);
                }
                emit_progress(&log, done, total);
                if done.is_multiple_of(10) || done == total {
                    em!(&log, "copy_progress", done, total);
                }
                if resume && done.is_multiple_of(50) {
                    if let Ok(c) = cache.lock() {
                        save_cache(&resume_log_path, &c, &log);
                    }
                }
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }

    if resume {
        if let Ok(c) = cache.lock() {
            save_cache(resume_log, &c, log);
        }
    }
    let ok = ok_count.load(Ordering::Relaxed);
    emit_progress(log, total, total);
    let verified = verified_count.load(Ordering::Relaxed);
    let mism = mismatches.lock().map(|m| m.clone()).unwrap_or_default();
    if opts.verify {
        em!(
            log,
            "copy_verify_done",
            verified.saturating_sub(mism.len()),
            mism.len()
        );
    }
    em!(log, "copy_done", ok, total);
    CopyReport {
        copied: ok,
        total,
        verified,
        mismatches: mism,
        missing,
    }
}

/// Диск/сетевой ресурс, на который пишем: `C:\`, `\\server\share` или `/`.
fn disk_root(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\") {
        // UNC: \\server\share\...
        let mut parts = rest.split(['\\', '/']);
        if let (Some(server), Some(share)) = (parts.next(), parts.next()) {
            return PathBuf::from(format!(r"\\{server}\{share}"));
        }
    }
    #[cfg(windows)]
    {
        if let Some(pos) = s.find(':') {
            return PathBuf::from(&s[..=pos]);
        }
    }
    #[cfg(unix)]
    {
        return PathBuf::from("/");
    }
    #[allow(unreachable_code)]
    PathBuf::from(".")
}
