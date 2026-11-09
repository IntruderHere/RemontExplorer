//! Каталоги данных приложения.
//!
//! Всё, что приложение пишет на диск (настройки, журналы, история),
//! хранится в «Документах» текущего пользователя:
//!
//! ```text
//! %USERPROFILE%\Documents\RemontExplorer\
//!     config\   — настройки (ui.json, schedule.json, списки путей)
//!     logs\     — журналы и история выполнений (history.jsonl)
//! ```
//!
//! Переменная окружения `REMONTEXPLORER_DATA_DIR` переопределяет корень —
//! используется в тестах, чтобы не трогать реальные документы.

use std::path::PathBuf;

/// Переменная окружения, переопределяющая каталог данных.
pub const DATA_DIR_ENV: &str = "REMONTEXPLORER_DATA_DIR";

/// Корень данных приложения: `Документы\RemontExplorer`.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV) {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let base = dirs::document_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("RemontExplorer")
}

/// Каталог настроек: `<данные>\config`. Создаётся при обращении.
pub fn config_dir() -> PathBuf {
    let dir = data_dir().join("config");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Каталог журналов: `<данные>\logs`. Создаётся при обращении.
pub fn logs_dir() -> PathBuf {
    let dir = data_dir().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Путь к файлу в каталоге настроек.
pub fn config_path(name: &str) -> PathBuf {
    config_dir().join(name)
}

/// Путь к файлу в каталоге журналов.
pub fn log_path(name: &str) -> PathBuf {
    logs_dir().join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Переменная окружения важнее «Документов» — на ней держатся тесты.
    #[test]
    fn env_overrides_documents() {
        let _guard = crate::test_env_lock();
        let tmp = std::env::temp_dir().join("remont_paths_env_test");
        std::env::set_var(DATA_DIR_ENV, &tmp);
        assert_eq!(data_dir(), tmp);
        assert!(logs_dir().starts_with(&tmp));
        std::env::remove_var(DATA_DIR_ENV);
    }
}
