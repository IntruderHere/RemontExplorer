//! Удаление в корзину Windows.
//!
//! Используется `SHFileOperationW` с `FOF_ALLOWUNDO`: файлы и папки попадают
//! в обычную корзину, откуда их можно восстановить штатными средствами.
//! Это Unicode-операция (кириллица в именах не страдает).
//!
//! Ограничение Windows: `SHFileOperationW` не понимает длинные пути
//! (`\\?\...`) и пути длиннее `MAX_PATH`; для них возвращается ошибка —
//! вызывающий решает, удалять ли безвозвратно.

use std::path::PathBuf;

/// Переместить пути в корзину.
///
/// Возвращает число путей, отправленных в корзину. Ошибка — если оболочка
/// отказала целиком (нет корзины, путь слишком длинный, отказ пользователя).
#[cfg(windows)]
pub fn move_to_recycle_bin(paths: &[PathBuf]) -> std::io::Result<usize> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::{
        SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FO_DELETE,
        SHFILEOPSTRUCTW,
    };

    let existing: Vec<&PathBuf> = paths.iter().filter(|p| p.exists() || is_broken_link(p)).collect();
    if existing.is_empty() {
        return Ok(0);
    }

    // Список путей: каждая строка завершается NUL, весь список — двумя NUL.
    let mut from: Vec<u16> = Vec::new();
    for p in &existing {
        let wide: Vec<u16> = p.as_os_str().encode_wide().collect();
        if wide.len() >= 259 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "путь длиннее 259 символов — корзина Windows его не принимает",
            ));
        }
        from.extend(wide);
        from.push(0);
    }
    from.push(0);

    let mut op: SHFILEOPSTRUCTW = unsafe { std::mem::zeroed() };
    op.wFunc = FO_DELETE;
    op.pFrom = from.as_ptr();
    op.fFlags = (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI) as u16;

    // SAFETY: структура и строка путей живут до конца вызова; список
    // корректно завершён двойным NUL.
    let rc = unsafe { SHFileOperationW(&mut op) };
    if rc != 0 {
        return Err(std::io::Error::from_raw_os_error(rc as i32));
    }
    if op.fAnyOperationsAborted != 0 {
        return Err(std::io::Error::other("операция отменена оболочкой Windows"));
    }
    Ok(existing.len())
}

/// Битые символические ссылки: `exists()` на них ложен, но удалить их можно.
#[cfg(windows)]
fn is_broken_link(p: &std::path::Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

/// Вне Windows корзины нет — операция не поддерживается.
#[cfg(not(windows))]
pub fn move_to_recycle_bin(_paths: &[PathBuf]) -> std::io::Result<usize> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "корзина доступна только в Windows",
    ))
}

/// Доступна ли корзина в этой сборке.
pub const fn is_supported() -> bool {
    cfg!(windows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_flag_matches_platform() {
        assert_eq!(is_supported(), cfg!(windows));
    }

    #[test]
    #[cfg(not(windows))]
    fn unsupported_platform_returns_error() {
        let err = move_to_recycle_bin(&[PathBuf::from("/tmp/x")]).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
    }

    #[test]
    #[cfg(windows)]
    fn rejects_too_long_path() {
        let long = PathBuf::from(format!("C:\\{}", "a".repeat(300)));
        assert!(move_to_recycle_bin(&[long]).is_err());
    }
}
