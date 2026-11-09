//! Переводы сообщений приложения: русский (по умолчанию) и английский.
//! Ключи core-логов; строки GUI — в `gui/src/i18n.rs`.
//! В шаблонах `{}` — позиционные плейсхолдеры (заполняются через `fill`).

use std::sync::{Mutex, OnceLock};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    #[default]
    Ru,
    En,
}

/// Глобальный язык — для Display-сообщений ошибок, где нет доступа к сток-логу.
static GLOBAL_LANG: OnceLock<Mutex<Lang>> = OnceLock::new();

pub fn set_global(lang: Lang) {
    *GLOBAL_LANG.get_or_init(|| Mutex::new(Lang::Ru)).lock().unwrap_or_else(|e| e.into_inner()) = lang;
}

pub fn global() -> Lang {
    *GLOBAL_LANG.get_or_init(|| Mutex::new(Lang::Ru)).lock().unwrap_or_else(|e| e.into_inner())
}

/// Перевод по ключу (ключ — строковый литерал). Для неизвестного ключа —
/// заметная пометка, а не тишина.
pub fn tr(lang: Lang, key: &'static str) -> &'static str {
    match lang {
        Lang::Ru => tr_ru(key),
        Lang::En => tr_en(key).unwrap_or(tr_ru(key)),
    }
}

/// Английский перевод ключа; `None`, если перевод отсутствует.
/// Служит проверке покрытия: `tr` при пропуске молча падает в русский.
pub fn en(key: &str) -> Option<&'static str> {
    tr_en(key)
}

/// Подстановка `{}` по порядку (format! требует литерал, шаблон — runtime).
/// Остались лишний `{}` без аргумента — остаются как есть.
pub fn fill(tpl: &str, args: &[String]) -> String {
    let mut out = String::with_capacity(tpl.len() + 32);
    let mut rest = tpl;
    let mut it = args.iter();
    while let Some(pos) = rest.find("{}") {
        out.push_str(&rest[..pos]);
        match it.next() {
            Some(a) => out.push_str(a),
            None => out.push_str("{}"),
        }
        rest = &rest[pos + 2..];
    }
    out.push_str(rest);
    out
}

/// Макрос перевода в лог: `em!(log, "key")` или `em!(log, "key", a, b)`.
#[macro_export]
macro_rules! em {
    ($log:expr, $key:expr) => {
        $crate::emit($log, $crate::tr($crate::sink_lang($log), $key))
    };
    ($log:expr, $key:expr, $($arg:expr),+ $(,)?) => {
        $crate::emit($log, $crate::i18n::fill(
            $crate::tr($crate::sink_lang($log), $key),
            &[$($arg.to_string()),+],
        ))
    };
}

fn tr_ru(key: &'static str) -> &'static str {
    match key {
        "cleanup_unreadable" => "Внимание: не удалось прочитать {} пап(ок) (отсутствуют права или путь недоступен).",
        "cleanup_none_found" => "Пустых папок в {} не найдено.",
        "cleanup_found" => "\nНайдено пустых папок: {}",
        "cleanup_aborted" => "Очистка прервана",
        "cleanup_deleted" => "Удалена пустая папка: {}",
        "delete_error" => "Ошибка удаления {}: {}",
        "cmp_config_missing" => "Файл конфигурации не найден: {}",
        "cmp_no_valid_pairs" => "Не найдено ни одной валидной пары папок. Формат: C:\\Папка1; D:\\Папка2  или две строки с путями",
        "cmp_aborted" => "Сравнение прервано",
        "cmp_a_missing" => "Папка A не существует: {}",
        "cmp_b_missing" => "Папка B не существует: {}",
        "cmp_header" => "\n=== Сравнение {} <--> {} ===",
        "cmp_scanning" => "Сканирование папок (параллельно, сбор по именам файлов)...",
        "cmp_scanning2" => "Сканирование папок (параллельно, сбор по именам файлов)...",
        "cmp_unique_names" => "Всего уникальных имён файлов: {}",
        "cmp_deep_tasks" => "Файлов, требующих глубокого сравнения: {}",
        "report_saved" => "Отчёт сохранён: {}",
        "cmp_stats" => "Статистика: всего записей={}, отличаются={}",
        "report_write_error" => "Ошибка записи отчёта: {}",
        "xlsx_convert_failed" => "Не удалось сохранить отчёт в XLSX ({}). Отчёт оставлен в CSV.",
        "copy_cache_save_warn" => "Внимание: не удалось сохранить кэш копирования {}: {}",
        "copy_empty_name" => "пустое имя файла",
        "copy_no_attempt" => "копирование: не было ни одной попытки",
        "copy_resume_corrupt" => "Внимание: файл возобновления повреждён или не читается — копирование начнётся заново.",
        "copy_config_missing" => "Конфигурационный файл не найден или пуст: {}",
        "copy_source_missing" => "Источник не существует: {}",
        "copy_skipped" => "Пропущено уже скопированных файлов (из кэша): {}",
        "copy_no_tasks" => "Нет новых задач копирования.",
        "copy_netcheck_start" => "Проверка доступности сетевых ресурсов (DNS + TCP/SMB порт 445)...",
        "copy_server_ok" => "  \\{} - доступен (DNS + TCP)",
        "copy_server_dns_fail" => "Сетевой ресурс \\{} недоступен (DNS resolution failed)",
        "copy_server_tcp_fail" => "Сетевой ресурс \\{} недоступен (TCP/SMB check failed)",
        "copy_net_unavailable" => "\nВНИМАНИЕ: следующие ресурсы недоступны: {}",
        "copy_net_warn_continue" => "Копирование продолжится, но может завершиться ошибками.\n",
        "copy_all_local" => "Все источники — локальные пути, проверка сети не требуется.",
        "copy_start" => "Начинаю копирование {} файлов (потоков: {})...",
        "copy_file_error" => "Ошибка копирования {} -> {}: {}",
        "copy_progress" => "[PROGRESS] Скопировано {} из {}...",
        "copy_done" => "Готово. Успешно скопировано: {} из {}",
        "create_aborted" => "Операция создания прервана пользователем",
        "create_exists" => "Путь уже существует: {}",
        "create_created" => "Создана папка: {}",
        "create_error" => "Ошибка создания {}: {}",
        "zip_start" => "Начинаю упаковку структуры в архив: {}...",
        "zip_root" => "Общий корень архива: {}",
        "zip_create_error" => "ОШИБКА: не удалось создать архив {}: {}. Оригиналы сохранены.",
        "zip_aborted" => "Архивация прервана",
        "err_originals_preserved" => "ОШИБКА: {}. Оригиналы сохранены.",
        "zip_read_error" => "ОШИБКА: не удалось прочитать файл {}: (нет прав или файл занят). Оригиналы сохранены.",
        "zip_write_fail" => "ОШИБКА: сбой записи в архив (нет места на диске?). Оригиналы сохранены.",
        "zip_finish_fail" => "ОШИБКА: не удалось завершить архив (нет места на диске?). Оригиналы сохранены.",
        "zip_verify" => "Проверка целостности архива...",
        "zip_verified" => "Архив успешно проверен.",
        "zip_delete_start" => "Удаление исходных папок...",
        "zip_deleted" => "Удалена папка: {}",
        "zip_delete_fail" => "Не удалось удалить {}: {}",
        "zip_extra_cleaned" => "Дополнительно очищено пустых родительских папок: {}",
        "zip_originals_deleted" => "Оригинальные папки удалены.",
        "delete_aborted" => "Операция удаления прервана пользователем.",
        "delete_archive_start" => "Создание архива перед удалением: {}",
        "delete_archive_ok" => "Архив создан: {} (файлов: {}). Удаление...",
        "delete_archive_failed" => "Не удалось создать архив: {} — удаление отменено (данные целы).",
        "delete_recycle_start" => "Перемещение в корзину: {} путей...",
        "delete_recycle_ok" => "В корзину отправлено: {}",
        "delete_recycle_failed" => "Не удалось переместить в корзину: {} — удаление отменено.",
        "delete_recycle_unsupported" => "Корзина доступна только в Windows.",
        "copy_space_ok" => "Место на {}: нужно {}, свободно {}",
        "copy_space_fail" => "Недостаточно места на {}: нужно {}, свободно {}. Копирование не начато.",
        "copy_verify_mismatch" => "Расхождение содержимого: {} → {}",
        "copy_verify_done" => "Сверено по SHA-256: {}, расхождений: {}",
        "dup_header" => "=== Пакетный поиск дубликатов ===",
        "dup_folders" => "Папки для анализа: {}",
        "dup_collect" => "Сбор информации о файлах со всех папок...",
        "dup_skip_nondir" => "Пропущена не-директория: {}",
        "dup_scanning" => "  Сканирование: {}",
        "dup_no_files" => "Нет файлов для анализа.",
        "dup_found_files" => "Найдено файлов: {}. Группировка по размеру...",
        "dup_potential" => "Потенциальных дубликатов (по размеру): {} файлов в {} размерных группах.",
        "dup_none" => "Дубликатов не найдено (нет файлов с одинаковым размером).",
        "dup_hashing" => "Вычисление {}-хешей...",
        "dup_hash_fail" => "Не удалось вычислить хеш для {}",
        "dup_processed" => "  Обработано {}/{}",
        "dup_groups" => "Найдено групп дубликатов: {}",
        "dup_group" => "Группа {}: {} файлов, размер {} bytes",
        "err_not_ported" => "операция '{}' недоступна в этой версии",
        "err_invalid_path" => "некорректный путь: {}",
        "err_io" => "I/O ошибка: {}",
        "parent_deleted" => "Удалена пустая родительская папка: {}",
        "scan_not_found" => "Папка не найдена или недоступна: {} ({})",
        "scan_not_dir" => "Не является директорией: {}",
        "scan_report_create_fail" => "ОШИБКА: не удалось создать файл отчёта {} (нет прав на запись или нет места): {}",
        "scan_start" => "[START] Сканирование: {}",
        "scan_progress" => "[PROGRESS] Просканировано файлов: {}...",
        "scan_report_finish_fail" => "ОШИБКА: не удалось завершить запись отчёта {} (нет места на диске?): {}",
        "scan_meta_warn" => "Внимание: не удалось прочитать метаданные {} файл(ов) (отсутствуют права) — они записаны в отчёт с размером 0.",
        "scan_stop" => "\n[STOP] Сканирование остановлено пользователем. Частичный отчёт (файлов: {}): {}",
        "scan_done" => "\n[DONE] Готово. Файлов: {}. Отчет: {}",
        other => other,
    }
}

fn tr_en(key: &str) -> Option<&'static str> {
    match key {
        "cleanup_unreadable" => Some("Warning: could not read {} folder(s) (missing permissions or path unavailable)."),
        "cleanup_none_found" => Some("No empty folders found in {}."),
        "cleanup_found" => Some("\nFound empty folders: {}"),
        "cleanup_aborted" => Some("Cleanup aborted"),
        "cleanup_deleted" => Some("Deleted empty folder: {}"),
        "delete_error" => Some("Delete error {}: {}"),
        "cmp_config_missing" => Some("Configuration file not found: {}"),
        "cmp_no_valid_pairs" => Some("No valid folder pairs found. Format: C:\\Folder1; D:\\Folder2 or two lines with paths"),
        "cmp_aborted" => Some("Comparison aborted"),
        "cmp_a_missing" => Some("Folder A does not exist: {}"),
        "cmp_b_missing" => Some("Folder B does not exist: {}"),
        "cmp_header" => Some("\n=== Compare {} <--> {} ==="),
        "cmp_scanning" => Some("Scanning folders (in parallel, collecting by file names)..."),
        "cmp_scanning2" => Some("Scanning folders (in parallel, collecting by file names)..."),
        "cmp_unique_names" => Some("Total unique file names: {}"),
        "cmp_deep_tasks" => Some("Files requiring deep comparison: {}"),
        "report_saved" => Some("Report saved: {}"),
        "cmp_stats" => Some("Statistics: total records={}, differ={}"),
        "report_write_error" => Some("Report write error: {}"),
        "xlsx_convert_failed" => Some("Failed to save the report as XLSX ({}). The report is kept in CSV."),
        "copy_cache_save_warn" => Some("Warning: could not save copy cache {}: {}"),
        "copy_empty_name" => Some("empty file name"),
        "copy_no_attempt" => Some("copy: no attempt was made"),
        "copy_resume_corrupt" => Some("Warning: resume file is corrupted or unreadable — copying will start over."),
        "copy_config_missing" => Some("Configuration file not found or empty: {}"),
        "copy_source_missing" => Some("Source does not exist: {}"),
        "copy_skipped" => Some("Skipped already-copied files (from cache): {}"),
        "copy_no_tasks" => Some("No new copy tasks."),
        "copy_netcheck_start" => Some("Checking availability of network resources (DNS + TCP/SMB port 445)..."),
        "copy_server_ok" => Some("  \\{} - available (DNS + TCP)"),
        "copy_server_dns_fail" => Some("Network resource \\{} unavailable (DNS resolution failed)"),
        "copy_server_tcp_fail" => Some("Network resource \\{} unavailable (TCP/SMB check failed)"),
        "copy_net_unavailable" => Some("\nWARNING: the following resources are unavailable: {}"),
        "copy_net_warn_continue" => Some("Copying will continue, but may finish with errors.\n"),
        "copy_all_local" => Some("All sources are local paths, no network check required."),
        "copy_start" => Some("Starting copy of {} files (threads: {})..."),
        "copy_file_error" => Some("Copy error {} -> {}: {}"),
        "copy_progress" => Some("[PROGRESS] Copied {} of {}..."),
        "copy_done" => Some("Done. Successfully copied: {} of {}"),
        "create_aborted" => Some("Create operation aborted by user"),
        "create_exists" => Some("Path already exists: {}"),
        "create_created" => Some("Folder created: {}"),
        "create_error" => Some("Create error {}: {}"),
        "zip_start" => Some("Starting structure packing into archive: {}..."),
        "zip_root" => Some("Common archive root: {}"),
        "zip_create_error" => Some("ERROR: failed to create archive {}: {}. Originals preserved."),
        "zip_aborted" => Some("Archiving aborted"),
        "err_originals_preserved" => Some("ERROR: {}. Originals preserved."),
        "zip_read_error" => Some("ERROR: failed to read file {}: (no permissions or file in use). Originals preserved."),
        "zip_write_fail" => Some("ERROR: archive write failure (out of disk space?). Originals preserved."),
        "zip_finish_fail" => Some("ERROR: failed to finish the archive (out of disk space?). Originals preserved."),
        "zip_verify" => Some("Verifying archive integrity..."),
        "zip_verified" => Some("Archive verified successfully."),
        "zip_delete_start" => Some("Deleting source folders..."),
        "zip_deleted" => Some("Deleted folder: {}"),
        "zip_delete_fail" => Some("Failed to delete {}: {}"),
        "zip_extra_cleaned" => Some("Additionally cleaned up empty parent folders: {}"),
        "zip_originals_deleted" => Some("Original folders deleted."),
        "delete_aborted" => Some("Delete operation aborted by user."),
        "delete_archive_start" => Some("Creating archive before deletion: {}"),
        "delete_archive_ok" => Some("Archive created: {} (files: {}). Deleting..."),
        "delete_archive_failed" => Some("Could not create archive: {} — deletion cancelled (data intact)."),
        "delete_recycle_start" => Some("Moving to Recycle Bin: {} paths..."),
        "delete_recycle_ok" => Some("Moved to Recycle Bin: {}"),
        "delete_recycle_failed" => Some("Could not move to Recycle Bin: {} — deletion cancelled."),
        "delete_recycle_unsupported" => Some("Recycle Bin is available on Windows only."),
        "copy_space_ok" => Some("Space on {}: need {}, free {}"),
        "copy_space_fail" => Some("Not enough space on {}: need {}, free {}. Copy not started."),
        "copy_verify_mismatch" => Some("Content mismatch: {} → {}"),
        "copy_verify_done" => Some("Verified with SHA-256: {}, mismatches: {}"),
        "dup_header" => Some("=== Batch duplicate search ==="),
        "dup_folders" => Some("Folders for analysis: {}"),
        "dup_collect" => Some("Collecting file information from all folders..."),
        "dup_skip_nondir" => Some("Skipped non-directory: {}"),
        "dup_scanning" => Some("  Scanning: {}"),
        "dup_no_files" => Some("No files to analyze."),
        "dup_found_files" => Some("Files found: {}. Grouping by size..."),
        "dup_potential" => Some("Potential duplicates (by size): {} files in {} size groups."),
        "dup_none" => Some("No duplicates found (no files with the same size)."),
        "dup_hashing" => Some("Computing {} hashes..."),
        "dup_hash_fail" => Some("Failed to compute hash for {}"),
        "dup_processed" => Some("  Processed {}/{}"),
        "dup_groups" => Some("Duplicate groups found: {}"),
        "dup_group" => Some("Group {}: {} files, size {} bytes"),
        "err_not_ported" => Some("operation '{}' is not available in this version"),
        "err_invalid_path" => Some("invalid path: {}"),
        "err_io" => Some("I/O error: {}"),
        "parent_deleted" => Some("Deleted empty parent folder: {}"),
        "scan_not_found" => Some("Folder not found or inaccessible: {} ({})"),
        "scan_not_dir" => Some("Not a directory: {}"),
        "scan_report_create_fail" => Some("ERROR: failed to create report file {} (no write permission or out of space): {}"),
        "scan_start" => Some("[START] Scanning: {}"),
        "scan_progress" => Some("[PROGRESS] Files scanned: {}..."),
        "scan_report_finish_fail" => Some("ERROR: failed to finish writing report {} (out of disk space?): {}"),
        "scan_meta_warn" => Some("Warning: could not read metadata for {} file(s) (no permissions) — they are written to the report with size 0."),
        "scan_stop" => Some("\n[STOP] Scanning stopped by user. Partial report (files: {}): {}"),
        "scan_done" => Some("\n[DONE] Done. Files: {}. Report: {}"),
        _ => None,
    }
}
