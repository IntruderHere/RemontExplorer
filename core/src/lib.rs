//! Ядро RemontExplorer — файловые операции без GUI.
//!
//! Архитектура: `core` — библиотека логики, `gui` — тонкий egui-слой.
//! Отчёты: CSV в UTF-8 с BOM и разделителем `;` (открывается в Excel),
//! колонки фиксированы контрактом (см. `docs`-комментарии функций).

#![allow(clippy::cloned_ref_to_slice_refs)]

pub mod cleanup;
pub mod cli;
pub mod compare;
pub mod config;
pub mod copy;
pub mod create;
pub mod delete;
pub mod disk;
pub mod duplicates;
pub mod error;
pub mod hashing;
pub mod history;
pub mod i18n;
pub mod netcheck;
pub mod paths;
pub mod pathutil;
pub mod recycle;
pub mod report_diff;
pub mod scan;
pub mod stop;
pub mod xlsx;

use std::sync::{Arc, Mutex, OnceLock};

pub use cleanup::cleanup_empty_folders;
pub use compare::{compare_folders, CompareMethod};
pub use config::{read_pairs_csv, read_paths_file, Pair};
pub use copy::copy_files_from_list;
pub use create::{archive_folders, create_folders};
pub use delete::{delete_paths, delete_paths_opts, DeleteMode, DeleteStats};
pub use duplicates::find_duplicates_multi;
pub use error::CoreError;
pub use hashing::HashAlgo;
pub use i18n::{global, set_global, tr, Lang};
pub use scan::{scan_folder, scan_folder_opts, ScanOptions, ScanStats};
pub use history::{HistoryEntry, Outcome};
pub use xlsx::csv_to_xlsx;
pub use stop::StopToken;


/// Формат файлов отчёта.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    /// CSV (UTF-8 с BOM, разделитель `;`) — открывается в Excel.
    #[default]
    Csv,
    /// Книга Excel: CSV конвертируется и удаляется.
    Xlsx,
}

static OUTPUT_FORMAT: OnceLock<Mutex<OutputFormat>> = OnceLock::new();

fn format_cell() -> &'static Mutex<OutputFormat> {
    OUTPUT_FORMAT.get_or_init(|| Mutex::new(OutputFormat::Csv))
}

/// Выбранный пользователем формат отчётов.
pub fn set_output_format(format: OutputFormat) {
    *format_cell().lock().unwrap_or_else(|e| e.into_inner()) = format;
}

/// Текущий формат отчётов (по умолчанию CSV).
pub fn output_format() -> OutputFormat {
    *format_cell().lock().unwrap_or_else(|e| e.into_inner())
}

/// Завершить отчёт: при выбранном XLSX — конвертировать CSV и удалить его.
///
/// Возвращает путь к актуальному файлу. При ошибке конвертации CSV
/// сохраняется — данные не теряются, в журнал попадает ошибка.
pub fn finalize_report(csv: std::path::PathBuf) -> std::path::PathBuf {
    finalize_report_logged(csv, None)
}

/// То же, но с записью ошибки в журнал операции.
pub fn finalize_report_logged(
    csv: std::path::PathBuf,
    log: Option<&LogSink>,
) -> std::path::PathBuf {
    if output_format() == OutputFormat::Csv {
        remember_report(&csv);
        return csv;
    }
    let xlsx = unique_path(csv.with_extension("xlsx"));
    match xlsx::csv_to_xlsx(&csv, &xlsx) {
        Ok(()) => {
            let _ = std::fs::remove_file(&csv);
            remember_report(&xlsx);
            xlsx
        }
        Err(e) => {
            if let Some(l) = log {
                emit(
                    l,
                    i18n::fill(i18n::tr(sink_lang(l), "xlsx_convert_failed"), &[e]),
                );
            }
            csv
        }
    }
}


static OUTPUT_DIR: OnceLock<Mutex<Option<std::path::PathBuf>>> = OnceLock::new();
static LAST_REPORT: OnceLock<Mutex<Option<std::path::PathBuf>>> = OnceLock::new();

fn output_dir_cell() -> &'static Mutex<Option<std::path::PathBuf>> {
    OUTPUT_DIR.get_or_init(|| Mutex::new(None))
}

fn last_report_cell() -> &'static Mutex<Option<std::path::PathBuf>> {
    LAST_REPORT.get_or_init(|| Mutex::new(None))
}

/// Единая папка для результатов всех операций (выбирается в настройках).
/// `None` — писать рядом с приложением (рабочий каталог).
pub fn set_output_dir(dir: Option<std::path::PathBuf>) {
    *output_dir_cell().lock().unwrap_or_else(|e| e.into_inner()) = dir;
}

/// Текущая папка результатов.
pub fn output_dir() -> Option<std::path::PathBuf> {
    output_dir_cell().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Полный путь для файла отчёта: имя кладётся в папку результатов.
/// Каталог создаётся, если его ещё нет.
pub fn resolve_report_path(name: impl AsRef<std::path::Path>) -> std::path::PathBuf {
    let path = match output_dir() {
        Some(dir) => {
            let _ = std::fs::create_dir_all(&dir);
            dir.join(name)
        }
        // Абсолютный путь обязателен: приложение может быть запущено из
        // Проводника с другим рабочим каталогом, и относительное имя файла
        // потом не находится при открытии.
        None => default_report_dir().join(name.as_ref()),
    };
    // Два запуска в одну секунду дают одинаковое имя отчёта — второй
    // затирал первый. Дубли добавляем суффиксом _2, _3…
    unique_path(path)
}

/// Папка отчётов по умолчанию: рядом с приложением, если туда можно писать.
///
/// Приложение могут положить в `Program Files` или другой каталог только для
/// чтения — тогда отчёты уходят в `Документы\RemontExplorer\reports`, чтобы
/// операция не срывалась на последнем шаге.
fn default_report_dir() -> std::path::PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    if is_writable_dir(&cwd) {
        return cwd;
    }
    let fallback = crate::paths::data_dir().join("reports");
    let _ = std::fs::create_dir_all(&fallback);
    fallback
}

/// Можно ли создать файл в каталоге (проверка пробным файлом).
fn is_writable_dir(dir: &std::path::Path) -> bool {
    let probe = dir.join(format!(".remont_probe_{}", std::process::id()));
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Свободное имя рядом с указанным: при совпадении добавляется `_2`, `_3`…
pub fn unique_path(path: std::path::PathBuf) -> std::path::PathBuf {
    if !path.exists() {
        return path;
    }
    let parent = path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned());
    for n in 2..10_000u32 {
        let name = match &ext {
            Some(e) => format!("{stem}_{n}.{e}"),
            None => format!("{stem}_{n}"),
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    path
}

/// Путь последнего созданного отчёта — GUI использует его, чтобы открыть
/// результат после завершения операции.
pub fn last_report() -> Option<std::path::PathBuf> {
    last_report_cell().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Забыть последний отчёт (вызывается перед новой операцией).
pub fn clear_last_report() {
    *last_report_cell().lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn remember_report(path: &std::path::Path) {
    *last_report_cell().lock().unwrap_or_else(|e| e.into_inner()) = Some(path.to_path_buf());
}

/// Сток логов и прогресса операции.
/// `progress` — необязательный колбэк (current, total); total == 0 означает
/// неопределённый прогресс (бар-индикатор без доли).
/// `lang` — язык сообщений, генерируемых в этот сток.
pub struct Sink {
    log: Box<dyn FnMut(String) + Send>,
    progress: Option<Box<dyn FnMut(usize, usize) + Send>>,
    lang: Lang,
}

impl Sink {
    /// Сток только с логами (прогресс игнорируется) — для тестов/CLI.
    pub fn text_only(f: impl FnMut(String) + Send + 'static) -> Self {
        Self {
            log: Box::new(f),
            progress: None,
            lang: Lang::Ru,
        }
    }

    /// Сток с логами и прогрессом.
    pub fn with_progress(
        f: impl FnMut(String) + Send + 'static,
        p: impl FnMut(usize, usize) + Send + 'static,
    ) -> Self {
        Self {
            log: Box::new(f),
            progress: Some(Box::new(p)),
            lang: Lang::Ru,
        }
    }

    /// Язык сообщений для этого стока (GUI задаёт перед запуском операции).
    pub fn with_lang(mut self, lang: Lang) -> Self {
        self.lang = lang;
        self
    }

    pub fn lang(&self) -> Lang {
        self.lang
    }
}

/// Язык стока (без блокировки на время форматирования строки — отдельная
/// блокировка в `emit` всё равно последует).
pub fn sink_lang(log: &LogSink) -> Lang {
    log.lock().ok().map(|s| s.lang).unwrap_or(Lang::Ru)
}

/// Общий тип стока: потокобезопасная обёртка над [`Sink`].
/// Функции core принимают `&LogSink` — как раньше.
pub type LogSink = Arc<Mutex<Sink>>;

/// Отправить строку в сток.
pub fn emit(log: &LogSink, msg: impl Into<String>) {
    if let Ok(mut s) = log.lock() {
        (s.log)(msg.into());
    }
}

/// Отправить прогресс (current, total); total == 0 — неопределённый.
pub fn emit_progress(log: &LogSink, current: usize, total: usize) {
    if let Ok(mut s) = log.lock() {
        if let Some(p) = s.progress.as_mut() {
            p(current, total);
        }
    }
}

/// Сток, пишущий в stdout (логи) и stderr (прогресс) — для тестов и CLI.
pub fn stdout_sink() -> LogSink {
    Arc::new(Mutex::new(Sink::with_progress(
        |msg: String| println!("{msg}"),
        |c: usize, t: usize| eprintln!("[PROGRESS {c}/{t}]"),
    )))
}

/// Блокировка для тестов, меняющих переменные окружения: `std::env` общий на
/// весь процесс, поэтому параллельные тесты перетирают друг другу каталог
/// данных. Все такие тесты обязаны брать этот мьютекс.
#[cfg(test)]
pub fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

pub use pathutil::{long_paths_enabled, set_long_paths};
