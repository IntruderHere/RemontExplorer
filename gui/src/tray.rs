//! Значок в системном трее (реализован для Windows).
//!
//! Почему отдельный поток: пока окно скрыто (`SW_HIDE`), egui не получает
//! кадров, поэтому обработчики в `update()` не выполняются и пункты меню
//! трея «не работают». Поток опрашивает события значка и управляет окном
//! напрямую через Win32 (показать), а «Закрыть приложение» завершает процесс.
//!
//! Приложение поставляется только для Windows. Заглушка для остальных ОС
//! существует исключительно для того, чтобы прогонять тесты логики на
//! машине разработчика; в продукт она не входит.

use std::sync::atomic::AtomicIsize;
use std::sync::Arc;

/// Показать окно по дескриптору (0 — окно ещё не создано).
#[cfg(windows)]
pub fn show_window(hwnd: isize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SetForegroundWindow, ShowWindow, SW_SHOW};
    if hwnd == 0 {
        return;
    }
    unsafe {
        ShowWindow(hwnd as *mut core::ffi::c_void, SW_SHOW);
        SetForegroundWindow(hwnd as *mut core::ffi::c_void);
    }
}

/// Скрыть окно в трей.
#[cfg(windows)]
pub fn hide_window(hwnd: isize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};
    if hwnd == 0 {
        return;
    }
    unsafe {
        ShowWindow(hwnd as *mut core::ffi::c_void, SW_HIDE);
    }
}

#[cfg(not(windows))]
#[allow(dead_code)]
pub fn show_window(_hwnd: isize) {}

#[cfg(not(windows))]
pub fn hide_window(_hwnd: isize) {}

#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};

/// Показать окно и попросить egui перерисовать содержимое.
#[cfg(windows)]
fn reveal(hwnd: &AtomicIsize, ctx: &egui::Context) {
    show_window(hwnd.load(Ordering::Relaxed));
    ctx.request_repaint();
}

#[cfg(windows)]
pub struct TrayState {
    /// Держим значок живым: при удалении он исчезает из трея.
    _icon: tray_icon::TrayIcon,
    /// Просим поток остановиться при уничтожении состояния.
    stop: Arc<AtomicBool>,
}

#[cfg(windows)]
pub fn create(
    icon_png: &[u8],
    tooltip: &str,
    open_text: &str,
    close_text: &str,
    hwnd: Arc<AtomicIsize>,
    ctx: egui::Context,
) -> Result<TrayState, String> {
    use tray_icon::menu::{Menu, MenuItem};
    use tray_icon::{Icon, TrayIconBuilder};

    let img = image::load_from_memory(icon_png)
        .map_err(|e| format!("иконка трея: {e}"))?
        .to_rgba8();
    let (w, h) = img.dimensions();
    let icon = Icon::from_rgba(img.into_raw(), w, h).map_err(|e| format!("иконка трея: {e}"))?;

    let open_item = MenuItem::new(open_text, true, None);
    let close_item = MenuItem::new(close_text, true, None);
    let menu = Menu::new();
    menu.append(&open_item)
        .map_err(|e| format!("меню трея: {e}"))?;
    menu.append(&close_item)
        .map_err(|e| format!("меню трея: {e}"))?;

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(tooltip)
        .with_icon(icon)
        .build()
        .map_err(|e| format!("значок трея: {e}"))?;

    let open_id = open_item.id().clone();
    let close_id = close_item.id().clone();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);

    // Поток живёт всё время работы значка: события читаются независимо от
    // того, рисует ли egui кадры (у скрытого окна кадров нет).
    std::thread::spawn(move || {
        use tray_icon::{MouseButton, MouseButtonState, TrayIconEvent};
        while !stop_thread.load(Ordering::Relaxed) {
            while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
                match ev {
                    TrayIconEvent::DoubleClick { .. } => reveal(&hwnd, &ctx),
                    TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } => reveal(&hwnd, &ctx),
                    _ => {}
                }
            }
            while let Ok(ev) = tray_icon::menu::MenuEvent::receiver().try_recv() {
                if ev.id() == &open_id {
                    reveal(&hwnd, &ctx);
                } else if ev.id() == &close_id {
                    // Настройки и расписание сохраняются на диск сразу при
                    // изменениях, поэтому здесь достаточно гарантированно
                    // завершить процесс.
                    std::process::exit(0);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(80));
        }
    });

    Ok(TrayState { _icon: tray, stop })
}

#[cfg(windows)]
impl Drop for TrayState {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

// ---------------- Заглушки для не-Windows ----------------

#[cfg(not(windows))]
pub struct TrayState;

#[cfg(not(windows))]
pub fn create(
    _icon_png: &[u8],
    _tooltip: &str,
    _open_text: &str,
    _close_text: &str,
    _hwnd: Arc<AtomicIsize>,
    _ctx: egui::Context,
) -> Result<TrayState, String> {
    Err("значок в трее доступен только в Windows".to_string())
}
