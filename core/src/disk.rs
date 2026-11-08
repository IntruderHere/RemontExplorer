//! Свободное место на диске и предварительная проверка перед копированием.
//!
//! Смысл проверки — не начинать перенос, который гарантированно упрётся в
//! «диск заполнен» на середине: часть файлов уже перенесена, повторить
//! операцию без разбора сложно.

use std::path::{Path, PathBuf};

/// Запас сверх требуемого объёма (5%), чтобы не заполнить диск «под ноль»:
/// системе тоже нужно место для временных файлов.
pub const SAFETY_MARGIN_PERCENT: u64 = 5;

/// Сколько байт свободно на диске, на котором лежит `path`.
///
/// Для несуществующего пути поднимаемся к ближайшему существующему предку —
/// целевая папка может быть создана позже.
pub fn available_space(path: &Path) -> Option<u64> {
    let mut probe = path.to_path_buf();
    while !probe.exists() {
        match probe.parent() {
            Some(p) if !p.as_os_str().is_empty() => probe = p.to_path_buf(),
            _ => return None,
        }
    }
    platform_available(&probe)
}

#[cfg(windows)]
fn platform_available(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut free: u64 = 0;
    // SAFETY: указатель на валидную широкую строку с нулём на конце.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
    (ok != 0).then_some(free)
}

#[cfg(unix)]
fn platform_available(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;

    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs заполняет предоставленную структуру; строка валидна.
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut st) != 0 {
            return None;
        }
        Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
    }
}

#[cfg(not(any(windows, unix)))]
fn platform_available(_path: &Path) -> Option<u64> {
    None
}

/// Суммарный размер файлов и папок (без символических ссылок внутри папок).
pub fn total_size(paths: &[PathBuf]) -> u64 {
    let mut total = 0u64;
    for p in paths {
        let Ok(md) = std::fs::symlink_metadata(p) else {
            continue;
        };
        if md.is_dir() {
            for entry in walkdir::WalkDir::new(p).follow_links(false).into_iter().flatten() {
                if let Ok(m) = entry.metadata() {
                    if m.is_file() {
                        total += m.len();
                    }
                }
            }
        } else {
            total += md.len();
        }
    }
    total
}

/// Сколько нужно места с учётом запаса.
pub fn required_with_margin(bytes: u64) -> u64 {
    bytes.saturating_add(bytes / 100 * SAFETY_MARGIN_PERCENT)
}

/// Проверить, хватит ли места для переноса `bytes` байт в `target`.
///
/// Возвращает `Ok(свободно)` если места хватает, `Err((нужно, свободно))`
/// если нет. `None` от системы (не смогли узнать) трактуется как «хватает»:
/// блокировать операцию из-за неизвестного объёма неправильно.
#[allow(clippy::result_large_err)]
pub fn check_space(bytes: u64, target: &Path) -> Result<u64, (u64, u64)> {
    let need = required_with_margin(bytes);
    match available_space(target) {
        Some(free) if free < need => Err((need, free)),
        Some(free) => Ok(free),
        None => Ok(0),
    }
}

/// Человекочитаемый объём: «1,4 ГБ».
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["Б", "КБ", "МБ", "ГБ", "ТБ"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_available_space_for_existing_dir() {
        let free = available_space(&std::env::temp_dir()).expect("не удалось узнать место");
        assert!(free > 0, "свободное место = 0");
    }

    #[test]
    fn nonexistent_path_walks_up_to_existing_parent() {
        let ghost = std::env::temp_dir().join("remont_no_such_dir_12345").join("a").join("b");
        assert!(available_space(&ghost).is_some());
    }

    #[test]
    fn total_size_sums_files_and_dirs() {
        let dir = std::env::temp_dir().join(format!("remont_disk_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.bin"), vec![0u8; 1000]).unwrap();
        std::fs::write(dir.join("sub").join("b.bin"), vec![0u8; 2345]).unwrap();
        assert_eq!(total_size(&[dir.clone()]), 3345);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn margin_adds_five_percent() {
        assert_eq!(required_with_margin(1000), 1050);
        assert_eq!(required_with_margin(0), 0);
    }

    #[test]
    fn huge_request_does_not_fit() {
        let (need, free) = check_space(u64::MAX / 4, &std::env::temp_dir())
            .expect_err("такой объём не должен помещаться");
        assert!(need > free);
    }

    #[test]
    fn small_request_fits() {
        assert!(check_space(1024, &std::env::temp_dir()).is_ok());
    }

    #[test]
    fn human_formats_units() {
        assert_eq!(human(512), "512 Б");
        assert_eq!(human(2048), "2.0 КБ");
        assert!(human(5 * 1024 * 1024 * 1024).ends_with("ГБ"));
    }
}
