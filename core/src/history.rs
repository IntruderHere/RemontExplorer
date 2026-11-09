//! История выполнений: каждая операция оставляет запись в
//! `<Документы>\RemontExplorer\logs\history.jsonl`.
//!
//! Формат — JSON Lines: одна строка = одно событие. Выбран осознанно:
//! - дописывание не переписывает прошлые записи (сбой посреди записи
//!   портит максимум последнюю строку, а не весь журнал);
//! - файл читается потоком, без загрузки целиком;
//! - при необходимости легко открыть в Excel после экспорта в CSV.
//!
//! Запись не должна ломать операцию: ошибки журнала игнорируются вызывающим
//! (журнал — вспомогательный, а не критичный путь).

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths::log_path;

/// Итог операции.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// Завершено успешно.
    #[default]
    Ok,
    /// Остановлено пользователем.
    Aborted,
    /// Завершилось с ошибками (часть элементов не обработана).
    Failed,
}

impl Outcome {
    /// Текст для журнала/таблицы.
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Aborted => "aborted",
            Outcome::Failed => "failed",
        }
    }
}

/// Одна запись истории.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Время начала операции, RFC 3339 (локальное время с офсетом).
    pub when: String,
    /// Машинный код режима: scan, copy, delete, create, compare, cleanup, duplicates.
    pub mode: String,
    /// Человеческое название режима на языке интерфейса.
    pub title: String,
    /// Входные пути (папки/файлы), к которым применялась операция.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Сколько элементов обработано успешно.
    #[serde(default)]
    pub ok: usize,
    /// Сколько ошибок.
    #[serde(default)]
    pub errors: usize,
    /// Объём в байтах (где применимо; 0 — не считался).
    #[serde(default)]
    pub bytes: u64,
    /// Длительность, секунды.
    #[serde(default)]
    pub seconds: f64,
    /// Путь к файлу результата (отчёт, архив).
    #[serde(default)]
    pub report: Option<String>,
    /// Итог операции.
    #[serde(default)]
    pub outcome: Outcome,
    /// Свободное пояснение (например, текст ошибки).
    #[serde(default)]
    pub details: String,
    /// Откуда запущено: `gui`, `schedule`, `context-menu`, `cli`.
    #[serde(default)]
    pub source: String,
}

impl Default for HistoryEntry {
    fn default() -> Self {
        Self {
            when: chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            mode: String::new(),
            title: String::new(),
            paths: Vec::new(),
            ok: 0,
            errors: 0,
            bytes: 0,
            seconds: 0.0,
            report: None,
            outcome: Outcome::Ok,
            details: String::new(),
            source: "gui".into(),
        }
    }
}

impl HistoryEntry {
    /// Новая запись с текущим временем.
    pub fn new(mode: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            mode: mode.into(),
            title: title.into(),
            ..Default::default()
        }
    }

    /// Путь к файлу истории.
    pub fn file() -> PathBuf {
        log_path("history.jsonl")
    }

    /// Дописать запись в историю. Возвращает путь к журналу.
    pub fn record(&self) -> std::io::Result<PathBuf> {
        let path = Self::file();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut line = serde_json::to_string(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        f.write_all(line.as_bytes())?;
        Ok(path)
    }
}

/// Прочитать всю историю (старые записи первыми).
///
/// Битые строки пропускаются: журнал мог быть дописан не до конца при
/// аварийном завершении — из-за этого терять всю историю нельзя.
pub fn read_all() -> Vec<HistoryEntry> {
    read_from(&HistoryEntry::file())
}

/// Прочитать историю из указанного файла.
pub fn read_from(path: &Path) -> Vec<HistoryEntry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<HistoryEntry>(l).ok())
        .collect()
}

/// Удалить историю целиком. Файл остаётся пустым.
pub fn clear() -> std::io::Result<()> {
    let path = HistoryEntry::file();
    if path.exists() {
        std::fs::write(&path, b"")?;
    }
    Ok(())
}

/// Экспорт истории в CSV (UTF-8 с BOM, разделитель `;` — открывается в Excel).
pub fn export_csv(path: &Path) -> std::io::Result<()> {
    let mut wtr = csv::WriterBuilder::new()
        .delimiter(b';')
        .from_path(path)
        .map_err(std::io::Error::other)?;

    wtr.write_record([
        "Дата и время",
        "Режим",
        "Название",
        "Пути",
        "Успешно",
        "Ошибок",
        "Объём, байт",
        "Секунд",
        "Результат",
        "Итог",
        "Источник",
        "Примечание",
    ])
    .map_err(std::io::Error::other)?;

    for e in read_all() {
        wtr.write_record([
            e.when.clone(),
            e.mode.clone(),
            e.title.clone(),
            e.paths.join(" | "),
            e.ok.to_string(),
            e.errors.to_string(),
            e.bytes.to_string(),
            format!("{:.1}", e.seconds),
            e.report.unwrap_or_default(),
            e.outcome.as_str().to_string(),
            e.source.clone(),
            e.details.clone(),
        ])
        .map_err(std::io::Error::other)?;
    }
    wtr.flush().map_err(std::io::Error::other)?;

    // BOM: без него Excel открывает кириллицу «кракозябрами».
    let body = std::fs::read(path)?;
    let mut with_bom = Vec::with_capacity(body.len() + 3);
    with_bom.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    with_bom.extend_from_slice(&body);
    std::fs::write(path, with_bom)?;
    Ok(())
}

/// Сводка по истории — для вкладки «История» и CLI.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    pub total: usize,
    pub ok: usize,
    pub failed: usize,
    pub aborted: usize,
}

/// Посчитать сводку.
pub fn summarize(entries: &[HistoryEntry]) -> Summary {
    let mut s = Summary {
        total: entries.len(),
        ..Default::default()
    };
    for e in entries {
        match e.outcome {
            Outcome::Ok => s.ok += 1,
            Outcome::Failed => s.failed += 1,
            Outcome::Aborted => s.aborted += 1,
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_tmp_history<T>(f: impl FnOnce(&Path) -> T) -> T {
        let _guard = crate::test_env_lock();
        let dir = std::env::temp_dir().join(format!(
            "remont_hist_{}_{}",
            std::process::id(),
            chrono::Local::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var(crate::paths::DATA_DIR_ENV, &dir);
        let r = f(&dir);
        std::env::remove_var(crate::paths::DATA_DIR_ENV);
        let _ = std::fs::remove_dir_all(&dir);
        r
    }

    #[test]
    fn entry_is_written_and_read_back() {
        with_tmp_history(|dir| {
            let mut e = HistoryEntry::new("scan", "Сканирование");
            e.paths = vec![r"D:\ОПС".into()];
            e.ok = 42;
            e.errors = 1;
            e.bytes = 1024;
            e.seconds = 2.5;
            e.report = Some(r"D:\out\files_info.xlsx".into());
            e.outcome = Outcome::Failed;
            e.source = "cli".into();
            let path = e.record().expect("запись");
            assert!(path.starts_with(dir));
            assert_eq!(path.file_name().unwrap(), "history.jsonl");

            let all = read_all();
            assert_eq!(all.len(), 1);
            let g = &all[0];
            assert_eq!(g.mode, "scan");
            assert_eq!(g.title, "Сканирование");
            assert_eq!(g.ok, 42);
            assert_eq!(g.errors, 1);
            assert_eq!(g.outcome, Outcome::Failed);
            assert_eq!(g.source, "cli");
            assert!(g.when.contains('T'), "время не в RFC3339: {}", g.when);
        });
    }

    #[test]
    fn records_append_and_broken_line_is_skipped() {
        with_tmp_history(|_| {
            HistoryEntry::new("copy", "Копирование").record().unwrap();
            HistoryEntry::new("delete", "Удаление").record().unwrap();
            // Имитация оборванной записи (аварийное завершение).
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(HistoryEntry::file())
                .unwrap();
            f.write_all(b"{\"mode\":\"broken\"").unwrap();
            drop(f);

            let all = read_all();
            assert_eq!(all.len(), 2);
            assert_eq!(all[0].mode, "copy");
            assert_eq!(all[1].mode, "delete");
        });
    }

    #[test]
    fn summary_counts_outcomes() {
        let entries = vec![
            HistoryEntry {
                outcome: Outcome::Ok,
                ..Default::default()
            },
            HistoryEntry {
                outcome: Outcome::Failed,
                ..Default::default()
            },
            HistoryEntry {
                outcome: Outcome::Aborted,
                ..Default::default()
            },
            HistoryEntry {
                outcome: Outcome::Ok,
                ..Default::default()
            },
        ];
        let s = summarize(&entries);
        assert_eq!(
            s,
            Summary {
                total: 4,
                ok: 2,
                failed: 1,
                aborted: 1
            }
        );
    }

    #[test]
    fn export_has_bom_and_semicolons() {
        with_tmp_history(|dir| {
            let mut e = HistoryEntry::new("scan", "Сканирование");
            e.paths = vec![r"D:\ОПС 1".into()];
            e.ok = 3;
            e.record().unwrap();

            let out = dir.join("history.csv");
            export_csv(&out).expect("экспорт");
            let bytes = std::fs::read(&out).unwrap();
            assert_eq!(&bytes[..3], &[0xEF, 0xBB, 0xBF], "нет BOM");
            let text = String::from_utf8(bytes[3..].to_vec()).unwrap();
            assert!(text.contains(';'), "нет разделителя ;");
            assert!(text.contains("Сканирование"));
            assert!(text.lines().count() >= 2, "нет строки данных");
        });
    }

    #[test]
    fn clear_empties_file() {
        with_tmp_history(|_| {
            HistoryEntry::new("scan", "Сканирование").record().unwrap();
            assert_eq!(read_all().len(), 1);
            clear().unwrap();
            assert!(read_all().is_empty());
        });
    }
}
