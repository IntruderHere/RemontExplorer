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
///
/// Почему PowerShell, а не bat: `.bat` читается cmd в OEM-кодировке, и путь
/// с кириллицей (`C:\Users\Иван\AppData\...`) превращается в мусор — замена
/// не выполнялась, приложение просто закрывалось. `-EncodedCommand`
/// передаёт сценарий в UTF-16 base64, кодировка не участвует вовсе.
pub fn apply(current_exe: &Path, new_exe: &Path) -> Result<(), String> {
    let dir = staging_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("не создать папку обновления: {e}"))?;
    let log = dir.join("update.log");
    let script = build_script(std::process::id(), new_exe, current_exe, &log);
    // Диагностика: если обновление не сработает, в этом файле останется след.
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .and_then(|mut f| {
            use std::io::Write as _;
            writeln!(
                f,
                "LAUNCH {} new={} dst={}",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                new_exe.display(),
                current_exe.display()
            )
        });

    let encoded = encode_powershell(&script);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-EncodedCommand",
                &encoded,
            ])
            .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| {
                format!(
                    "не удалось запустить обновление: {e} (подробности: {})",
                    log.display()
                )
            })?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = encoded;
        Err("обновление с заменой файла поддерживается только в Windows".into())
    }
}

/// Текст сценария замены. Вынесен отдельно, чтобы проверялся тестом:
/// ошибки в кавычках здесь означали бы «приложение закрылось и не вернулось».
fn build_script(pid: u32, new_exe: &Path, dst: &Path, log: &Path) -> String {
    // Одинарные кавычки в PowerShell: внутри строки удваиваются.
    let q = |p: &Path| p.to_string_lossy().replace('\'', "''");
    format!(
        r#"$ErrorActionPreference = 'Stop'
$log = '{log}'
$oldPid = {pid}
try {{
    # Ждём выхода приложения: заменить работающий exe нельзя.
    for ($i = 0; $i -lt 600; $i++) {{
        if (-not (Get-Process -Id $oldPid -ErrorAction SilentlyContinue)) {{ break }}
        Start-Sleep -Milliseconds 300
    }}
    Start-Sleep -Milliseconds 300
    Copy-Item -LiteralPath '{new}' -Destination '{dst}' -Force
    Add-Content -LiteralPath $log -Value ("OK  " + (Get-Date -Format s) + "  " + '{dst}')
    Start-Process -FilePath '{dst}' -WorkingDirectory (Split-Path -LiteralPath '{dst}')
    exit 0
}} catch {{
    Add-Content -LiteralPath $log -Value ("FAIL " + (Get-Date -Format s) + "  " + $_.Exception.Message)
    # Даже при неудаче возвращаем приложение пользователю (старая версия).
    try {{ Start-Process -FilePath '{dst}' -WorkingDirectory (Split-Path -LiteralPath '{dst}') }} catch {{ }}
    exit 1
}}
"#,
        log = q(log),
        pid = pid,
        new = q(new_exe),
        dst = q(dst)
    )
}

/// Сценарий PowerShell → base64 UTF-16LE (формат `-EncodedCommand`).
fn encode_powershell(script: &str) -> String {
    let utf16: Vec<u8> = script
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    base64(&utf16)
}

/// Минимальный base64 (без внешних зависимостей).
fn base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
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
    fn base64_matches_reference() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"hello"), "aGVsbG8=");
        assert_eq!(base64(b"\xff\xfe"), "//4=");
    }

    /// `-EncodedCommand` ждёт UTF-16LE в base64: проверяем декодированием.
    #[test]
    fn powershell_command_is_utf16_base64() {
        let script = "Write-Output 'Привет, ОПС'";
        let enc = encode_powershell(script);
        let bytes = decode_base64(&enc);
        let units: Vec<u16> = (0..bytes.len() / 2)
            .map(|i| u16::from_le_bytes([bytes[i * 2], bytes[i * 2 + 1]]))
            .collect();
        let decoded = String::from_utf16(&units).unwrap();
        assert_eq!(decoded, script);
    }

    /// Простая проверка base64 — нужна только в тестах.
    fn decode_base64(s: &str) -> Vec<u8> {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = Vec::new();
        let mut buf: u32 = 0;
        let mut bits = 0;
        for ch in s.bytes() {
            if ch == b'=' {
                break;
            }
            let Some(v) = T.iter().position(|&c| c == ch) else { continue };
            buf = (buf << 6) | v as u32;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((buf >> bits) as u8);
            }
        }
        out
    }


    /// Сценарий ждёт PID, копирует и запускает приложение; пути с одинарными
    /// кавычками не ломают синтаксис.
    #[test]
    fn update_script_waits_copies_and_starts() {
        let new = Path::new(r"C:\Temp\new.exe");
        let dst = Path::new(r"C:\Program Files\Remont's\remont_explorer.exe");
        let log = Path::new(r"C:\Temp\update.log");
        let script = build_script(4242, new, dst, log);

        assert!(script.contains("$oldPid = 4242"));
        assert!(script.contains("Get-Process -Id $oldPid"));
        assert!(script.contains("Copy-Item -LiteralPath"));
        assert!(script.contains("Start-Process -FilePath"));
        // Кавычка в пути удвоена — иначе PowerShell оборвал бы строку.
        assert!(script.contains("Remont''s"));
        assert!(!script.contains("Remont's"));
        // Даже при ошибке старая версия запускается обратно.
        assert!(script.contains("catch"));
        assert!(script.contains("exit 1"));
    }

}
