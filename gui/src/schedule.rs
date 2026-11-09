//! Планировщик заданий: «функция + время суток + число выполнений».
//! Хранится в `config/schedule.json` в каталоге данных приложения
//! (в «Документах» пользователя).

use serde::{Deserialize, Serialize};
use std::fs;

/// Полный путь к файлу расписания (каталог данных приложения).
fn file() -> std::path::PathBuf {
    crate::config_path("schedule.json")
}

/// Одно задание расписания.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Task {
    /// Устойчивый идентификатор (для журнала и порядка).
    pub id: u64,
    /// Ключ режима: delete/create/scan/compare/cleanup/copy/duplicates.
    pub mode: String,
    /// Задание включено.
    pub enabled: bool,
    /// Час запуска (0..=23), местное время.
    pub hour: u8,
    /// Минута запуска (0..=59).
    pub minute: u8,
    /// Сколько выполнений осталось.
    pub remaining: u32,
    /// Сколько выполнений задано изначально.
    pub total: u32,
    /// Папка для сохранения отчётов (пусто — папка приложения).
    pub output_dir: String,
    /// Дата последнего запуска (ГГГГ-ММ-ДД) — защита от повтора в тот же день.
    pub last_date: String,
    /// Отметка времени последнего запуска (для журнала и UI).
    pub last_run: String,
}

impl Task {
    pub fn new(id: u64, mode: &str, hour: u8, minute: u8, total: u32) -> Self {
        Self {
            id,
            mode: mode.to_string(),
            enabled: true,
            hour,
            minute,
            remaining: total,
            total,
            output_dir: String::new(),
            last_date: String::new(),
            last_run: String::new(),
        }
    }

    /// Время запуска в минутах от начала суток.
    pub fn minutes(&self) -> u32 {
        self.hour as u32 * 60 + self.minute as u32
    }

    /// Строка «ЧЧ:ММ».
    pub fn time_label(&self) -> String {
        format!("{:02}:{:02}", self.hour, self.minute)
    }
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct Schedule {
    #[serde(default)]
    pub tasks: Vec<Task>,
}

impl Schedule {
    pub fn load() -> Self {
        fs::read_to_string(file())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = fs::create_dir_all(crate::config_dir());
            let _ = fs::write(file(), json);
        }
    }

    /// Следующий свободный идентификатор.
    pub fn next_id(&self) -> u64 {
        self.tasks.iter().map(|t| t.id).max().unwrap_or(0) + 1
    }
}
