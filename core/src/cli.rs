//! Консольный режим: запуск операций без окна.
//!
//! Предназначен для bat-файлов и штатного Планировщика Windows:
//!
//! ```text
//! remont_explorer.exe --cli --scan "D:\ОПС" --out "D:\Отчёты" --format xlsx
//! remont_explorer.exe --cli --copy D:\config\from_to.csv --verify
//! remont_explorer.exe --cli --delete D:\config\folders_to_delete.txt --recycle
//! remont_explorer.exe --cli --diff old.csv new.csv
//! remont_explorer.exe --cli --history
//! ```
//!
//! Коды возврата: 0 — успех, 1 — операция завершилась с ошибками,
//! 2 — неверные аргументы.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Вывод в stdout без паники: при перенаправлении в `head`/файл канал может
/// закрыться раньше — это не повод падать с «Broken pipe».
macro_rules! outln {
    ($($arg:tt)*) => {{
        let _ = writeln!(std::io::stdout(), $($arg)*);
    }};
}

macro_rules! errln {
    ($($arg:tt)*) => {{
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

use crate::delete::DeleteMode;
use crate::history::{HistoryEntry, Outcome};
use crate::{OutputFormat, Sink, StopToken};

/// Что делать.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Scan {
        dir: PathBuf,
        out: Option<PathBuf>,
    },
    Compare {
        config: PathBuf,
    },
    Duplicates {
        config: PathBuf,
    },
    Delete {
        config: PathBuf,
    },
    Copy {
        config: PathBuf,
    },
    Cleanup {
        dir: PathBuf,
    },
    Create {
        config: PathBuf,
        zip: bool,
    },
    DiffReports {
        old: PathBuf,
        new: PathBuf,
    },
    History {
        export: Option<PathBuf>,
        clear: bool,
    },
    Help,
}

/// Разобранные аргументы.
#[derive(Debug, Clone)]
pub struct CliOptions {
    pub action: Action,
    /// Единая папка результатов (для любого режима).
    pub out_dir: Option<PathBuf>,
    pub format: Option<OutputFormat>,
    pub quiet: bool,
    pub threads: Option<usize>,
    pub delete_mode: DeleteMode,
    pub verify: bool,
    pub check_space: bool,
}

/// Текст справки.
pub const HELP: &str = "\
RemontExplorer — файловые операции из командной строки

  --cli                        включить консольный режим (без окна)

Режимы (выберите один):
  --scan <папка>               сканировать папку в отчёт
  --compare <файл>             сравнить пары папок из конфига
  --duplicates <файл>          найти дубликаты по списку папок
  --delete <файл>              удалить пути из списка
  --copy <файл>                скопировать файлы по списку from;to
  --cleanup <папка>            удалить пустые папки
  --create <файл>              создать папки из списка (--zip — упаковать в архив)
  --diff <старый> <новый>      сравнить два отчёта сканирования
  --history                    показать историю выполнений

Параметры:
  --out <папка>                папка результатов (по умолчанию — рядом с приложением)
  --format csv|xlsx            формат отчёта
  --threads <N>                число потоков сканирования
  --recycle                    удалять в корзину
  --archive                    удалять через ZIP-архив
  --verify                     сверять копии по SHA-256
  --no-space-check             не проверять свободное место
  --export <файл>              выгрузить историю в CSV
  --clear                      очистить историю
  --quiet                      только итог, без подробного журнала
  -h, --help                   эта справка
";

fn need_value(args: &[String], i: &mut usize, flag: &str) -> Result<String, String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| format!("после {flag} нужно значение"))
}

/// Разобрать аргументы. `Ok(None)` — консольный режим не запрашивался.
pub fn parse(args: &[String]) -> Result<Option<CliOptions>, String> {
    let mut cli = false;
    let mut action: Option<Action> = None;
    let mut format = None;
    let mut quiet = false;
    let mut threads = None;
    let mut delete_mode = DeleteMode::Permanent;
    let mut verify = false;
    let mut check_space = true;
    let mut out: Option<PathBuf> = None;
    let mut zip = false;
    let mut export: Option<PathBuf> = None;
    let mut clear = false;
    // Отложенные позиционные аргументы для --diff.
    let mut pending_diff_old: Option<PathBuf> = None;

    let mut i = 0usize;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "--cli" => cli = true,
            "-h" | "--help" => action = Some(Action::Help),
            "--quiet" => quiet = true,
            "--verify" => verify = true,
            "--no-space-check" => check_space = false,
            "--recycle" => delete_mode = DeleteMode::Recycle,
            "--archive" => delete_mode = DeleteMode::Archive,
            "--zip" => zip = true,
            "--clear" => clear = true,
            "--out" => out = Some(PathBuf::from(need_value(args, &mut i, "--out")?)),
            "--export" => export = Some(PathBuf::from(need_value(args, &mut i, "--export")?)),
            "--format" => {
                let v = need_value(args, &mut i, "--format")?;
                format = Some(match v.to_lowercase().as_str() {
                    "csv" => OutputFormat::Csv,
                    "xlsx" => OutputFormat::Xlsx,
                    other => return Err(format!("неизвестный формат: {other} (csv|xlsx)")),
                });
            }
            "--threads" => {
                let v = need_value(args, &mut i, "--threads")?;
                threads = Some(v.parse().map_err(|_| format!("не число: {v}"))?);
            }
            "--scan" => {
                let dir = PathBuf::from(need_value(args, &mut i, "--scan")?);
                action = Some(Action::Scan { dir, out: None });
            }
            "--compare" => {
                let c = PathBuf::from(need_value(args, &mut i, "--compare")?);
                action = Some(Action::Compare { config: c });
            }
            "--duplicates" => {
                let c = PathBuf::from(need_value(args, &mut i, "--duplicates")?);
                action = Some(Action::Duplicates { config: c });
            }
            "--delete" => {
                let c = PathBuf::from(need_value(args, &mut i, "--delete")?);
                action = Some(Action::Delete { config: c });
            }
            "--copy" => {
                let c = PathBuf::from(need_value(args, &mut i, "--copy")?);
                action = Some(Action::Copy { config: c });
            }
            "--cleanup" => {
                let d = PathBuf::from(need_value(args, &mut i, "--cleanup")?);
                action = Some(Action::Cleanup { dir: d });
            }
            "--create" => {
                let c = PathBuf::from(need_value(args, &mut i, "--create")?);
                action = Some(Action::Create { config: c, zip });
            }
            "--diff" => {
                let old = PathBuf::from(need_value(args, &mut i, "--diff")?);
                let new = PathBuf::from(need_value(args, &mut i, "--diff")?);
                action = Some(Action::DiffReports { old, new });
            }
            "--history" => action = Some(Action::History {
                export: export.clone(),
                clear,
            }),
            other if other.starts_with('-') => {
                return Err(format!("неизвестный параметр: {other}"));
            }
            other => {
                // Позиционный аргумент: второй файл для --diff.
                if pending_diff_old.is_none() {
                    pending_diff_old = Some(PathBuf::from(other));
                }
            }
        }
        i += 1;
    }

    // --diff old new: второй путь мог прийти позиционно.
    if let (Some(Action::DiffReports { old, new }), Some(second)) = (&action, pending_diff_old) {
        if new.as_os_str().is_empty() {
            action = Some(Action::DiffReports {
                old: old.clone(),
                new: second,
            });
        }
    }

    // Без --cli консольный режим не включается (кроме справки): так
    // `--scan "D:\Папка"` из контекстного меню по-прежнему открывает окно.
    if !cli && !matches!(action, Some(Action::Help)) {
        return Ok(None);
    }
    let Some(mut action) = action else {
        return Err("не указан режим (--scan, --copy, --history, ...)".into());
    };
    if let Action::Scan { dir, .. } = &action {
        action = Action::Scan {
            dir: dir.clone(),
            out: out.clone(),
        };
    }
    if let Action::History { .. } = &action {
        action = Action::History { export, clear };
    }

    Ok(Some(CliOptions {
        action,
        out_dir: out,
        format,
        quiet,
        threads,
        delete_mode,
        verify,
        check_space,
    }))
}

/// Сток вывода: при `quiet` в stdout попадает только итоговая строка.
pub fn sink(quiet: bool) -> crate::LogSink {
    Arc::new(Mutex::new(Sink::text_only(move |msg: String| {
        if !quiet {
            outln!("{msg}");
        }
    })))
}

/// Выполнить действие. Возвращает код возврата процесса.
pub fn run(opts: &CliOptions) -> i32 {
    let log = sink(opts.quiet);
    let stop = StopToken::new();
    let started = std::time::Instant::now();

    if let Some(f) = opts.format {
        crate::set_output_format(f);
    }
    // Папка результатов действует для любого режима: отчёты сканирования,
    // сравнения, архивы удаления — всё в одно место.
    if let Action::Scan { out: Some(dir), .. } = &opts.action {
        crate::set_output_dir(Some(dir.clone()));
    }

    if let Some(dir) = &opts.out_dir {
        crate::set_output_dir(Some(dir.clone()));
    }

    let mut entry = HistoryEntry::new("", "");
    entry.source = "cli".into();
    entry.paths = match &opts.action {
        Action::Scan { dir, .. } => vec![dir.display().to_string()],
        Action::Cleanup { dir } => vec![dir.display().to_string()],
        Action::Compare { config }
        | Action::Duplicates { config }
        | Action::Delete { config }
        | Action::Copy { config }
        | Action::Create { config, .. } => vec![config.display().to_string()],
        Action::DiffReports { old, new } => {
            vec![old.display().to_string(), new.display().to_string()]
        }
        Action::History { .. } | Action::Help => Vec::new(),
    };

    let mut code = 0i32;
    match &opts.action {
        Action::Help => {
            outln!("{HELP}");
            return 0;
        }
        Action::History { export, clear } => {
            if let Some(path) = export {
                match crate::history::export_csv(path) {
                    Ok(()) => outln!("История выгружена: {}", path.display()),
                    Err(e) => {
                        errln!("Ошибка экспорта: {e}");
                        code = 1;
                    }
                }
            }
            if *clear {
                if let Err(e) = crate::history::clear() {
                    errln!("Ошибка очистки: {e}");
                    code = 1;
                }
            }
            let all = crate::history::read_all();
            let s = crate::history::summarize(&all);
            outln!(
                "Записей: {} (успешно {}, с ошибками {}, прервано {})",
                s.total, s.ok, s.failed, s.aborted
            );
            for e in all.iter().rev().take(20) {
                outln!(
                    "{}  {:<12} {:<14} ok={} err={} {}",
                    e.when,
                    e.mode,
                    e.title,
                    e.ok,
                    e.errors,
                    e.report.clone().unwrap_or_default()
                );
            }
            return code;
        }
        Action::Scan { dir, .. } => {
            entry.mode = "scan".into();
            entry.title = "Сканирование".into();
            let threads = opts.threads.unwrap_or(16);
            match crate::scan_folder(dir, None, threads, &stop, &log) {
                Some((path, stats)) => {
                    entry.ok = stats.files;
                    entry.errors = stats.meta_errors;
                    entry.bytes = (stats.size_mb * 1024.0 * 1024.0) as u64;
                    entry.report = Some(path.display().to_string());
                    if opts.quiet {
                        outln!("{}", path.display());
                    }
                }
                None => {
                    entry.outcome = Outcome::Failed;
                    code = 1;
                }
            }
        }
        Action::Compare { config } => {
            entry.mode = "compare".into();
            entry.title = "Сравнение".into();
            match crate::compare_folders(config, crate::CompareMethod::SizeDate, None, &stop, &log) {
                Ok(reports) => {
                    entry.ok = reports.len();
                    if let Some(p) = reports.first() {
                        entry.report = Some(p.display().to_string());
                    }
                }
                Err(e) => {
                    errln!("{e}");
                    entry.outcome = Outcome::Failed;
                    entry.details = e.to_string();
                    code = 1;
                }
            }
        }
        Action::Duplicates { config } => {
            entry.mode = "duplicates".into();
            entry.title = "Дубликаты".into();
            let paths = crate::read_paths_file(config);
            match crate::find_duplicates_multi(&paths, crate::HashAlgo::Sha256, None, &stop, &log) {
                Ok(groups) => entry.ok = groups.len(),
                Err(e) => {
                    errln!("{e}");
                    entry.outcome = Outcome::Failed;
                    code = 1;
                }
            }
        }
        Action::Delete { config } => {
            entry.mode = "delete".into();
            entry.title = "Удаление".into();
            let paths = crate::read_paths_file(config);
            let stats =
                crate::delete::delete_paths_opts(&paths, true, opts.delete_mode, &stop, &log);
            entry.ok = stats.files + stats.folders;
            entry.errors = stats.errors;
            entry.bytes = stats.size_bytes;
            if let Some(a) = stats.archive {
                entry.report = Some(a.display().to_string());
            }
            if stats.errors > 0 {
                entry.outcome = Outcome::Failed;
                code = 1;
            }
        }
        Action::Copy { config } => {
            entry.mode = "copy".into();
            entry.title = "Копирование".into();
            let resume = crate::paths::config_path("copy_resume.json");
            let report = crate::copy::copy_files_from_list_opts(
                config,
                &resume,
                crate::copy::DEFAULT_WORKERS,
                true,
                crate::copy::CopyOptions {
                    verify: opts.verify,
                    check_space: opts.check_space,
                },
                &stop,
                &log,
            );
            entry.ok = report.copied;
            entry.errors = report.total.saturating_sub(report.copied)
                + report.mismatches.len()
                + usize::from(report.config_error);
            if !report.is_clean() {
                entry.outcome = Outcome::Failed;
                code = 1;
            }
        }
        Action::Cleanup { dir } => {
            entry.mode = "cleanup".into();
            entry.title = "Очистка".into();
            entry.ok = crate::cleanup_empty_folders(dir, &stop, &log);
        }
        Action::Create { config, zip } => {
            entry.mode = "create".into();
            entry.title = "Создание".into();
            let paths = crate::read_paths_file(config);
            if *zip {
                match crate::archive_folders(&paths, false, &stop, &log) {
                    Some(p) => {
                        entry.report = Some(p.display().to_string());
                        entry.ok = paths.len();
                    }
                    None => {
                        entry.outcome = Outcome::Failed;
                        code = 1;
                    }
                }
            } else {
                entry.ok = crate::create_folders(&paths, &stop, &log).len();
            }
        }
        Action::DiffReports { old, new } => {
            entry.mode = "diff".into();
            entry.title = "Сравнение отчётов".into();
            match crate::report_diff::diff_reports(old, new) {
                Ok((rows, stats)) => {
                    let name = format!(
                        "diff_{}.csv",
                        chrono::Local::now().format("%Y%m%d_%H%M%S")
                    );
                    let path = crate::resolve_report_path(name);
                    match crate::report_diff::save_diff_report(&rows, &stats, path) {
                        Ok(p) => {
                            entry.report = Some(p.display().to_string());
                            entry.ok = rows.len();
                            outln!(
                                "Добавлено: {}, удалено: {}, изменено: {}, без изменений: {}",
                                stats.added, stats.removed, stats.changed, stats.same
                            );
                            if opts.quiet {
                                outln!("{}", p.display());
                            }
                        }
                        Err(e) => {
                            errln!("{e}");
                            entry.outcome = Outcome::Failed;
                            code = 1;
                        }
                    }
                }
                Err(e) => {
                    errln!("{e}");
                    entry.outcome = Outcome::Failed;
                    entry.details = e.to_string();
                    code = 1;
                }
            }
        }
    }

    entry.seconds = started.elapsed().as_secs_f64();
    if entry.mode.is_empty() {
        return code;
    }
    if !matches!(entry.outcome, Outcome::Failed) && code != 0 {
        entry.outcome = Outcome::Failed;
    }
    let _ = entry.record();
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn no_cli_flag_means_gui() {
        assert!(parse(&args(&["--scan", "D:\\Папка"])).unwrap().is_none());
    }

    #[test]
    fn parses_scan_with_options() {
        let r = parse(&args(&[
            "--cli", "--scan", "D:\\ОПС", "--out", "D:\\Отчёты", "--format", "xlsx", "--threads",
            "8", "--quiet",
        ]))
        .unwrap()
        .expect("режим не разобран");
        assert_eq!(
            r.action,
            Action::Scan {
                dir: PathBuf::from("D:\\ОПС"),
                out: Some(PathBuf::from("D:\\Отчёты")),
            }
        );
        assert_eq!(r.format, Some(OutputFormat::Xlsx));
        assert_eq!(r.threads, Some(8));
        assert!(r.quiet);
    }

    #[test]
    fn parses_delete_modes() {
        let r = parse(&args(&["--cli", "--delete", "list.txt", "--recycle"]))
            .unwrap()
            .unwrap();
        assert_eq!(r.delete_mode, DeleteMode::Recycle);

        let r = parse(&args(&["--cli", "--delete", "list.txt", "--archive"]))
            .unwrap()
            .unwrap();
        assert_eq!(r.delete_mode, DeleteMode::Archive);
    }

    #[test]
    fn parses_diff_two_files() {
        let r = parse(&args(&["--cli", "--diff", "old.csv", "new.xlsx"]))
            .unwrap()
            .unwrap();
        assert_eq!(
            r.action,
            Action::DiffReports {
                old: PathBuf::from("old.csv"),
                new: PathBuf::from("new.xlsx"),
            }
        );
    }

    #[test]
    fn copy_flags() {
        let r = parse(&args(&["--cli", "--copy", "pairs.csv", "--verify"]))
            .unwrap()
            .unwrap();
        assert!(r.verify);
        assert!(r.check_space);

        let r = parse(&args(&["--cli", "--copy", "pairs.csv", "--no-space-check"]))
            .unwrap()
            .unwrap();
        assert!(!r.check_space);
    }

    #[test]
    fn rejects_unknown_flag_and_bad_format() {
        assert!(parse(&args(&["--cli", "--nope"])).is_err());
        assert!(parse(&args(&["--cli", "--scan", "x", "--format", "pdf"])).is_err());
        assert!(parse(&args(&["--cli", "--scan"])).is_err());
    }

    #[test]
    fn cli_without_action_is_error() {
        assert!(parse(&args(&["--cli"])).is_err());
    }

    #[test]
    fn help_works_without_cli_flag() {
        let r = parse(&args(&["--help"])).unwrap().unwrap();
        assert_eq!(r.action, Action::Help);
    }
}
