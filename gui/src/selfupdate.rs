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
    // Никаких внешних сценариев: PowerShell с `-EncodedCommand` и bat-файлы
    // либо блокируются антивирусом, либо ломаются на путях с кириллицей.
    // Новый exe запускается с `--self-install` и сам заменяет старый файл,
    // как только тот завершится.
    let log = staging_dir().join("update.log");
    let _ = std::fs::create_dir_all(staging_dir());
    log_line(&log, &format!("LAUNCH new={} dst={}", new_exe.display(), current_exe.display()));

    // Метка «обновление начато»: если замена не удастся, следующий запуск
    // увидит её и не станет повторять установку автоматически.
    let pending = staging_dir().join("pending.flag");
    let _ = std::fs::write(&pending, env!("CARGO_PKG_VERSION"));

    spawn_self_install(new_exe, current_exe, std::process::id())
        .map_err(|e| format!("не удалось запустить установку: {e} (журнал: {})", log.display()))
}

/// Запустить новый exe в режиме установки: `--self-install <куда> <pid старого>`.
fn spawn_self_install(new_exe: &Path, dst: &Path, old_pid: u32) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new(new_exe)
            .arg("--self-install")
            .arg(dst)
            .arg(old_pid.to_string())
            .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
            .spawn()
            .map(|_| ())
    }
    #[cfg(not(windows))]
    {
        let _ = (new_exe, dst, old_pid);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "только для Windows",
        ))
    }
}

/// Режим установки: дождаться выхода старого процесса, заменить файл, запустить.
///
/// Замена повторяется: сразу после выхода процесса файл может быть ещё занят
/// антивирусом, индексатором или клиентом синхронизации — одиночный
/// `fs::copy` в этом случае падал, установщик запускал старую версию, и
/// обновление зацикливалось. Результат проверяется по размеру файла.
///
/// Возвращает код выхода процесса-установщика.
pub fn run_self_install(dst: &Path, old_pid: u32) -> i32 {
    let log = staging_dir().join("update.log");
    let my_version = env!("CARGO_PKG_VERSION");
    log_line(
        &log,
        &format!("INSTALL start ver={my_version} dst={} pid={old_pid}", dst.display()),
    );

    // Ждём выхода старого процесса: заменить работающий exe нельзя.
    for _ in 0..400 {
        if !process_alive(old_pid) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    std::thread::sleep(std::time::Duration::from_millis(400));

    let Ok(me) = std::env::current_exe() else {
        log_line(&log, "INSTALL fail: не определён путь установщика");
        message_box(
            "RemontExplorer — обновление",
            "Не удалось определить путь установщика.\nЗапустите приложение вручную.",
        );
        return 1;
    };

    let me = std::fs::canonicalize(&me).unwrap_or(me);
    let dst_cmp = std::fs::canonicalize(dst).unwrap_or_else(|_| dst.to_path_buf());
    if me == dst_cmp {
        // Установщик уже лежит на месте приложения — заменять нечего.
        log_line(&log, "INSTALL skip: установщик и цель совпадают");
        let _ = std::process::Command::new(dst).spawn();
        return 0;
    }

    match replace_file(&me, dst, &log) {
        Ok(()) => {
            let _ = std::fs::remove_file(staging_dir().join("pending.flag"));
            let _ = std::process::Command::new(dst).spawn();
            0
        }
        Err(last_error) => {
            log_line(&log, &format!("FAIL copy: {last_error}"));
            // Без сообщения пользователь видел бы только «приложение перезапустилось».
            message_box(
                "RemontExplorer — обновление не установлено",
                &format!(
                    "Не удалось заменить файл приложения.\n\n{}\n\nЦель: {}\nЖурнал: {}\n\nЗакройте программу и запустите её снова. Если приложение лежит на сетевом диске (\\\\server\\...), скопируйте его на локальный диск — там обновление проходит без помех.",
                    last_error,
                    dst.display(),
                    log.display()
                ),
            );
            let _ = std::process::Command::new(dst).spawn();
            1
        }
    }
}

/// Заменить файл `dst` содержимым `me`, до 20 попыток.
///
/// Сетевые папки (`\\server\share`) не дают перезаписать запущенный exe
/// (`os error 32`), но позволяют его **переименовать**: загрузчик Windows
/// открывает файл с правом на удаление. Поэтому старый файл уводится в
/// `<имя>.old`, после чего имя свободно.
fn replace_file(me: &Path, dst: &Path, log: &Path) -> Result<(), String> {
    let new_len = std::fs::metadata(me).map(|m| m.len()).unwrap_or(0);
    log_line(
        log,
        &format!("INSTALL copy {} -> {} ({new_len} байт)", me.display(), dst.display()),
    );

    let backup = dst.with_file_name(format!(
        "{}.old",
        dst.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "old".into())
    ));
    let mut renamed = false;
    let mut last_error = String::new();

    for attempt in 1..=20u32 {
        if attempt == 1 || attempt % 5 == 0 {
            let _ = std::fs::remove_file(&backup);
            match std::fs::rename(dst, &backup) {
                Ok(()) => {
                    renamed = true;
                    log_line(log, &format!("BACKUP renamed target to {}", backup.display()));
                }
                Err(e) => log_line(log, &format!("BACKUP skip: {e}")),
            }
        }
        // Снимаем «только чтение», если файл помечен: иначе копирование не пройдёт.
        // Только Windows: на Unix `set_readonly(false)` открыл бы файл всем.
        #[cfg(windows)]
        if let Ok(meta) = std::fs::metadata(dst) {
            let mut perms = meta.permissions();
            if perms.readonly() {
                perms.set_readonly(false);
                let _ = std::fs::set_permissions(dst, perms);
            }
        }
        match std::fs::copy(me, dst) {
            Ok(written) => {
                let ok = std::fs::metadata(dst)
                    .map(|m| m.len() == new_len && (new_len == 0 || written == new_len))
                    .unwrap_or(false);
                if ok {
                    log_line(log, &format!("OK file replaced on attempt {attempt}"));
                    let _ = std::fs::remove_file(&backup);
                    return Ok(());
                }
                last_error = format!("размер после копирования не совпал ({written} != {new_len})");
                log_line(log, &format!("RETRY {attempt}: {last_error}"));
            }
            Err(e) => {
                last_error = e.to_string();
                log_line(log, &format!("RETRY {attempt}: {e}"));
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    // Не вышло — возвращаем исходный файл на место, чтобы приложение не пропало.
    if renamed && !dst.exists() {
        let _ = std::fs::rename(&backup, dst);
    }
    Err(last_error)
}

/// Показать системное окно с сообщением (установщик работает без egui).
fn message_box(title: &str, text: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONWARNING, MB_OK};
        let wide = |s: &str| -> Vec<u16> {
            std::ffi::OsStr::new(s)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect()
        };
        let (t, b) = (wide(title), wide(text));
        // SAFETY: строки живут до конца вызова.
        unsafe {
            MessageBoxW(std::ptr::null_mut(), b.as_ptr(), t.as_ptr(), MB_OK | MB_ICONWARNING);
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (title, text);
    }
}

/// Жив ли процесс (Windows — через OpenProcess/WaitForSingleObject).
fn process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
        };
        // SAFETY: хэндл закрывается; ожидание с нулевым таймаутом.
        unsafe {
            let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if h.is_null() {
                return false;
            }
            let rc = WaitForSingleObject(h, 0);
            let _ = CloseHandle(h);
            rc == WAIT_TIMEOUT
        }
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        false
    }
}

/// Осталась ли метка о неудачной установке (см. `apply`).
pub fn pending_failed() -> bool {
    staging_dir().join("pending.flag").exists()
}

/// Снять метку после того, как пользователь увидел сообщение.
pub fn clear_pending() {
    let _ = std::fs::remove_file(staging_dir().join("pending.flag"));
}

/// Дописать строку в журнал обновления (ошибки журнала не критичны).
pub fn log_line(log: &Path, msg: &str) {
    use std::io::Write as _;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log) {
        let _ = writeln!(
            f,
            "{}  {msg}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        );
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




    /// Журнал обновления пишется в файл: по нему видно, что произошло.
    #[test]
    fn update_log_records_lines() {
        let dir = std::env::temp_dir().join(format!("upd_log_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("update.log");
        log_line(&log, "LAUNCH test=1");
        log_line(&log, "OK file replaced");
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("LAUNCH test=1"));
        assert!(text.contains("OK file replaced"));
        assert_eq!(text.lines().count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Метка «установка начата» снимается после успеха и читается после сбоя:
    /// по ней приложение понимает, что обновление не встало, и не зацикливается.
    #[test]
    fn pending_flag_roundtrip() {
        let dir = staging_dir();
        let _ = std::fs::create_dir_all(&dir);
        let flag = dir.join("pending.flag");
        let _ = std::fs::remove_file(&flag);
        assert!(!pending_failed());
        std::fs::write(&flag, "2.4.0").unwrap();
        assert!(pending_failed());
        clear_pending();
        assert!(!pending_failed());
    }

    /// Замена файла: новое содержимое на месте, резервная копия не остаётся.
    #[test]
    fn replace_file_overwrites_target() {
        let dir = std::env::temp_dir().join(format!("re_replace_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dst = dir.join("app.exe");
        let me = dir.join("new.exe");
        let log = dir.join("update.log");
        std::fs::write(&dst, b"old-version").unwrap();
        std::fs::write(&me, b"new-version-longer").unwrap();

        replace_file(&me, &dst, &log).expect("замена не удалась");

        assert_eq!(std::fs::read(&dst).unwrap(), b"new-version-longer");
        assert!(
            !dir.join("app.exe.old").exists(),
            "осталась резервная копия"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Если цели нет (первый запуск), замена всё равно проходит.
    #[test]
    fn replace_file_creates_missing_target() {
        let dir = std::env::temp_dir().join(format!("re_replace2_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dst = dir.join("app.exe");
        let me = dir.join("new.exe");
        let log = dir.join("update.log");
        std::fs::write(&me, b"fresh").unwrap();

        replace_file(&me, &dst, &log).expect("замена не удалась");
        assert_eq!(std::fs::read(&dst).unwrap(), b"fresh");
        let _ = std::fs::remove_dir_all(&dir);
    }

}
