//! Уведомление Windows по завершении операции.
//!
//! Используется всплывающая подсказка значка в области уведомлений
//! (`Shell_NotifyIcon` + `NIF_INFO`): на Windows 10/11 она показывается как
//! системное уведомление и не требует дополнительных зависимостей.
//!
//! Значок создаётся один раз (при первом уведомлении) и живёт до выхода из
//! приложения — отдельно от значка «свернуть в трей».

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows_sys::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_INFO, NIIF_INFO, NIIF_WARNING, NIM_ADD, NIM_MODIFY,
        NOTIFYICONDATAW,
    };

    /// Уникальный идентификатор значка уведомлений (не пересекается с треем).
    const NOTIFY_UID: u32 = 0x5245; // 'RE'

    static ADDED: AtomicBool = AtomicBool::new(false);

    fn wide(s: &str, buf: &mut [u16]) {
        let src: Vec<u16> = s.encode_utf16().collect();
        let n = src.len().min(buf.len().saturating_sub(1));
        buf[..n].copy_from_slice(&src[..n]);
        buf[n] = 0;
    }

    /// Показать уведомление. `hwnd` — окно приложения (владелец значка).
    pub fn notify(hwnd: isize, title: &str, body: &str, warning: bool) {
        if hwnd == 0 {
            return;
        }
        // SAFETY: структура целиком заполняется нулями и валидными полями;
        // строки копируются с завершающим NUL в фиксированные буферы.
        unsafe {
            let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = hwnd as *mut core::ffi::c_void;
            nid.uID = NOTIFY_UID;

            if !ADDED.swap(true, Ordering::SeqCst) {
                nid.uFlags = 0;
                let _ = Shell_NotifyIconW(NIM_ADD, &nid);
            }

            nid.uFlags = NIF_INFO;
            nid.dwInfoFlags = if warning { NIIF_WARNING } else { NIIF_INFO };
            wide(title, &mut nid.szInfoTitle);
            wide(body, &mut nid.szInfo);
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }
}

#[cfg(not(windows))]
mod imp {
    /// Вне Windows уведомления не показываются.
    pub fn notify(_hwnd: isize, _title: &str, _body: &str, _warning: bool) {}
}

pub use imp::notify;

#[cfg(test)]
mod tests {
    use super::notify;

    /// Вне Windows вызов не падает и ничего не делает.
    #[test]
    #[cfg(not(windows))]
    fn notify_is_noop_off_windows() {
        notify(0, "t", "b", false);
    }
}
