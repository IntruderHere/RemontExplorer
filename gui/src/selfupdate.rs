//! Самообновление: скачать архив релиза, распаковать и заменить приложение.
//!
//! Порядок:
//! 1. `download` — качает `.zip` с GitHub в `%TEMP%\RemontExplorerUpdate`,
//!    отдавая прогресс (байты получены / всего).
//! 2. `unpack` — распаковывает и находит внутри `remont_explorer.exe`.
//! 3. `apply` — создаёт bat-файл, который дождётся выхода приложения,
//!    скопирует новый exe на место старого и запустит его заново.
//!
//! Заменить запущенный exe в Windows нельзя (файл заблокирован), поэтому
//! используется отдельный процесс cmd, переживающий закрытие приложения.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Папка для скачанного архива.
pub fn staging_dir() -> PathBuf {
    std::env::temp_dir().join("RemontExplorerUpdate")
}

/// Скачать архив. `progress(получено, всего)`; `всего = 0`, если сервер не
/// сообщил размер.
pub fn download(
    url: &str,
    dest: &Path,
    mut progress: impl FnMut(u64, u64),
) -> Result<(), String> {
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("не создать папку загрузки: {e}"))?;
    }
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(10))
        // Сама загрузка может быть долгой — ограничение на соединение, не на объём.
        .timeout_read(std::time::Duration::from_secs(60))
        .user_agent(concat!("RemontExplorer/", env!("CARGO_PKG_VERSION")))
        .build();

    let resp = agent
        .get(url)
        .call()
        .map_err(|e| format!("не удалось скачать обновление: {e}"))?;
    let total: u64 = resp
        .header("Content-Length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(dest).map_err(|e| format!("не создать файл: {e}"))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut got: u64 = 0;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("ошибка загрузки: {e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| format!("ошибка записи: {e}"))?;
        got += n as u64;
        progress(got, total);
    }
    file.flush().map_err(|e| format!("ошибка записи: {e}"))?;

    // Обрезанная загрузка — не обновляемся битым файлом.
    if total > 0 && got != total {
        let _ = std::fs::remove_file(dest);
        return Err(format!("загрузка неполная: {got} из {total} байт"));
    }
    Ok(())
}

/// Распаковать архив и вернуть путь к новому exe.
pub fn unpack(zip_path: &Path, out_dir: &Path) -> Result<PathBuf, String> {
    let _ = std::fs::remove_dir_all(out_dir);
    std::fs::create_dir_all(out_dir).map_err(|e| format!("не создать папку распаковки: {e}"))?;

    let file = std::fs::File::open(zip_path).map_err(|e| format!("архив не открывается: {e}"))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("это не ZIP-архив: {e}"))?;

    let mut exe: Option<PathBuf> = None;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("ошибка чтения архива: {e}"))?;
        let name = entry.name().to_string();
        // Защита от путей с выходом из каталога (`../`).
        if name.contains("..") {
            continue;
        }
        let out = out_dir.join(&name);
        if entry.is_dir() {
            let _ = std::fs::create_dir_all(&out);
            continue;
        }
        if let Some(parent) = out.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut f = std::fs::File::create(&out).map_err(|e| format!("не создать {name}: {e}"))?;
        std::io::copy(&mut entry, &mut f).map_err(|e| format!("ошибка распаковки: {e}"))?;
        if name.to_lowercase().ends_with(".exe") {
            exe = Some(out);
        }
    }
    exe.ok_or_else(|| "в архиве нет .exe".to_string())
}

/// Запустить замену приложения и выйти.
///
/// Возвращает управление только при ошибке запуска — при успехе вызывающий
/// должен завершить процесс (`std::process::exit(0)`).
pub fn apply(current_exe: &Path, new_exe: &Path) -> Result<(), String> {
    let dir = staging_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("не создать папку обновления: {e}"))?;
    let bat = dir.join("apply_update.bat");

    let script = format!(
        "@echo off\r\n\
chcp 65001 >nul\r\n\
setlocal\r\n\
set \"PID={pid}\"\r\n\
set \"NEW={new}\"\r\n\
set \"DST={dst}\"\r\n\
:wait\r\n\
tasklist /FI \"PID eq %PID%\" 2>nul | findstr /C:\"%PID%\" >nul\r\n\
if errorlevel 1 goto apply\r\n\
timeout /t 1 /nobreak >nul\r\n\
goto wait\r\n\
:apply\r\n\
copy /y \"%NEW%\" \"%DST%\" >nul\r\n\
if errorlevel 1 (\r\n\
  echo Не удалось заменить файл: %DST% 1>>\"%~dp0update.log\"\r\n\
) else (\r\n\
  echo Обновление установлено: %DST% 1>>\"%~dp0update.log\"\r\n\
)\r\n\
start \"\" \"%DST%\"\r\n\
timeout /t 2 /nobreak >nul\r\n\
del \"%~f0\"\r\n",
        pid = std::process::id(),
        new = new_exe.display(),
        dst = current_exe.display()
    );

    std::fs::write(&bat, script.as_bytes()).map_err(|e| format!("не создать сценарий: {e}"))?;

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("cmd")
            .args(["/C", &bat.to_string_lossy()])
            .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| format!("не удалось запустить обновление: {e}"))?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        Err("обновление с заменой файла поддерживается только в Windows".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpack_extracts_exe() {
        let dir = std::env::temp_dir().join(format!("upd_unpack_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let zip_path = dir.join("app.zip");
        {
            let f = std::fs::File::create(&zip_path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            zw.start_file("remont_explorer.exe", opts).unwrap();
            zw.write_all(b"MZ fake exe").unwrap();
            zw.start_file("readme.txt", opts).unwrap();
            zw.write_all(b"hello").unwrap();
            zw.finish().unwrap();
        }

        let out = dir.join("out");
        let exe = unpack(&zip_path, &out).expect("распаковка");
        assert!(exe.exists());
        assert_eq!(exe.file_name().unwrap(), "remont_explorer.exe");
        assert_eq!(std::fs::read(&exe).unwrap(), b"MZ fake exe");
        assert!(out.join("readme.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unpack_rejects_zip_without_exe() {
        let dir = std::env::temp_dir().join(format!("upd_noexe_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let zip_path = dir.join("app.zip");
        {
            let f = std::fs::File::create(&zip_path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            zw.start_file("readme.txt", zip::write::SimpleFileOptions::default())
                .unwrap();
            zw.write_all(b"x").unwrap();
            zw.finish().unwrap();
        }
        assert!(unpack(&zip_path, &dir.join("out")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unpack_skips_path_traversal() {
        let dir = std::env::temp_dir().join(format!("upd_trav_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let zip_path = dir.join("app.zip");
        {
            let f = std::fs::File::create(&zip_path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            zw.start_file("../evil.exe", zip::write::SimpleFileOptions::default())
                .unwrap();
            zw.write_all(b"evil").unwrap();
            zw.finish().unwrap();
        }
        let out = dir.join("out");
        assert!(unpack(&zip_path, &out).is_err(), "путь с .. должен игнорироваться");
        assert!(!dir.join("evil.exe").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Реальная загрузка по HTTP: сервер отдаёт файл, проверяем содержимое
    /// и прогресс (без сети наружу).
    #[test]
    fn downloads_over_http_with_progress() {
        use std::io::Read as _;
        use std::net::TcpListener;

        let body = b"UPDATE-BYTES-1234567890".to_vec();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let body_srv = body.clone();
        let srv = std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body_srv.len()
                );
                use std::io::Write as _;
                let _ = sock.write_all(header.as_bytes());
                let _ = sock.write_all(&body_srv);
            }
        });

        let dir = std::env::temp_dir().join(format!("upd_http_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("update.zip");

        let mut seen: Vec<(u64, u64)> = Vec::new();
        let url = format!("http://{addr}/update.zip");
        let res = download(&url, &dest, |got, total| seen.push((got, total)));
        srv.join().ok();

        res.expect("загрузка не удалась");
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert!(!seen.is_empty(), "прогресс не сообщался");
        let (got, total) = *seen.last().unwrap();
        assert_eq!(got as usize, body.len());
        assert_eq!(total as usize, body.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_fails_on_bad_url() {
        let dir = std::env::temp_dir().join(format!("upd_bad_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("x.zip");
        let r = download("http://127.0.0.1:1/nothing.zip", &dest, |_, _| {});
        assert!(r.is_err(), "несуществующий сервер должен давать ошибку");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn staging_dir_is_in_temp() {
        let p = staging_dir();
        assert!(p.starts_with(std::env::temp_dir()));
        assert!(p.to_string_lossy().contains("RemontExplorerUpdate"));
    }

    #[test]
    #[cfg(windows)]
    fn apply_creates_script() {
        let dir = std::env::temp_dir().join(format!("upd_apply_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cur = dir.join("remont_explorer.exe");
        let new = dir.join("new.exe");
        std::fs::write(&cur, b"old").unwrap();
        std::fs::write(&new, b"new").unwrap();
        apply(&cur, &new).expect("сценарий обновления");
        let bat = staging_dir().join("apply_update.bat");
        assert!(bat.exists());
        let body = std::fs::read_to_string(&bat).unwrap();
        assert!(body.contains("copy /y"));
        assert!(body.contains(&format!("{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
