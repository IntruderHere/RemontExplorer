//! Интеграция с Windows: пункт в контекстном меню Проводника и автозапуск.
//!
//! Всё пишется в `HKEY_CURRENT_USER` через `reg.exe` — права администратора
//! не нужны, сторонних крейтов не требуется. Приложение поставляется только
//! для Windows; ветки для других ОС нужны лишь для прогона тестов на машине
//! разработчика и в продукт не входят.

use std::path::Path;

/// Ключ пункта контекстного меню папок.
#[cfg_attr(not(windows), allow(dead_code))]
const MENU_KEY: &str = r"HKCU\Software\Classes\Directory\shell\RemontExplorer";
/// Ключ автозапуска текущего пользователя.
#[cfg_attr(not(windows), allow(dead_code))]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
/// Имя значения автозапуска.
#[cfg_attr(not(windows), allow(dead_code))]
const RUN_VALUE: &str = "RemontExplorer";
/// Подпись пункта меню на русском языке.
pub const MENU_CAPTION_RU: &str = "Просканировать папку в RemontExplorer";
/// Подпись пункта меню на английском языке.
pub const MENU_CAPTION_EN: &str = "Scan with RemontExplorer";

/// Подпись пункта меню для выбранного языка.
pub fn menu_caption(russian: bool) -> &'static str {
    if russian {
        MENU_CAPTION_RU
    } else {
        MENU_CAPTION_EN
    }
}

#[cfg_attr(windows, allow(dead_code))]
const NOT_WINDOWS: &str = "доступно только в Windows";

#[cfg(windows)]
fn reg(args: &[&str]) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    // Без этого при вызове reg.exe мигает консольное окно.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new("reg")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("не удалось запустить reg.exe: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() {
            "reg.exe завершился с ошибкой".to_string()
        } else {
            err
        })
    }
}

#[cfg(windows)]
fn key_exists(key: &str) -> bool {
    reg(&["query", key]).is_ok()
}

#[cfg(windows)]
fn value_exists(key: &str, value: &str) -> bool {
    reg(&["query", key, "/v", value]).is_ok()
}

/// Есть ли пункт в контекстном меню папок.
pub fn menu_installed() -> bool {
    #[cfg(windows)]
    {
        key_exists(MENU_KEY) && key_exists(&format!(r"{MENU_KEY}\command"))
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Добавить пункт «Просканировать папку в RemontExplorer» в контекстное меню.
/// `exe` — полный путь к исполняемому файлу приложения.
pub fn menu_install(exe: &Path, caption: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        let cmd = format!("\"{}\" --scan \"%1\"", exe.display());
        reg(&["add", MENU_KEY, "/ve", "/d", caption, "/f"])?;
        reg(&[
            "add",
            &format!(r"{MENU_KEY}\command"),
            "/ve",
            "/d",
            &cmd,
            "/f",
        ])?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (exe, caption);
        Err(NOT_WINDOWS.to_string())
    }
}

/// Удалить пункт из контекстного меню.
pub fn menu_uninstall() -> Result<(), String> {
    #[cfg(windows)]
    {
        reg(&["delete", MENU_KEY, "/f"])?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        Err(NOT_WINDOWS.to_string())
    }
}

/// Запускается ли приложение вместе с Windows.
pub fn autostart_installed() -> bool {
    #[cfg(windows)]
    {
        value_exists(RUN_KEY, RUN_VALUE)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Включить автозапуск вместе с Windows (для текущего пользователя).
pub fn autostart_install(exe: &Path, minimized: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        let value = if minimized {
            format!("\"{}\" --minimized", exe.display())
        } else {
            format!("\"{}\"", exe.display())
        };
        reg(&[
            "add", RUN_KEY, "/v", RUN_VALUE, "/t", "REG_SZ", "/d", &value, "/f",
        ])?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (exe, minimized);
        Err(NOT_WINDOWS.to_string())
    }
}

/// Выключить автозапуск.
pub fn autostart_uninstall() -> Result<(), String> {
    #[cfg(windows)]
    {
        reg(&["delete", RUN_KEY, "/v", RUN_VALUE, "/f"])?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        Err(NOT_WINDOWS.to_string())
    }
}

/// Путь к текущему исполняемому файлу (для команд меню и автозапуска).
pub fn exe_path() -> std::path::PathBuf {
    std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("RemontExplorer.exe"))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn menu_and_autostart_roundtrip() {
        let exe = exe_path();
        // ставим, проверяем, снимаем
        menu_install(&exe, MENU_CAPTION_RU).expect("установка пункта меню");
        assert!(menu_installed());
        menu_uninstall().expect("удаление пункта меню");
        assert!(!menu_installed());

        autostart_install(&exe, true).expect("включение автозапуска");
        assert!(autostart_installed());
        autostart_uninstall().expect("выключение автозапуска");
        assert!(!autostart_installed());
    }
}
