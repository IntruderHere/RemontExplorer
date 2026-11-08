//! RemontExplorer GUI (egui/eframe): интерфейс и файловые операции.
//! Целевая ОС — Windows. Сборка на других ОС используется только для
//! прогона тестов логики и в поставку не входит.
//! Структура 1:1: вкладки «Работа» / «Настройки»; в «Работе» — ряд кнопок
//! режимов, «Старт/Остановить», описание режима, «Консоль вывода»; в
//! «Настройках» — секция «Интерфейс» (тема, язык) + список режимов и
//! редакторы config/*.txt.
#![cfg_attr(windows, windows_subsystem = "windows")] // без консольного окна

mod i18n;
mod logfmt;
mod notify;
mod schedule;
mod selfupdate;
mod tray;
mod updates;
mod winshell;

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use chrono::Timelike;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::sync::atomic::{AtomicIsize, Ordering};
use egui::{FontId, RichText, ScrollArea, TextEdit};
use remont_core::i18n::Lang;
use remont_core::OutputFormat;
use remont_core::{
    archive_folders, cleanup_empty_folders, compare_folders, copy_files_from_list, create_folders,
    delete_paths, emit, find_duplicates_multi, read_paths_file, scan_folder, set_global,
    CompareMethod, HashAlgo, LogSink, Sink, StopToken,
};

const DONE: &str = "\0__OP_DONE__";
const MAX_LOG: usize = 2000;

/// Настройки интерфейса: тема + язык. Хранится в `config/ui.json`.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
enum ThemeMode {
    Light,
    Dark,
    System,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, PartialEq)]
struct UiConfig {
    theme: ThemeMode,
    lang: Lang,
    /// Сворачивать в трей вместо закрытия окна (по умолчанию — включено).
    #[serde(default = "default_true")]
    close_to_tray: bool,
    /// Формат файлов отчёта: CSV (по умолчанию) или XLSX.
    #[serde(default)]
    report_format: OutputFormat,
    /// Единая папка для результатов всех операций (пусто — рядом с приложением).
    #[serde(default)]
    output_dir: String,
    /// Открывать полученный файл после выполнения функции.
    #[serde(default)]
    open_result: bool,
    /// Проверять обновления на GitHub при запуске.
    #[serde(default = "default_true")]
    check_updates: bool,
    /// Скачивать обновление в фоне сразу после обнаружения.
    #[serde(default = "default_true")]
    auto_download: bool,
    /// Устанавливать без вопроса: закрыть, заменить, запустить заново.
    #[serde(default)]
    auto_install: bool,
    /// Показывать уведомление Windows по завершении операции.
    #[serde(default = "default_true")]
    notify: bool,
    /// Как удалять: безвозвратно, в корзину или в архив.
    #[serde(default)]
    delete_mode: remont_core::DeleteMode,
    /// Сверять копии по SHA-256 после копирования.
    #[serde(default)]
    verify_copy: bool,
    /// Проверять свободное место перед копированием.
    #[serde(default = "default_true")]
    check_space: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: ThemeMode::Light,
            lang: Lang::Ru,
            close_to_tray: true,
            report_format: OutputFormat::Csv,
            output_dir: String::new(),
            open_result: false,
            check_updates: true,
            auto_download: true,
            auto_install: false,
            notify: true,
            delete_mode: remont_core::DeleteMode::Permanent,
            verify_copy: false,
            check_space: true,
        }
    }
}

fn default_true() -> bool {
    true
}

fn load_ui_config() -> UiConfig {
    fs::read_to_string(config_path("ui.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save_ui_config(cfg: &UiConfig) {
    if let Ok(json) = serde_json::to_string_pretty(cfg) {
        let _ = fs::create_dir_all(config_dir());
        let _ = fs::write(config_path("ui.json"), json);
    }
}

/// Режимы: порядок, файлы настройки и подписи.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Delete,
    Create,
    Scan,
    Compare,
    Cleanup,
    Copy,
    Duplicates,
}

struct ModeDef {
    mode: Mode,
    title_key: &'static str,
    is_pair: bool,
    config: &'static str,
}

const MODES: [ModeDef; 7] = [
    ModeDef { mode: Mode::Delete,     title_key: "mode.delete",     is_pair: false, config: "folders_to_delete.txt" },
    ModeDef { mode: Mode::Create,     title_key: "mode.create",     is_pair: false, config: "folders_to_create.txt" },
    ModeDef { mode: Mode::Scan,       title_key: "mode.scan",       is_pair: false, config: "folder_to_list.txt" },
    ModeDef { mode: Mode::Compare,    title_key: "mode.compare",    is_pair: true,  config: "folders_to_compare.txt" },
    ModeDef { mode: Mode::Cleanup,    title_key: "mode.cleanup",    is_pair: false, config: "folders_to_cleanup.txt" },
    ModeDef { mode: Mode::Copy,       title_key: "mode.copy",       is_pair: true,  config: "from_to.csv" },
    ModeDef { mode: Mode::Duplicates, title_key: "mode.duplicates", is_pair: false, config: "folders_to_duplicates.txt" },
];

/// Перевод по ключу с учётом глобального языка — для кода вне `App`
/// (потоки операций, свободные функции).
fn tg(key: &'static str) -> &'static str {
    i18n::t(remont_core::i18n::global(), key)
}

/// Перевод с подстановкой `{}` — вариант `tg` для форматированных строк.
fn tfg(key: &'static str, args: &[String]) -> String {
    remont_core::i18n::fill(tg(key), args)
}

/// Служебный ключ режима (для config/schedule.json).
/// Окно «догона», мин: если приложение было закрыто в момент запуска,
/// задание сработает при старте, но не позже чем через полчаса.
const CATCHUP_MIN: u32 = 30;

/// Пора ли выполнять задание: включено, есть выполнения, сегодня ещё не
/// запускалось, время наступило и не вышло за окно «догона».
fn task_due(t: &schedule::Task, today: &str, cur: u32) -> bool {
    t.enabled
        && t.remaining > 0
        && t.last_date != today
        && t.minutes() <= cur
        && cur - t.minutes() <= CATCHUP_MIN
}

/// Задание пропущено на сегодня: включено, есть выполнения, сегодня не
/// запускалось, время наступило И окно «догона» уже прошло (приложение
/// было закрыто в момент запуска либо занято другой операцией).
fn task_missed(t: &schedule::Task, today: &str, cur: u32) -> bool {
    t.enabled
        && t.remaining > 0
        && t.last_date != today
        && t.minutes() <= cur
        && cur - t.minutes() > CATCHUP_MIN
}

/// Первое исчерпанное задание (включено, выполнений не осталось) —
/// индекс того, которое нужно отключить.
fn exhausted_index(tasks: &[schedule::Task]) -> Option<usize> {
    tasks.iter().position(|t| t.remaining == 0 && t.enabled)
}

/// Смена «количества выполнений»: увеличение total добавляется к
/// остатку, уменьшение ужимает остаток до нового total.
/// (Чистая функция — покрыта тестом.)
fn apply_total_change(t: &mut schedule::Task, new_total: u32) {
    let old = t.total;
    t.total = new_total;
    if new_total > old {
        t.remaining = t.remaining.saturating_add(new_total - old);
    } else {
        t.remaining = t.remaining.min(new_total);
    }
}

fn mode_slug(m: Mode) -> &'static str {
    match m {
        Mode::Delete => "delete",
        Mode::Create => "create",
        Mode::Scan => "scan",
        Mode::Compare => "compare",
        Mode::Cleanup => "cleanup",
        Mode::Copy => "copy",
        Mode::Duplicates => "duplicates",
    }
}

/// Режим по служебному ключу.
fn mode_from_key(k: &str) -> Option<Mode> {
    MODES.iter().map(|d| d.mode).find(|m| mode_slug(*m) == k)
}

/// Имя папки данных в «Документах» пользователя.
const APP_DIR: &str = "RemontExplorer";

/// Каталог данных приложения: `Документы\RemontExplorer`.
///
/// Конфиги не должны лежать рядом с exe (папка может быть только для чтения,
/// да и чистить её пользователю неудобно). Переменная окружения
/// `REMONTEXPLORER_DATA_DIR` переопределяет путь — используется тестами.
fn data_dir() -> PathBuf {
    if let Ok(p) = std::env::var("REMONTEXPLORER_DATA_DIR") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    dirs::document_dir().or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from(".")).join(APP_DIR)
}

/// Каталог конфигурации (`<Документы>\RemontExplorer\config`).
fn config_dir() -> PathBuf {
    data_dir().join("config")
}

/// Полный путь к файлу конфигурации по имени.
fn config_path(name: &str) -> PathBuf {
    config_dir().join(name)
}

/// Открыть ссылку в браузере по умолчанию.
fn open_url(url: &str) {
    if url.is_empty() {
        return;
    }
    #[cfg(windows)]
    {
        open_with_shell(Path::new(url), false);
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
}

/// Открыть полученный файл (отчёт) ассоциированным приложением.
///
/// Используется `ShellExecuteW`, а не `cmd /C start`: командная строка
/// преобразует аргументы в ANSI, из-за чего имена с кириллицей (например,
/// `files_info_260885_Новые_материалы_….xlsx`) искажаются и файл «не
/// находится». `ShellExecuteW` работает в Unicode и с абсолютным путём.
fn open_result_file(path: &Path) {
    #[cfg(windows)]
    {
        // Абсолютный путь — чтобы файл нашёлся при любом рабочем каталоге.
        let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        // `canonicalize` возвращает путь с префиксом `\\?\`, а оболочка
        // Windows такие пути не открывает: папка журналов «не открывалась».
        let abs = unverbatim(&abs);
        if abs.exists() {
            open_with_shell(&abs, abs.is_dir());
        }
    }
    #[cfg(not(windows))]
    {
        if path.exists() {
            let _ = std::process::Command::new("open").arg(path).spawn();
        }
    }
}

/// Можно ли создать файл в каталоге (проверка пробным файлом).
fn dir_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".remont_probe_{}", std::process::id()));
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Убрать Windows-префикс длинных путей (`\\?\`, `\\?\UNC\`).
///
/// Оболочка (`ShellExecute`) и Проводник префикс не понимают — с ним не
/// открывается ни файл, ни папка.
#[cfg(windows)]
fn unverbatim(p: &Path) -> std::path::PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return std::path::PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return std::path::PathBuf::from(rest);
    }
    p.to_path_buf()
}

/// Открыть файл, папку или ссылку средствами оболочки Windows (Unicode).
#[cfg(windows)]
fn open_with_shell(target: &Path, is_dir: bool) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    // Для каталога — «explore» (Проводник), иначе — приложение по умолчанию.
    let op = wide(std::ffi::OsStr::new(if is_dir { "explore" } else { "open" }));
    let file = wide(target.as_os_str());
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            op.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// Открыть файл конфигурации в системном редакторе.
/// Если файла ещё нет — создаётся пустой (чтобы пользователь мог заполнить).
fn open_config_file(path: &Path) {
    ensure_config_file(path);
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("notepad").arg(path).spawn();
    }
    #[cfg(not(windows))]
    {
        // Открываем в редакторе по умолчанию (на не-Windows — только для отладки).
        let _ = std::process::Command::new("open").arg("-t").arg(path).spawn();
    }
}

/// Создать файл конфигурации (и его каталог), если их ещё нет.
fn ensure_config_file(path: &Path) {
    if path.exists() {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::write(path, "");
}

/// Визуальное оформление: цвета, отступы, скругления.
///
/// Вынесено из `update`, чтобы цвета можно было проверить тестом: цвет
/// обычного текста здесь нейтральный — акцентный синий делал синими вкладки,
/// кнопки и консоль.
fn tune_style(style: &mut egui::Style) {
    let visuals = &mut style.visuals;
            // Акцентный цвет — профессиональный синий
            // Мягкие тени и скругления
            visuals.panel_fill = if visuals.dark_mode {
                egui::Color32::from_rgb(28, 30, 35)
            } else {
                egui::Color32::from_rgb(245, 247, 250)
            };
            visuals.indent_has_left_vline = true;
            // Кнопки и интерактивные элементы
            visuals.widgets.active.bg_fill = if visuals.dark_mode {
                egui::Color32::from_rgb(50, 55, 65)
            } else {
                egui::Color32::from_rgb(220, 228, 240)
            };
            visuals.widgets.hovered.bg_fill = if visuals.dark_mode {
                egui::Color32::from_rgb(42, 48, 58)
            } else {
                egui::Color32::from_rgb(232, 238, 248)
            };
            visuals.widgets.inactive.bg_fill = if visuals.dark_mode {
                egui::Color32::from_rgb(38, 40, 46)
            } else {
                egui::Color32::from_rgb(248, 249, 251)
            };
            // Обычный текст — нейтральный: акцентный синий здесь делал синими
            // вкладки, кнопки и всю консоль.
            visuals.widgets.inactive.fg_stroke.color = if visuals.dark_mode {
                egui::Color32::from_rgb(200, 205, 215)
            } else {
                egui::Color32::from_rgb(45, 50, 60)
            };
            visuals.widgets.noninteractive.fg_stroke.color = if visuals.dark_mode {
                egui::Color32::from_rgb(200, 205, 215)
            } else {
                egui::Color32::from_rgb(60, 66, 78)
            };
            // Текст выбранного пункта — на фоне выделения, без цветного
            // оттенка (в тёмной теме синий на синем не читался).
            visuals.selection.stroke = if visuals.dark_mode {
                egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(240, 243, 248))
            } else {
                egui::Stroke::new(1.0_f32, egui::Color32::WHITE)
            };
            // Группы (границы)
            visuals.widgets.open.bg_fill = if visuals.dark_mode {
                egui::Color32::from_rgb(35, 37, 43)
            } else {
                egui::Color32::from_rgb(250, 251, 253)
            };
            visuals.extreme_bg_color = if visuals.dark_mode {
                egui::Color32::from_rgb(20, 22, 27)
            } else {
                egui::Color32::from_rgb(238, 242, 248)
            };
}

fn theme_preference(mode: ThemeMode) -> egui::ThemePreference {
    match mode {
        ThemeMode::Light => egui::ThemePreference::Light,
        ThemeMode::Dark => egui::ThemePreference::Dark,
        ThemeMode::System => egui::ThemePreference::System,
    }
}

/// Передать ядру выбранную папку результатов (пусто — рядом с приложением).
fn apply_output_dir(dir: &str) {
    let trimmed = dir.trim();
    if trimmed.is_empty() {
        remont_core::set_output_dir(None);
    } else {
        remont_core::set_output_dir(Some(PathBuf::from(trimmed)));
    }
}

fn mode_def(m: Mode) -> &'static ModeDef {
    MODES.iter().find(|d| d.mode == m).unwrap()
}

fn mode_key(m: Mode) -> &'static str {
    match m {
        Mode::Delete => "mode.delete",
        Mode::Create => "mode.create",
        Mode::Scan => "mode.scan",
        Mode::Compare => "mode.compare",
        Mode::Cleanup => "mode.cleanup",
        Mode::Copy => "mode.copy",
        Mode::Duplicates => "mode.duplicates",
    }
}

fn desc_key(m: Mode) -> &'static str {
    match m {
        Mode::Delete => "desc.delete",
        Mode::Create => "desc.create",
        Mode::Scan => "desc.scan",
        Mode::Compare => "desc.compare",
        Mode::Cleanup => "desc.cleanup",
        Mode::Copy => "desc.copy",
        Mode::Duplicates => "desc.duplicates",
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TopTab {
    Work,
    Settings,
    Schedule,
    History,
    Help,
}

/// Сообщение фоновой загрузки обновления.
enum DlMsg {
    Progress(u64, u64),
    Done(PathBuf),
    Failed(String),
}

/// Метаданные запущенной операции — попадают в историю выполнений.
#[derive(Clone, Debug, Default)]
struct OpMeta {
    /// Машинный код режима: scan, copy, delete, ...
    mode: String,
    /// Название на языке интерфейса.
    title: String,
    /// Входные пути.
    paths: Vec<String>,
    /// Откуда запущено: gui, schedule, context-menu.
    source: String,
}

/// Модальные вопросы и диалоги выбора.
enum Pending {
    Confirm {
        title: String,
        prompt: String,
        kind: ConfirmKind,
    },
    CompareMethod {
        selected: usize,
    },
    DupAlgo {
        paths: Vec<PathBuf>,
    },
    /// Окно «О приложении».
    About,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ConfirmKind {
    DeleteRecursive,
    CreateZip,
    CopyResume,
    SavePairMismatch,
    /// Очистка истории выполнений.
    ClearHistory,
}

struct App {
    top_tab: TopTab,
    mode: Option<Mode>,
    running: bool,
    stop: StopToken,
    log_rx: Option<mpsc::Receiver<String>>,
    prog_rx: Option<mpsc::Receiver<(usize, usize)>>,
    /// Текущий прогресс операции (current, total); total == 0 — неопределённый.
    progress: Option<(usize, usize)>,
    logs: VecDeque<String>,
    pending: Option<Pending>,
    /// Предупреждение о недоступности папки приложения (недоступна для записи).
    startup_warning: Option<String>,

    // Интерфейс (тема, язык)
    ui_config: UiConfig,
    /// Предыдущая тема — для применения только при изменении.
    applied_theme: ThemeMode,

    // Настройки
    settings_sel: usize,
    settings_loaded: usize,
    editor_single: String,
    editor_left: String,
    editor_right: String,
    /// Потоки сканирования (Настройки → Сканирование, config/scan_threads.txt).
    scan_threads: usize,

    // Справка
    help_sel: usize,

    // Планирование
    sched: schedule::Schedule,
    sched_sel: usize,
    /// Метка последней проверки расписания (раз в секунду).
    sched_last_tick: Option<std::time::Instant>,
    /// Метка последнего уведомления «занято» — кулдаун ~1 минута.
    sched_busy_last: Option<std::time::Instant>,
    /// (id задания, дата) — уже сообщено, что задание пропущено в этот день.
    sched_missed_notified: Vec<(u64, String)>,

    // Интеграция с Windows
    /// Состояние пункта контекстного меню (None — ещё не проверялось).
    ctx_menu: Option<bool>,
    /// Состояние автозапуска (None — ещё не проверялось).
    autostart: Option<bool>,
    /// Значок в системном трее (создаётся, когда включено сворачивание).
    tray: Option<tray::TrayState>,
    /// Окно скрыто в трей.
    hidden_to_tray: bool,
    /// Пользователь запросил полный выход (из меню трея).
    exit_requested: bool,
    /// Папка из командной строки (`--scan "D:\Папка"`).
    startup_scan: Option<PathBuf>,
    /// Запуск свёрнутым в трей (`--minimized`, автозапуск).
    start_hidden: bool,
    /// Вкладка, открытая при старте (`--tab ...`).
    startup_tab: Option<TopTab>,
    /// Стартовые действия выполнены (один раз).
    startup_done: bool,
    /// Найденное обновление (версия новее текущей).
    update_info: Option<updates::UpdateInfo>,
    /// Метаданные текущей операции (для журнала истории).
    op_meta: Option<OpMeta>,
    /// Начало текущей операции.
    op_started: Option<std::time::Instant>,
    /// Прочитанная история выполнений (вкладка «История»).
    history: Vec<remont_core::HistoryEntry>,
    /// История уже читалась с диска.
    history_loaded: bool,
    /// Последняя сводка истории.
    history_summary: remont_core::history::Summary,
    /// Пути сравниваемых отчётов (режим «Сравнение» → «Сравнить отчёты»).
    diff_old: String,
    diff_new: String,
    /// Текст ошибки проверки обновлений (показывается в «О приложении»).
    update_error: Option<String>,
    /// Проверка обновлений уже выполнялась/выполняется.
    update_done: bool,
    /// Канал результата проверки обновлений.
    update_rx: Option<mpsc::Receiver<Result<Option<updates::UpdateInfo>, String>>>,
    /// Идёт фоновая загрузка архива обновления.
    update_downloading: bool,
    /// Прогресс загрузки (получено, всего; всего = 0 — неизвестно).
    update_progress: Option<(u64, u64)>,
    /// Скачанный и распакованный новый exe (готов к установке).
    update_exe: Option<PathBuf>,
    /// Ошибка загрузки/установки обновления.
    update_download_error: Option<String>,
    /// Канал фоновой загрузки.
    update_dl_rx: Option<mpsc::Receiver<DlMsg>>,
    /// Обновление готово, но ждёт окончания текущей операции.
    install_after_op: bool,
    /// Дескриптор окна Windows (0 — ещё не получен). Нужен, чтобы трей мог
    /// показать окно, когда egui не рисует кадры.
    hwnd: std::sync::Arc<AtomicIsize>,
}

impl Default for App {
    fn default() -> Self {
        let ui_config = load_ui_config();
        Self {
            top_tab: TopTab::Work,
            mode: None,
            running: false,
            stop: StopToken::new(),
            log_rx: None,
            prog_rx: None,
            progress: None,
            logs: VecDeque::new(),
            pending: None,
            startup_warning: None,
            applied_theme: ui_config.theme,
            ui_config,
            settings_sel: 0,
            settings_loaded: usize::MAX,
            editor_single: String::new(),
            editor_left: String::new(),
            editor_right: String::new(),
            scan_threads: load_scan_threads(),
            help_sel: 0,
            sched: schedule::Schedule::load(),
            sched_sel: 0,
            sched_last_tick: None,
            sched_busy_last: None,
            sched_missed_notified: Vec::new(),
            ctx_menu: None,
            autostart: None,
            tray: None,
            hidden_to_tray: false,
            exit_requested: false,
            startup_scan: None,
            start_hidden: false,
            startup_tab: None,
            startup_done: false,
            op_meta: None,
            op_started: None,
            history: Vec::new(),
            history_loaded: false,
            history_summary: Default::default(),
            diff_old: String::new(),
            diff_new: String::new(),
            update_info: None,
            update_error: None,
            update_done: false,
            update_rx: None,
            update_downloading: false,
            update_progress: None,
            update_exe: None,
            update_download_error: None,
            update_dl_rx: None,
            install_after_op: false,
            hwnd: std::sync::Arc::new(AtomicIsize::new(0)),
        }
    }
}

/// Потоки сканирования: config/scan_threads.txt (1..=256), иначе число ядер.
fn load_scan_threads() -> usize {
    let def = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    fs::read_to_string(config_path("scan_threads.txt")).ok().and_then(|s| s.trim().parse::<usize>().ok()).filter(|n| (1..=256).contains(n)).unwrap_or(def)
}

impl App {


    /// Запустить проверку обновлений в фоне (сеть не должна блокировать UI).
    fn start_update_check(&mut self) {
        if self.update_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.update_rx = Some(rx);
        thread::spawn(move || {
            let res = updates::check(env!("CARGO_PKG_VERSION"));
            let _ = tx.send(res);
        });
    }

    fn poll_update(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.update_rx else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(info)) => {
                let found = info.is_some();
                self.update_info = info;
                self.update_error = None;
                self.update_done = true;
                self.update_rx = None;
                // Обновление найдено — сразу тянем архив в фоне, чтобы к
                // моменту решения пользователя он уже лежал на диске.
                if found && (self.ui_config.auto_download || self.ui_config.auto_install) {
                    self.start_update_download();
                }
                ctx.request_repaint();
            }
            Ok(Err(e)) => {
                self.update_error = Some(e);
                self.update_done = true;
                self.update_rx = None;
                ctx.request_repaint();
            }
            Err(mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(400));
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.update_done = true;
                self.update_rx = None;
            }
        }
    }

    /// Создать/убрать значок в трее в соответствии с настройкой.
    fn ensure_tray(&mut self, ctx: &egui::Context) {
        // Трей реализован только для Windows: на других ОС настройку не трогаем.
        if !cfg!(windows) {
            return;
        }
        let want = self.ui_config.close_to_tray;
        if want && self.tray.is_none() {
            let open = self.t("tray.open").to_string();
            let exit = self.t("tray.exit").to_string();
            match tray::create(
                app_png(),
                "RemontExplorer",
                &open,
                &exit,
                std::sync::Arc::clone(&self.hwnd),
                ctx.clone(),
            ) {
                Ok(t) => self.tray = Some(t),
                Err(e) => {
                    // Не Windows или нет прав — выключаем настройку, чтобы
                    // не повторять попытку и не засорять журнал.
                    let msg = self.tf("log.win_error", &[e]);
                    self.ui_config.close_to_tray = false;
                    self.push_log(&msg);
                }
            }
        } else if !want && self.tray.is_some() {
            self.tray = None;
        }
    }
    /// Сканирование папки, переданной в командной строке.
    fn scan_external(&mut self, dir: PathBuf) {
        let threads = self.scan_threads;
        self.mode = Some(Mode::Scan);
        self.push_log(&self.tf("log.external_scan", &[dir.display().to_string()]));
        let meta = OpMeta {
            mode: "scan".into(),
            title: self.t("mode.scan").to_string(),
            paths: vec![dir.display().to_string()],
            source: "context-menu".into(),
        };
        self.spawn_op_meta(meta, move |log, stop| {
            match scan_folder(&dir, None, threads, &stop, &log) {
                Some((out, stats)) => emit(
                    &log,
                    tfg(
                        "log.scan_result",
                        &[stats.files.to_string(), out.display().to_string()],
                    ),
                ),
                None => emit(&log, tg("log.scan_failed")),
            }
        });
    }

    /// Действия при старте: скрыть окно (автозапуск) и/или сканировать папку.
    fn startup_actions(&mut self) {
        if self.startup_done {
            return;
        }
        self.startup_done = true;
        if let Some(tab) = self.startup_tab.take() {
            self.top_tab = tab;
        }
        if let Some(dir) = self.startup_scan.take() {
            self.scan_external(dir);
        }
        if self.start_hidden && self.tray.is_some() {
            self.hidden_to_tray = true;
            tray::hide_window(self.hwnd.load(Ordering::Relaxed));
        }
    }

    // Модальное окно поверх всего остального: компактное центрированное
    // окно с затемнением фона (не на всю ширину экрана, а по содержимому).
    /// Возвращает `true`, если диалог показан и кадр нужно прервать.
    fn pending_modal(&mut self, ctx: &egui::Context) -> bool {
        let mut pending = self.pending.take();
        let Some(p) = &mut pending else {
            return false;
        };
            let mut handled = false;
            // Окно центрируется через anchor; высота — по содержимому.
            // ВАЖНО: внутри авторазмерного Window нельзя использовать
            // вертикальное центрирование (vertical_centered и т.п.): они
            // запрашивают всю доступную высоту и раздувают окно на весь экран.
            // Для центрирования по горизонтали — только centered_row().
            egui::Window::new(egui::RichText::new("")).id(egui::Id::new("fm_modal")).title_bar(false).resizable(false).collapsible(false).anchor(egui::Align2::CENTER_CENTER, (0.0, 0.0)).min_size(egui::vec2(420.0, 0.0)).show(ctx, |ui| {
                    ui.set_min_width(400.0);
                    ui.set_max_width(460.0);
                    ui.add_space(12.0);
                    match p {
                        Pending::Confirm { title, prompt, kind } => {
                            centered_row(ui, |ui| {
                                ui.label(RichText::new(title.as_str()).strong().size(16.0));
                            });
                            ui.add_space(8.0);
                            ui.add(egui::Label::new(prompt.as_str()).wrap());
                            ui.add_space(14.0);
                            centered_row(ui, |ui| {
                                if ui.button(self.t("btn.yes")).clicked() {
                                    handled = true;
                                    self.pending = None;
                                    self.confirm_yes(*kind);
                                }
                                if ui.button(self.t("btn.no")).clicked() {
                                    handled = true;
                                    self.pending = None;
                                    self.confirm_no(*kind);
                                }
                            });
                        }
                        Pending::CompareMethod { selected } => {
                            centered_row(ui, |ui| {
                                ui.label(
                                    RichText::new(self.t("dlg.compare_title")).strong().size(16.0),
                                );
                            });
                            ui.add_space(12.0);
                            let methods = [
                                self.t("cmp.size_date"),
                                self.t("cmp.binary"),
                                self.t("cmp.sha256"),
                                self.t("cmp.md5"),
                            ];
                            for (i, m) in methods.iter().enumerate() {
                                ui.radio_value(selected, i, *m);
                            }
                            ui.add_space(14.0);
                            centered_row(ui, |ui| {
                                if ui.button(self.t("btn.ok")).clicked() {
                                    handled = true;
                                    self.pending = None;
                                    let method = match *selected {
                                        1 => CompareMethod::Binary,
                                        2 => CompareMethod::Hash(HashAlgo::Sha256),
                                        3 => CompareMethod::Hash(HashAlgo::Md5),
                                        _ => CompareMethod::SizeDate,
                                    };
                                    let cfg = PathBuf::from("config/folders_to_compare.txt");
                                    self.spawn_op(move |log, stop| {
                                        if let Err(e) = compare_folders(&cfg, method, None, &stop, &log)
                                        {
                                            emit(&log, tfg("log.error", &[e.to_string()]));
                                        }
                                    });
                                }
                                if ui.button(self.t("btn.cancel")).clicked() {
                                    handled = true;
                                    self.pending = None;
                                    self.push_log(self.t("log.compare_cancelled"));
                                }
                            });
                        }
                        Pending::About => {
                            centered_row(ui, |ui| {
                                ui.label(RichText::new("RemontExplorer").strong().size(20.0));
                            });
                            ui.add_space(6.0);
                            centered_row(ui, |ui| {
                                ui.label(
                                    RichText::new(self.tf(
                                        "about.version",
                                        &[env!("CARGO_PKG_VERSION").to_string()],
                                    )).weak(),
                                );
                            });
                            ui.add_space(12.0);
                            ui.add(egui::Label::new(self.t("about.desc")).wrap());
                            ui.add_space(10.0);
                            ui.label(RichText::new(self.t("about.author")).strong());
                            ui.add_space(10.0);

                            // ---- Проверка и установка обновлений ----
                            let upd_version = self.update_info.as_ref().map(|i| i.version.clone());
                            let upd_page = self.update_info.as_ref().map(|i| i.url.clone()).unwrap_or_default();
                            let has_asset = self
                                .update_info
                                .as_ref()
                                .map(|i| i.asset_url.is_some())
                                .unwrap_or(false);

                            if let Some(version) = upd_version {
                                let text = self.tf("upd.available", std::slice::from_ref(&version));
                                ui.label(
                                    RichText::new(text).strong().color(egui::Color32::from_rgb(200, 120, 0)),
                                );
                                ui.add_space(6.0);

                                if self.update_exe.is_some() {
                                    // Архив скачан и распакован — предлагаем установку.
                                    let ready = self.tf("upd.ready", std::slice::from_ref(&version));
                                    ui.label(RichText::new(ready).color(egui::Color32::from_rgb(40, 140, 70)));
                                    ui.add_space(6.0);
                                    if ui
                                        .add(egui::Button::new(RichText::new(self.t("upd.install")).strong()))
                                        .clicked()
                                    {
                                        self.apply_update();
                                    }
                                    ui.add_space(4.0);
                                    ui.label(RichText::new(self.t("upd.install_hint")).weak());
                                } else if self.update_downloading {
                                    let (got, total) = self.update_progress.unwrap_or((0, 0));
                                    if total > 0 {
                                        let pct = (got as f64 / total as f64 * 100.0).round() as u64;
                                        ui.add(
                                            egui::ProgressBar::new((got as f32 / total as f32).clamp(0.0, 1.0))
                                                .text(self.tf("upd.downloading", &[pct.to_string()])),
                                        );
                                    } else {
                                        ui.add(egui::ProgressBar::new(0.0).animate(true).text(self.t("upd.downloading_unknown")));
                                    }
                                } else if let Some(err) = self.update_download_error.clone() {
                                    ui.label(
                                        RichText::new(self.tf("upd.dl_failed", std::slice::from_ref(&err)))
                                            .color(egui::Color32::from_rgb(180, 60, 60)),
                                    );
                                    ui.add_space(4.0);
                                    if has_asset && ui.button(self.t("upd.retry")).clicked() {
                                        self.start_update_download();
                                    }
                                } else if has_asset {
                                    if ui.button(self.t("upd.download")).clicked() {
                                        self.start_update_download();
                                    }
                                } else if ui.button(self.t("upd.download")).clicked() {
                                    // Atom-фид: прямой ссылки нет — открываем страницу релиза.
                                    open_url(&upd_page);
                                }
                            } else if self.update_rx.is_some() {
                                ui.label(RichText::new(self.t("upd.checking")).weak());
                            } else if let Some(err) = &self.update_error {
                                ui.label(
                                    RichText::new(self.tf("upd.error", std::slice::from_ref(err))).weak().color(egui::Color32::from_rgb(180, 60, 60)),
                                );
                            } else if self.update_done {
                                ui.label(RichText::new(self.t("upd.latest")).weak());
                            }

                            ui.add_space(12.0);
                            centered_row(ui, |ui| {
                                if ui.button(self.t("btn.check_updates")).clicked() {
                                    self.update_info = None;
                                    self.update_error = None;
                                    self.update_done = false;
                                    self.start_update_check();
                                }
                                if ui.button(self.t("about.close")).clicked() {
                                    handled = true;
                                    self.pending = None;
                                }
                            });
                        }
                        Pending::DupAlgo { paths } => {
                            centered_row(ui, |ui| {
                                ui.label(
                                    RichText::new(self.t("dlg.dup_title")).strong().size(16.0),
                                );
                            });
                            ui.add_space(14.0);
                            centered_row(ui, |ui| {
                                if ui.button("SHA-256").clicked() {
                                    handled = true;
                                    self.pending = None;
                                    self.run_duplicates(paths.clone(), HashAlgo::Sha256);
                                }
                                if ui.button("MD5").clicked() {
                                    handled = true;
                                    self.pending = None;
                                    self.run_duplicates(paths.clone(), HashAlgo::Md5);
                                }
                                if ui.button(self.t("btn.cancel")).clicked() {
                                    handled = true;
                                    self.pending = None;
                                    self.push_log(self.t("log.dup_cancelled"));
                                }
                            });
                        }
                    }
                    ui.add_space(12.0);
                });
            // Если действие не выбрано — окно остаётся открытым
        if !handled {
            self.pending = pending;
        }
        true
    }

    /// Короткий псевдоним для перевода GUI-строки текущим языком.
    fn t(&self, key: &'static str) -> &'static str {
        i18n::t(self.ui_config.lang, key)
    }

    /// Перевод с подстановкой значений вместо `{}`.
    fn tf(&self, key: &'static str, args: &[String]) -> String {
        remont_core::i18n::fill(self.t(key), args)
    }

    /// Запуск операции в отдельном потоке (журнал и прогресс — через каналы).
    /// Паника в операции перехватывается: приложение живёт, в консоль
    /// выводится текст внутренней ошибки.
    fn spawn_op(&mut self, f: impl FnOnce(LogSink, StopToken) + Send + 'static) {
        let meta = self.current_meta();
        self.spawn_op_meta(meta, f);
    }

    /// Метаданные операции по выбранному режиму и его файлу настройки.
    fn current_meta(&self) -> OpMeta {
        let Some(m) = self.mode else {
            return OpMeta::default();
        };
        let def = mode_def(m);
        let paths = read_paths_file(&config_path(def.config))
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        OpMeta {
            mode: match m {
                Mode::Delete => "delete",
                Mode::Create => "create",
                Mode::Scan => "scan",
                Mode::Compare => "compare",
                Mode::Cleanup => "cleanup",
                Mode::Copy => "copy",
                Mode::Duplicates => "duplicates",
            }
            .into(),
            title: self.t(mode_key(m)).to_string(),
            paths,
            source: "gui".into(),
        }
    }

    /// Запуск операции с явными метаданными (расписание, контекстное меню).
    fn spawn_op_meta(
        &mut self,
        meta: OpMeta,
        f: impl FnOnce(LogSink, StopToken) + Send + 'static,
    ) {
        if self.running {
            return;
        }
        self.op_meta = Some(meta);
        self.op_started = Some(std::time::Instant::now());
        let (tx, rx) = mpsc::channel::<String>();
        let (txp, rxp) = mpsc::channel::<(usize, usize)>();
        let tx_done = tx.clone();
        let lang = self.ui_config.lang;
        let log: LogSink = Arc::new(Mutex::new(
            Sink::with_progress(
                move |msg: String| {
                    let _ = tx.send(msg);
                },
                move |c: usize, t: usize| {
                    let _ = txp.send((c, t));
                },
            ).with_lang(lang),
        ));
        // Глобальный язык для Display-ошибок core.
        set_global(lang);
        let stop = self.stop.clone();
        // Сброс стоп-флага: предыдущая операция могла быть остановлена
        // пользователем, иначе новая стартовала бы уже со старым сигналом.
        self.stop.reset();
        self.log_rx = Some(rx);
        self.prog_rx = Some(rxp);
        self.progress = None;
        self.running = true;
        remont_core::clear_last_report();
        let h = thread::spawn(move || {
            let log2 = log.clone();
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(log, stop)));
            if let Err(e) = r {
                let msg = e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_else(|| "unknown reason".into());
                emit(&log2, tfg("log.internal_error", &[msg]));
            }
        });
        thread::spawn(move || {
            let _ = h.join();
            let _ = tx_done.send(DONE.to_string());
        });
    }

    fn push_log(&mut self, msg: &str) {
        self.logs.push_back(msg.to_string());
    }

    fn poll_logs(&mut self) {
        if let Some(prx) = &self.prog_rx {
            while let Ok((c, t)) = prx.try_recv() {
                self.progress = Some((c, t));
            }
        }
        if let Some(rx) = &self.log_rx {
            let mut done = false;
            loop {
                match rx.try_recv() {
                    Ok(m) if m == DONE => {
                        done = true;
                        break;
                    }
                    Ok(m) => self.logs.push_back(m),
                    Err(_) => break,
                }
            }
            if done {
                self.running = false;
                self.log_rx = None;
                self.prog_rx = None;
                self.progress = None;
                self.finish_op();
                // Настройка «открывать полученный файл после выполнения».
                if self.ui_config.open_result {
                    if let Some(path) = remont_core::last_report() {
                        open_result_file(&path);
                    }
                }
            }
        }
        while self.logs.len() > MAX_LOG {
            self.logs.pop_front();
        }
    }

    /// Операция завершилась: пишем историю и (если включено) уведомление.
    fn finish_op(&mut self) {
        let Some(meta) = self.op_meta.take() else {
            return;
        };
        if meta.mode.is_empty() {
            return;
        }
        let seconds = self
            .op_started
            .take()
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        let errors = self.logs.iter().filter(|l| logfmt::is_error(l)).count();
        let outcome = if self.stop.is_set() {
            remont_core::Outcome::Aborted
        } else if errors > 0 {
            remont_core::Outcome::Failed
        } else {
            remont_core::Outcome::Ok
        };

        let mut entry = remont_core::HistoryEntry::new(meta.mode.clone(), meta.title.clone());
        entry.paths = meta.paths.clone();
        entry.errors = errors;
        entry.seconds = seconds;
        entry.report = remont_core::last_report().map(|p| p.display().to_string());
        entry.outcome = outcome;
        entry.source = meta.source.clone();
        entry.details = self.logs.back().cloned().unwrap_or_default();
        let _ = entry.record();

        // Уведомление Windows — не должно мешать, ошибки игнорируются.
        if self.ui_config.notify {
            let key = match outcome {
                remont_core::Outcome::Ok => "notify.done",
                remont_core::Outcome::Failed => "notify.error",
                remont_core::Outcome::Aborted => "notify.aborted",
            };
            let body = self.tf(key, std::slice::from_ref(&meta.title));
            let title = self.t("notify.title").to_string();
            notify::notify(
                self.hwnd.load(Ordering::Relaxed),
                &title,
                &body,
                outcome != remont_core::Outcome::Ok,
            );
        }

        self.reload_history();

        // Отложенная автоустановка: не обрываем выполняющуюся операцию.
        if self.install_after_op && !self.running {
            self.install_after_op = false;
            self.apply_update();
        }
    }

    /// Перечитать историю с диска (вкладка «История»).
    fn reload_history(&mut self) {
        self.history = remont_core::history::read_all();
        self.history_summary = remont_core::history::summarize(&self.history);
    }

    // ==================== ВКЛАДКА «РАБОТА» ====================

    fn work_tab(&mut self, ui: &mut egui::Ui) {
        // Цвета режимов для визуальной индикации
        let mode_colors = [
            egui::Color32::from_rgb(220, 53, 69),    // Удаление — красный
            egui::Color32::from_rgb(40, 167, 69),    // Создание — зелёный
            egui::Color32::from_rgb(33, 100, 220),   // Сканирование — синий
            egui::Color32::from_rgb(255, 159, 10),   // Сравнение — оранжевый
            egui::Color32::from_rgb(108, 117, 125),  // Очистка — серый
            egui::Color32::from_rgb(0, 123, 167),    // Копирование — бирюзовый
            egui::Color32::from_rgb(154, 52, 185),   // Дубликаты — фиолетовый
        ];
        let mode_names: [(&'static str, &'static str); 7] = [
            ("mode.delete", "🗑"),
            ("mode.create", "📁"),
            ("mode.scan", "🔍"),
            ("mode.compare", "⚖"),
            ("mode.cleanup", "♻"),
            ("mode.copy", "📋"),
            ("mode.duplicates", "🔎"),
        ];

        // Перенос вместо обрезки: при узком окне «Дубликаты» уезжали за край.
        ui.horizontal_wrapped(|ui| {
            for (i, def) in MODES.iter().enumerate() {
                let (key, icon) = mode_names[i];
                let label = self.t(key);
                let color = mode_colors[i];
                let is_active = self.mode == Some(def.mode);
                if self.running {
                    ui.label(
                        RichText::new(format!("{} {}", icon, label))
                            .size(13.0)
                            .color(if is_active { color } else { color.linear_multiply(0.4) })
                            .weak(),
                    );
                } else {
                    let btn = egui::Button::new(format!("{} {}", icon, label))
                        .fill(if is_active { color.linear_multiply(0.15) } else { egui::Color32::TRANSPARENT })
                        .stroke(egui::Stroke::new(1.0_f32, if is_active { color } else { egui::Color32::TRANSPARENT }))
                        .rounding(egui::Rounding::same(4.0));
                    if ui.add(btn).clicked() {
                        self.mode = Some(def.mode);
                    }
                }
            }
            ui.separator();
            // «Старт» активна, когда выбран режим и операция не выполняется.
            let start_enabled = !self.running && self.mode.is_some();
            if ui.add_enabled(
                    start_enabled,
                    egui::Button::new(
                        RichText::new(self.t("btn.start")).strong().color(egui::Color32::WHITE),
                    ).fill(egui::Color32::from_rgb(40, 167, 69)).rounding(egui::Rounding::same(4.0)),
                ).clicked()
            {
                self.start_action();
            }
            // «Остановить»: ярко-красная, когда её можно нажать (идёт операция).
            let stop_enabled = self.running;
            let stop_btn = if stop_enabled {
                egui::Button::new(
                    RichText::new(self.t("btn.stop")).strong().color(egui::Color32::WHITE),
                ).fill(egui::Color32::from_rgb(225, 20, 20)).rounding(egui::Rounding::same(4.0))
            } else {
                egui::Button::new(self.t("btn.stop"))
            };
            if ui.add_enabled(stop_enabled, stop_btn).clicked() {
                self.stop.stop();
                self.push_log(self.t("lbl.stopped_log"));
            }
        });

        if let Some(m) = self.mode {
            let ci = match m {
                Mode::Delete => 0, Mode::Create => 1, Mode::Scan => 2,
                Mode::Compare => 3, Mode::Cleanup => 4, Mode::Copy => 5, Mode::Duplicates => 6,
            };
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("● ").color(mode_colors[ci]).size(10.0),
                );
                ui.add(egui::Label::new(RichText::new(self.t(desc_key(m))).weak()).wrap());
            });
        }
        // Сравнение отчётов — дополнительный инструмент режима «Сравнение».
        if self.mode == Some(Mode::Compare) {
            ui.add_space(4.0);
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(self.t("cmp.reports"))
                            .strong()
                            .color(egui::Color32::from_rgb(255, 159, 10)),
                    );
                    ui.label(RichText::new(self.t("cmp.reports_hint")).weak());
                });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(self.t("cmp.old"));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.diff_old)
                            .desired_width(320.0)
                            .hint_text("*.csv / *.xlsx"),
                    );
                    if ui.button(self.t("cmp.pick_old")).clicked() {
                        if let Some(p) = rfd::FileDialog::new()
                            .set_title(self.t("cmp.pick_old"))
                            .add_filter("CSV/XLSX", &["csv", "xlsx"])
                            .pick_file()
                        {
                            self.diff_old = p.display().to_string();
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(self.t("cmp.new"));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.diff_new)
                            .desired_width(320.0)
                            .hint_text("*.csv / *.xlsx"),
                    );
                    if ui.button(self.t("cmp.pick_new")).clicked() {
                        if let Some(p) = rfd::FileDialog::new()
                            .set_title(self.t("cmp.pick_new"))
                            .add_filter("CSV/XLSX", &["csv", "xlsx"])
                            .pick_file()
                        {
                            self.diff_new = p.display().to_string();
                        }
                    }
                    let ready = !self.running && !self.diff_old.trim().is_empty() && !self.diff_new.trim().is_empty();
                    if ui.add_enabled(ready, egui::Button::new(self.t("cmp.run"))).clicked() {
                        self.start_diff();
                    }
                });
            });
        }

        // Прогресс-бар активной операции
        if self.running {
            let frac = match self.progress {
                Some((c, t)) if t > 0 => (c as f32 / t as f32).clamp(0.0, 1.0),
                _ => {
                    // Неопределённый: плавная «бегущая» шкала
                    let t = ui.ctx().input(|i| i.time) as f32;
                    let t = t.rem_euclid(1.6);
                    0.5 - 0.5 * (t / 1.6 * std::f32::consts::PI * 2.0).cos()
                }
            };
            let label = match self.progress {
                Some((c, t)) if t > 0 => format!("{c} / {t}"),
                _ => self.t("lbl.executing").to_string(),
            };
            let prog = egui::ProgressBar::new(frac)
                .text(label)
                .animate(true)
                .show_percentage();
            // Кастомный цвет прогресс-бара (стиль меняется на кадр и
            // возвращается — иначе он «протекает» в следующие элементы).
            let prev_stroke = ui.ctx().style().visuals.widgets.active.fg_stroke;
            ui.ctx().style_mut(|st| {
                st.visuals.widgets.active.fg_stroke =
                    egui::Stroke::new(10.0_f32, egui::Color32::from_rgb(33, 100, 220));
            });
            ui.add(prog);
            ui.ctx().style_mut(|st| {
                st.visuals.widgets.active.fg_stroke = prev_stroke;
            });
        }
        ui.add_space(2.0);

        let console_bg = if ui.visuals().dark_mode {
            egui::Color32::from_rgb(34, 36, 42)
        } else {
            egui::Color32::from_rgb(248, 249, 252)
        };
        egui::Frame::default()
            .fill(console_bg)
            .stroke(egui::Stroke::NONE)
            .rounding(egui::Rounding::same(6.0))
            .inner_margin(egui::Margin::same(8.0))
            .show(ui, |ui| {
            ui.set_height(ui.available_height().max(100.0));
            if let Some(w) = &self.startup_warning {
                ui.add(
                    egui::Label::new(RichText::new(w.as_str()).color(egui::Color32::from_rgb(200, 60, 0))).wrap(),
                );
                ui.add_space(4.0);
            }
            ui.label(RichText::new(self.t("lbl.console")));
            ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
                    if self.logs.is_empty() {
                        ui.label(
                            RichText::new(self.t("lbl.console_empty")).weak(),
                        );
                    }
                    for line in &self.logs {
                        let color = logfmt::line_color(line, ui.style().visuals.text_color());
                        ui.label(RichText::new(line).monospace().size(12.0).color(color));
                    }
                });
        });
    }

    /// Чтение списка путей с понятным сообщением при пустом/отсутствующем
    /// файле настройки (вместо общего «список пуст»).
    fn check_paths(&mut self, def: &ModeDef) -> Option<Vec<PathBuf>> {
        let cfg_buf = config_path(def.config);
        let cfg = cfg_buf.as_path();
        let paths = read_paths_file(cfg);
        if !paths.is_empty() {
            return Some(paths);
        }
        if cfg.exists() {
            self.push_log(self.t("lbl.list_empty"));
        } else {
            self.push_log(&self.tf("lbl.config_missing", &[config_path(def.config).display().to_string()]));
        }
        None
    }

    /// Запуск операции по кнопке «Старт».
    /// Скачать архив обновления в фоне.
    fn start_update_download(&mut self) {
        if self.update_downloading {
            return;
        }
        let Some(info) = self.update_info.clone() else {
            return;
        };
        // Если API закрыт и пришёл Atom-фид, прямой ссылки нет — строим её
        // по шаблону: у релиза всегда один архив с постоянным именем.
        let url = info
            .asset_url
            .clone()
            .unwrap_or_else(|| updates::asset_url_for(&info.version));
        self.update_downloading = true;
        self.update_download_error = None;
        self.update_progress = None;

        let (tx, rx) = mpsc::channel::<DlMsg>();
        self.update_dl_rx = Some(rx);
        thread::spawn(move || {
            let dir = selfupdate::staging_dir();
            let zip = dir.join("update.zip");
            let txp = tx.clone();
            let res = selfupdate::download(&url, &zip, move |got, total| {
                let _ = txp.send(DlMsg::Progress(got, total));
            })
            .and_then(|()| selfupdate::unpack(&zip, &dir.join("unpacked")));
            match res {
                Ok(exe) => {
                    let _ = tx.send(DlMsg::Done(exe));
                }
                Err(e) => {
                    let _ = tx.send(DlMsg::Failed(e));
                }
            }
        });
    }

    /// Забрать состояние фоновой загрузки.
    fn poll_update_download(&mut self, ctx: &egui::Context) {
        // Сообщения собираются заранее: держать заимствование канала и
        // одновременно менять поля `self` нельзя.
        let mut messages: Vec<DlMsg> = Vec::new();
        let mut disconnected = false;
        if let Some(rx) = &self.update_dl_rx {
            loop {
                match rx.try_recv() {
                    Ok(m) => messages.push(m),
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                }
            }
        }
        for m in messages {
            match m {
                DlMsg::Progress(got, total) => {
                    self.update_progress = Some((got, total));
                    ctx.request_repaint();
                }
                DlMsg::Done(exe) => {
                    self.update_downloading = false;
                    self.update_exe = Some(exe);
                    self.update_dl_rx = None;
                    self.update_progress = None;
                    ctx.request_repaint();
                    // Настройка «автоматическая загрузка и установка»:
                    // ставим сразу, не дожидаясь нажатия кнопки.
                    if self.ui_config.auto_install && !self.running {
                        self.apply_update();
                    } else if self.ui_config.auto_install {
                        self.install_after_op = true;
                    }
                }
                DlMsg::Failed(e) => {
                    self.update_downloading = false;
                    self.update_download_error = Some(e);
                    self.update_dl_rx = None;
                    self.update_progress = None;
                    ctx.request_repaint();
                }
            }
        }
        if disconnected {
            self.update_downloading = false;
            self.update_dl_rx = None;
        }
    }

    /// Заменить приложение и перезапустить.
    ///
    /// Перед выходом сохраняются настройки и расписание: новый exe стартует
    /// с тем же состоянием.
    fn apply_update(&mut self) {
        let Some(new_exe) = self.update_exe.clone() else {
            return;
        };
        let Ok(current) = std::env::current_exe() else {
            self.update_download_error = Some(self.t("upd.no_path").to_string());
            return;
        };
        // Приложение может лежать в Program Files — тогда заменить файл без
        // прав администратора нельзя. Сообщаем заранее, не закрывая окно.
        if let Some(dir) = current.parent() {
            if !dir_writable(dir) {
                let msg = self.tf("upd.no_permission", &[dir.display().to_string()]);
                self.update_download_error = Some(msg);
                return;
            }
        }
        save_ui_config(&self.ui_config);
        self.sched.save();
        match selfupdate::apply(&current, &new_exe) {
            Ok(()) => {
                // Даём bat-сценарию время увидеть наш PID до выхода.
                std::process::exit(0);
            }
            Err(e) => {
                self.update_download_error = Some(e);
            }
        }
    }

    /// Сравнить два отчёта сканирования: что появилось, исчезло, изменилось.
    fn start_diff(&mut self) {
        let old = std::path::PathBuf::from(self.diff_old.trim());
        let new = std::path::PathBuf::from(self.diff_new.trim());
        if old.as_os_str().is_empty() || new.as_os_str().is_empty() {
            self.push_log(self.t("cmp.need_files"));
            return;
        }
        let meta = OpMeta {
            mode: "diff".into(),
            title: self.t("cmp.reports").to_string(),
            paths: vec![
                old.display().to_string(),
                new.display().to_string(),
            ],
            source: "gui".into(),
        };
        self.spawn_op_meta(meta, move |log, _stop| {
            match remont_core::report_diff::diff_reports(&old, &new) {
                Ok((rows, stats)) => {
                    let name = format!(
                        "diff_{}.csv",
                        chrono::Local::now().format("%Y%m%d_%H%M%S")
                    );
                    let path = remont_core::resolve_report_path(name);
                    match remont_core::report_diff::save_diff_report(&rows, &stats, path) {
                        Ok(_p) => emit(
                            &log,
                            tfg(
                                "cmp.done",
                                &[
                                    stats.added.to_string(),
                                    stats.removed.to_string(),
                                    stats.changed.to_string(),
                                    stats.same.to_string(),
                                ],
                            ),
                        ),
                        Err(e) => emit(&log, tfg("cmp.failed", &[e.to_string()])),
                    }
                }
                Err(e) => emit(&log, tfg("cmp.failed", &[e.to_string()])),
            }
        });
    }

    fn start_action(&mut self) {
        let Some(mode) = self.mode else {
            return;
        };
        if self.running {
            return;
        }
        let def = mode_def(mode);
        match mode {
            Mode::Delete => {
                let Some(_paths) = self.check_paths(def) else {
                    return;
                };
                self.pending = Some(Pending::Confirm {
                    title: self.t("dlg.recursive_title").into(),
                    prompt: self.t("dlg.recursive_prompt").into(),
                    kind: ConfirmKind::DeleteRecursive,
                });
            }
            Mode::Create => {
                let Some(_paths) = self.check_paths(def) else {
                    return;
                };
                self.pending = Some(Pending::Confirm {
                    title: self.t("dlg.zip_title").into(),
                    prompt: self.t("dlg.zip_prompt").into(),
                    kind: ConfirmKind::CreateZip,
                });
            }
            Mode::Scan => {
                let cfg_buf = config_path(def.config);
                let cfg = cfg_buf.as_path();
                let paths = read_paths_file(cfg);
                if paths.is_empty() {
                    if cfg.exists() {
                        self.push_log(self.t("lbl.list_empty"));
                    } else {
                        self.push_log(&self.tf("lbl.config_missing", &[config_path(def.config).display().to_string()]));
                    }
                    match rfd::FileDialog::new().set_title(self.t("dlg.pick_scan")).pick_folder()
                    {
                        Some(p) => {
                            let p2 = p;
                            let threads = self.scan_threads;
                            self.spawn_op(move |log, stop| {
                                match scan_folder(&p2, None, threads, &stop, &log) {
                                    Some((out, stats)) => emit(
                                        &log,
                                        tfg("log.scan_result", &[stats.files.to_string(), out.display().to_string()]),
                                    ),
                                    None => emit(&log, tg("log.scan_failed")),
                                }
                            });
                        }
                        None => self.push_log(self.t("lbl.op_cancelled")),
                    }
                } else {
                    let master = format!("BATCH_SCAN_{}.csv", timestamp());
                    let threads = self.scan_threads;
                    self.spawn_op(move |log, stop| {
                        emit(&log, tfg("log.scan_batch", std::slice::from_ref(&master)));
                        for p in &paths {
                            if stop.is_set() {
                                break;
                            }
                            scan_folder(p, Some(Path::new(&master)), threads, &stop, &log);
                        }
                        // Пакетный файл конвертируется один раз — после всех папок.
                        let master = remont_core::finalize_report(PathBuf::from(&master)).display().to_string();
                        if stop.is_set() {
                            emit(&log, tfg("log.scan_batch_stop", std::slice::from_ref(&master)));
                        } else {
                            emit(&log, tfg("log.scan_batch_done", std::slice::from_ref(&master)));
                        }
                    });
                }
            }
            Mode::Compare => {
                self.pending = Some(Pending::CompareMethod { selected: 0 });
            }
            Mode::Cleanup => {
                let cfg_buf = config_path(def.config);
                let cfg = cfg_buf.as_path();
                let paths = read_paths_file(cfg);
                if paths.is_empty() {
                    if cfg.exists() {
                        self.push_log(self.t("lbl.list_empty"));
                    } else {
                        self.push_log(&self.tf("lbl.config_missing", &[config_path(def.config).display().to_string()]));
                    }
                    match rfd::FileDialog::new().set_title(self.t("dlg.pick_cleanup")).pick_folder()
                    {
                        Some(p) => {
                            self.spawn_op(move |log, stop| {
                                let n = cleanup_empty_folders(&p, &stop, &log);
                                emit(&log, tfg("log.cleanup_done", &[n.to_string()]));
                            });
                        }
                        None => self.push_log(self.t("lbl.op_cancelled")),
                    }
                } else {
                    self.spawn_op(move |log, stop| {
                        let mut total = 0;
                        for p in &paths {
                            if stop.is_set() {
                                break;
                            }
                            emit(&log, tfg("log.cleanup_header", &[p.display().to_string()]));
                            total += cleanup_empty_folders(p, &stop, &log);
                        }
                        emit(&log, tfg("log.cleanup_done", &[total.to_string()]));
                    });
                }
            }
            Mode::Copy => {
                if !config_path(def.config).exists() {
                    self.push_log(&self.tf("lbl.config_missing", &[config_path(def.config).display().to_string()]));
                    return;
                }
                self.pending = Some(Pending::Confirm {
                    title: self.t("dlg.resume_title").into(),
                    prompt: self.t("dlg.resume_prompt").into(),
                    kind: ConfirmKind::CopyResume,
                });
            }
            Mode::Duplicates => {
                let cfg_buf = config_path(def.config);
                let cfg = cfg_buf.as_path();
                let paths = read_paths_file(cfg);
                if paths.is_empty() {
                    if cfg.exists() {
                        self.push_log(self.t("lbl.list_empty"));
                    } else {
                        self.push_log(&self.tf("lbl.config_missing", &[config_path(def.config).display().to_string()]));
                    }
                    match rfd::FileDialog::new().set_title(self.t("dlg.pick_dup")).pick_folder()
                    {
                        Some(p) => self.pending = Some(Pending::DupAlgo { paths: vec![p] }),
                        None => self.push_log(self.t("lbl.op_cancelled")),
                    }
                } else {
                    self.pending = Some(Pending::DupAlgo { paths });
                }
            }
        }
    }

    /// Ответ «Да» в модальном окне.
    fn confirm_yes(&mut self, kind: ConfirmKind) {
        match kind {
            ConfirmKind::DeleteRecursive => {
                let paths = read_paths_file(&config_path("folders_to_delete.txt"));
                let mode = self.ui_config.delete_mode;
                self.spawn_op(move |log, stop| {
                    let s = remont_core::delete_paths_opts(&paths, true, mode, &stop, &log);
                    emit(
                        &log,
                        tfg(
                            "log.delete_summary",
                            &[
                                s.folders.to_string(),
                                s.files.to_string(),
                                s.parents.to_string(),
                            ],
                        ),
                    );
                });
            }
            ConfirmKind::CreateZip => {
                let paths = read_paths_file(&config_path("folders_to_create.txt"));
                self.spawn_op(move |log, stop| {
                    let created = create_folders(&paths, &stop, &log);
                    emit(&log, tfg("log.create_summary", &[created.len().to_string()]));
                    if created.is_empty() {
                        emit(&log, tg("log.create_none"));
                    } else if let Some(zip) = archive_folders(&created, true, &stop, &log) {
                        emit(&log, tfg("log.zip_created", &[zip.display().to_string()]));
                    } else {
                        emit(&log, tg("log.zip_failed"));
                    }
                });
            }
            ConfirmKind::CopyResume => {
                let cfg = config_path("from_to.csv");
                let opts = remont_core::copy::CopyOptions {
                    verify: self.ui_config.verify_copy,
                    check_space: self.ui_config.check_space,
                };
                self.spawn_op(move |log, stop| {
                    let cache = remont_core::paths::config_path("copy_resume.json");
                    let rep = remont_core::copy::copy_files_from_list_opts(
                        &cfg, &cache, 16, true, opts, &stop, &log,
                    );
                    emit(&log, tfg("log.copy_summary", &[rep.copied.to_string()]));
                });
            }
            ConfirmKind::SavePairMismatch => {
                self.save_pair_config(true);
            }
            ConfirmKind::ClearHistory => {
                match remont_core::history::clear() {
                    Ok(()) => {
                        self.push_log(self.t("hist.cleared"));
                        self.reload_history();
                    }
                    Err(e) => {
                        let msg = self.tf("hist.failed", &[e.to_string()]);
                        self.push_log(&msg);
                    }
                }
            }
        }
    }

    /// Ответ «Нет» в модальном окне.
    fn confirm_no(&mut self, kind: ConfirmKind) {
        match kind {
            ConfirmKind::DeleteRecursive => {
                let paths = read_paths_file(&config_path("folders_to_delete.txt"));
                let mode = self.ui_config.delete_mode;
                self.spawn_op(move |log, stop| {
                    let s = remont_core::delete_paths_opts(&paths, false, mode, &stop, &log);
                    emit(
                        &log,
                        tfg(
                            "log.delete_summary_noparents",
                            &[s.folders.to_string(), s.files.to_string()],
                        ),
                    );
                });
            }
            ConfirmKind::CreateZip => {
                let paths = read_paths_file(&config_path("folders_to_create.txt"));
                self.spawn_op(move |log, stop| {
                    let created = create_folders(&paths, &stop, &log);
                    emit(&log, tfg("log.create_summary", &[created.len().to_string()]));
                });
            }
            ConfirmKind::CopyResume => {
                let cfg = config_path("from_to.csv");
                let opts = remont_core::copy::CopyOptions {
                    verify: self.ui_config.verify_copy,
                    check_space: self.ui_config.check_space,
                };
                self.spawn_op(move |log, stop| {
                    let cache = remont_core::paths::config_path("copy_resume.json");
                    let rep = remont_core::copy::copy_files_from_list_opts(
                        &cfg, &cache, 16, false, opts, &stop, &log,
                    );
                    emit(&log, tfg("log.copy_summary", &[rep.copied.to_string()]));
                });
            }
            ConfirmKind::SavePairMismatch => {
                // «Нет» = не сохранять
            }
            ConfirmKind::ClearHistory => {}
        }
    }

    // ==================== ВКЛАДКА «НАСТРОЙКИ» ====================


    /// Все настройки одним блоком (без деления на «Интерфейс» и «Windows»).
    fn settings_options(&mut self, ui: &mut egui::Ui) {
        let is_win = cfg!(windows);
        if is_win {
            if self.ctx_menu.is_none() {
                self.ctx_menu = Some(winshell::menu_installed());
            }
            if self.autostart.is_none() {
                self.autostart = Some(winshell::autostart_installed());
            }
        }
        let l_theme = self.t("ui.theme").to_string();
        let l_light = self.t("ui.theme_light").to_string();
        let l_dark = self.t("ui.theme_dark").to_string();
        let l_system = self.t("ui.theme_system").to_string();
        let l_lang = self.t("ui.lang").to_string();
        let l_format = self.t("ui.format").to_string();
        let l_output = self.t("ui.output_dir").to_string();
        let l_output_hint = self.t("ui.output_hint").to_string();
        let l_pick = self.t("sch.pick").to_string();
        let l_reset = self.t("ui.output_reset").to_string();
        let l_open_result = self.t("ui.open_result").to_string();
        let l_check_updates = self.t("ui.check_updates").to_string();
        let l_auto_download = self.t("ui.auto_download").to_string();
        let l_auto_install = self.t("ui.auto_install").to_string();
        let l_notify = self.t("ui.notify").to_string();
        let l_delete_mode = self.t("ui.delete_mode").to_string();
        let l_verify_copy = self.t("ui.verify_copy").to_string();
        let l_check_space = self.t("ui.check_space").to_string();
        let l_menu = self.tf("win.ctx_menu", &[winshell::menu_caption(self.ui_config.lang == Lang::Ru).to_string()]);
        let l_auto = self.t("win.autostart").to_string();
        let l_tray = self.t("win.tray").to_string();
        let l_na = self.t("win.not_available").to_string();

        let mut browse = false;
        let mut reset = false;

        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.label(&l_theme);
                ui.selectable_value(&mut self.ui_config.theme, ThemeMode::Light, &l_light);
                ui.selectable_value(&mut self.ui_config.theme, ThemeMode::Dark, &l_dark);
                ui.selectable_value(&mut self.ui_config.theme, ThemeMode::System, &l_system);
                ui.separator();
                ui.label(&l_lang);
                ui.selectable_value(&mut self.ui_config.lang, Lang::Ru, "Русский");
                ui.selectable_value(&mut self.ui_config.lang, Lang::En, "English");
                ui.separator();
                ui.label(&l_format);
                ui.selectable_value(&mut self.ui_config.report_format, OutputFormat::Csv, "CSV");
                ui.selectable_value(&mut self.ui_config.report_format, OutputFormat::Xlsx, "XLSX");
                ui.separator();
                if ui.button(self.t("btn.open_config")).clicked() {
                    save_ui_config(&self.ui_config);
                    open_config_file(&config_path("ui.json"));
                }
            });

            ui.horizontal(|ui| {
                ui.label(&l_output);
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.ui_config.output_dir).desired_width(ui.available_width() - 180.0).hint_text(&l_output_hint),
                );
                let _ = r;
                if ui.button(&l_pick).clicked() {
                    browse = true;
                }
                if ui.button(&l_reset).clicked() {
                    reset = true;
                }
            });

            ui.checkbox(&mut self.ui_config.open_result, &l_open_result);
            ui.checkbox(&mut self.ui_config.notify, &l_notify);


            ui.checkbox(&mut self.ui_config.verify_copy, &l_verify_copy);
            ui.checkbox(&mut self.ui_config.check_space, &l_check_space);
            ui.checkbox(&mut self.ui_config.check_updates, &l_check_updates);
            // Автоустановка немыслима без автозагрузки — включаем её следом.
            let mut auto_install = self.ui_config.auto_install;
            if ui.checkbox(&mut auto_install, &l_auto_install).changed() {
                self.ui_config.auto_install = auto_install;
                if auto_install {
                    self.ui_config.auto_download = true;
                }
            }
            ui.add_enabled_ui(!self.ui_config.auto_install, |ui| {
                ui.checkbox(&mut self.ui_config.auto_download, &l_auto_download);
            });

            // Способ удаления: безвозвратно / корзина / архив.
            ui.horizontal(|ui| {
                ui.label(&l_delete_mode);
                for m in remont_core::DeleteMode::ALL {
                    ui.selectable_value(
                        &mut self.ui_config.delete_mode,
                        m,
                        m.label(self.ui_config.lang),
                    );
                }
            });

            if is_win {
                // Все флажки идут одним списком, без отбивки и разделителей:
                // иначе казалось, что блок разбит на части.
                // Все три пункта — одинаковые флажки: без статус-точек и
                // разных отступов (пользователь просил один тип).
                let mut menu_on = self.ctx_menu.unwrap_or(false);
                ui.checkbox(&mut menu_on, &l_menu);
                let mut auto_on = self.autostart.unwrap_or(false);
                ui.checkbox(&mut auto_on, &l_auto);
                ui.checkbox(&mut self.ui_config.close_to_tray, &l_tray);
                // Обработка изменений (после отрисовки статусов)
                if menu_on != self.ctx_menu.unwrap_or(false) {
                    let exe = winshell::exe_path();
                    let caption = winshell::menu_caption(self.ui_config.lang == Lang::Ru);
                    let res = if menu_on {
                        winshell::menu_install(&exe, caption)
                    } else {
                        winshell::menu_uninstall()
                    };
                    match res {
                        Ok(()) => {
                            self.ctx_menu = Some(menu_on);
                            self.push_log(if menu_on {
                                tg("log.win_added")
                            } else {
                                tg("log.win_removed")
                            });
                        }
                        Err(e) => {
                            self.ctx_menu = Some(!menu_on);
                            self.push_log(&tfg("log.win_error", &[e]));
                        }
                    }
                }
                if auto_on != self.autostart.unwrap_or(false) {
                    let exe = winshell::exe_path();
                    let minimized = self.ui_config.close_to_tray;
                    let res = if auto_on {
                        winshell::autostart_install(&exe, minimized)
                    } else {
                        winshell::autostart_uninstall()
                    };
                    match res {
                        Ok(()) => {
                            self.autostart = Some(auto_on);
                            self.push_log(if auto_on {
                                tg("log.autostart_on")
                            } else {
                                tg("log.autostart_off")
                            });
                        }
                        Err(e) => {
                            self.autostart = Some(!auto_on);
                            self.push_log(&tfg("log.win_error", &[e]));
                        }
                    }
                }
            } else {
                ui.label(RichText::new(l_na).weak());
            }
        });

        if browse {
            if let Some(p) = rfd::FileDialog::new().pick_folder() {
                self.ui_config.output_dir = p.to_string_lossy().to_string();
            }
        }
        if reset {
            self.ui_config.output_dir.clear();
        }
    }

    fn settings_tab(&mut self, ui: &mut egui::Ui) {
        let ui_before = self.ui_config.clone();
        self.settings_options(ui);
        // Сохраняем только при изменении + сразу применяем все настройки.
        if self.ui_config != ui_before {
            save_ui_config(&self.ui_config);
            set_global(self.ui_config.lang);
            remont_core::set_output_format(self.ui_config.report_format);
            apply_output_dir(&self.ui_config.output_dir);
            // Пункт меню Проводника подписан текущим языком — обновляем,
            // если он установлен.
            if self.ctx_menu == Some(true) && self.ui_config.lang != ui_before.lang {
                let caption = winshell::menu_caption(self.ui_config.lang == Lang::Ru);
                if let Err(e) = winshell::menu_install(&winshell::exe_path(), caption) {
                    self.push_log(&tfg("log.win_error", &[e]));
                }
            }
        }
        // Тема применяется немедленно: без этого переключение «не работало»,
        // потому что следующий кадр уже видел applied_theme == ui_config.theme.
        if self.ui_config.theme != self.applied_theme {
            self.applied_theme = self.ui_config.theme;
            ui.ctx().set_theme(theme_preference(self.applied_theme));
            // Заголовок окна рисует Windows — ему тему нужно сообщить отдельно.
            let dark = match self.applied_theme {
                ThemeMode::Dark => true,
                ThemeMode::Light => false,
                ThemeMode::System => ui.ctx().style().visuals.dark_mode,
            };
            winshell::set_dark_title(self.hwnd.load(Ordering::Relaxed), dark);
            ui.ctx().request_repaint();
        }

        // Перезагрузка редактора при смене режима
        if self.settings_loaded != self.settings_sel {
            self.settings_loaded = self.settings_sel;
            let def = &MODES[self.settings_sel];
            if def.is_pair {
                let (l, r) = load_pair_file(config_path(def.config));
                self.editor_left = l;
                self.editor_right = r;
            } else {
                self.editor_single = load_text_file(config_path(def.config));
            }
        }
        // Отступ отделяет раздел «Выберите режим для настройки» от общих
        // настроек выше — именно его просили визуально отделить.
        ui.add_space(12.0);
        let full_w = ui.available_width();
        let avail_h = ui.available_height();
        ui.horizontal(|ui| {
            let list_w = 220.0;
            // Запас справа: кнопки и поле не должны упираться в край окна.
            let right_margin = 28.0;
            let editor_w = (full_w - list_w - 14.0 - right_margin).max(320.0);
            ui.vertical(|ui| {
                ui.set_width(list_w);
                ui.set_height(avail_h);
                ui.label(RichText::new(self.t("lbl.select_mode")).strong());
                for (i, def) in MODES.iter().enumerate() {
                    let label = self.t(mode_key(def.mode));
                    ui.selectable_value(&mut self.settings_sel, i, label);
                }
            });
            ui.separator();
            ui.vertical(|ui| {
                ui.set_width(editor_w);
                ui.set_height(avail_h);
                let def = &MODES[self.settings_sel];
                if def.is_pair {
                    self.pair_editor(ui, def);
                } else {
                    self.single_editor(ui, def);
                }
            });
        });
    }

    fn single_editor(&mut self, ui: &mut egui::Ui, def: &ModeDef) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(self.tf("lbl.mode_line", &[self.t(def.title_key).to_string()])).strong());
            ui.label(RichText::new(self.tf("lbl.file_line", &[config_path(def.config).display().to_string()])).weak());
        });
        if def.mode == Mode::Scan {
            ui.horizontal(|ui| {
                ui.label(self.t("lbl.threads"));
                ui.add(egui::DragValue::new(&mut self.scan_threads).range(1..=256));
                ui.label(RichText::new(self.t("lbl.threads_hint")).weak());
            });
        }
        ui.label(RichText::new(
            self.t("lbl.paths_hint"),
        ).weak());
        ui.horizontal_wrapped(|ui| {
            if ui.button(self.t("btn.load")).clicked() {
                // Выбор файла на диске (раньше кнопка просто перечитывала
                // уже открытый конфиг и выглядела «неработающей»).
                if let Some(p) = rfd::FileDialog::new().pick_file() {
                    match fs::read(&p) {
                        Ok(bytes) => {
                            self.editor_single = remont_core::config::decode_config(&bytes);
                        }
                        Err(e) => {
                            self.push_log(&tfg("lbl.load_failed", &[e.to_string()]));
                        }
                    }
                }
            }
            if ui.button(self.t("btn.add_folder")).clicked() {
                if let Some(p) = rfd::FileDialog::new().pick_folder() {
                    Self::append_path(&mut self.editor_single, p.to_string_lossy().as_ref());
                }
            }
            if ui.button(self.t("btn.add_file")).clicked() {
                if let Some(p) = rfd::FileDialog::new().pick_file() {
                    Self::append_path(&mut self.editor_single, p.to_string_lossy().as_ref());
                }
            }
            if ui.button(self.t("btn.clear")).clicked() {
                self.editor_single.clear();
            }
            if ui.button(self.t("btn.open_config")).clicked() {
                open_config_file(&config_path(def.config));
            }
        });
        let h = (ui.available_height() - 34.0).max(60.0);
        let w = ui.available_width();
        ui.add_sized(
            [w, h],
            TextEdit::multiline(&mut self.editor_single).font(FontId::monospace(13.0)),
        );
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(self.t("btn.save")).clicked() {
                    if let Err(e) = save_text_file(config_path(def.config), &self.editor_single) {
                        self.push_log(&tfg("lbl.save_failed", std::slice::from_ref(&e)));
                    } else {
                        if def.mode == Mode::Scan {
                            let _ = save_text_file(
                                config_path("scan_threads.txt"),
                                &self.scan_threads.to_string(),
                            );
                        }
                        self.push_log(&tfg("lbl.config_saved", &[config_path(def.config).display().to_string()]));
                    }
                }
            });
        });
    }

    fn pair_editor(&mut self, ui: &mut egui::Ui, def: &ModeDef) {
        let (left_l, right_l) = match def.mode {
            Mode::Copy => (self.t("lbl.source"), self.t("lbl.dest")),
            _ => (self.t("lbl.folder_a"), self.t("lbl.folder_b")),
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new(self.tf("lbl.mode_line", &[self.t(def.title_key).to_string()])).strong());
            ui.label(RichText::new(self.tf("lbl.file_line", &[config_path(def.config).display().to_string()])).weak());
        });
        ui.label(RichText::new(
            self.t("lbl.pairs_hint"),
        ).weak());
        ui.horizontal_wrapped(|ui| {
            if ui.button(self.t("btn.load")).clicked() {
                if let Some(p) = rfd::FileDialog::new().pick_file() {
                    let (l, r) = load_pair_file(&p);
                    self.editor_left = l;
                    self.editor_right = r;
                }
            }
            if ui.button(self.t("btn.clear_all")).clicked() {
                self.editor_left.clear();
                self.editor_right.clear();
            }
            if ui.button(self.t("btn.open_config")).clicked() {
                open_config_file(&config_path(def.config));
            }
        });
        ui.horizontal(|ui| {
            let cols_h = (ui.available_height() - 46.0).max(120.0);
            let col_w = (ui.available_width() - 10.0) / 2.0;
            ui.vertical(|ui| {
                ui.set_width(col_w);
                ui.set_height(cols_h);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(left_l).strong());
                    if ui.button(self.t("btn.pick_folder")).clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_folder() {
                            Self::append_path(&mut self.editor_left, p.to_string_lossy().as_ref());
                        }
                    }
                    if ui.button(self.t("btn.pick_file")).clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_file() {
                            Self::append_path(&mut self.editor_left, p.to_string_lossy().as_ref());
                        }
                    }
                });
                let h = (ui.available_height() - 4.0).max(60.0);
                ui.add_sized(
                    [col_w, h],
                    TextEdit::multiline(&mut self.editor_left).font(FontId::monospace(13.0)),
                );
            });
            ui.separator();
            ui.vertical(|ui| {
                ui.set_width(col_w);
                ui.set_height(cols_h);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(right_l).strong());
                    if ui.button(self.t("btn.pick_folder")).clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_folder() {
                            Self::append_path(&mut self.editor_right, p.to_string_lossy().as_ref());
                        }
                    }
                    if ui.button(self.t("btn.pick_file")).clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_file() {
                            Self::append_path(&mut self.editor_right, p.to_string_lossy().as_ref());
                        }
                    }
                });
                let h = (ui.available_height() - 4.0).max(60.0);
                ui.add_sized(
                    [col_w, h],
                    TextEdit::multiline(&mut self.editor_right).font(FontId::monospace(13.0)),
                );
            });
        });
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(self.t("btn.save")).clicked() {
                    self.save_pair_config(false);
                }
            });
        });
    }

    /// Сохранение парного конфига: выравнивание,
    /// проверка неполных пар (с подтверждением), запись CSV `a;b`.
    fn save_pair_config(&mut self, force: bool) {
        let def = &MODES[self.settings_sel];
        let mut lines1: Vec<String> = self.editor_left.lines().map(|l| l.trim().to_string()).collect();
        let mut lines2: Vec<String> = self.editor_right.lines().map(|l| l.trim().to_string()).collect();
        let max_len = lines1.len().max(lines2.len());
        while lines1.len() < max_len {
            lines1.push(String::new());
        }
        while lines2.len() < max_len {
            lines2.push(String::new());
        }
        let mismatched: Vec<usize> = lines1.iter().zip(lines2.iter()).enumerate().filter(|(_, (a, b))| (a.is_empty() && !b.is_empty()) || (!a.is_empty() && b.is_empty())).map(|(i, _)| i + 1).collect();
        if !mismatched.is_empty() && !force {
            let sample = mismatched.iter().take(5).map(|n| n.to_string()).collect::<Vec<_>>().join(", ");
            let more = if mismatched.len() > 5 {
                tfg("lbl.more", &[(mismatched.len() - 5).to_string()])
            } else {
                String::new()
            };
            self.pending = Some(Pending::Confirm {
                title: self.t("dlg.mismatch_title").into(),
                prompt: self.tf(
                    "dlg.mismatch_prompt",
                    &[
                        format!("{sample}{more}"),
                        mismatched.len().to_string(),
                    ],
                ),
                kind: ConfirmKind::SavePairMismatch,
            });
            return;
        }
        let mut out = String::new();
        for (a, b) in lines1.iter().zip(lines2.iter()) {
            if !a.is_empty() || !b.is_empty() {
                out.push_str(&format!("{a};{b}\n"));
            }
        }
        if let Err(e) = save_text_file(config_path(def.config), &out) {
            self.push_log(&tfg("lbl.save_failed", std::slice::from_ref(&e)));
        } else {
            self.push_log(&tfg("lbl.config_saved", &[config_path(def.config).display().to_string()]));
        }
    }

    fn append_path(text: &mut String, path: &str) {
        if text.trim().is_empty() {
            *text = path.to_string();
        } else {
            text.push('\n');
            text.push_str(path);
        }
    }

    fn run_duplicates(&mut self, paths: Vec<PathBuf>, algo: HashAlgo) {
        self.spawn_op(move |log, stop| {
            match find_duplicates_multi(&paths, algo, None, &stop, &log) {
                Ok(groups) => {
                    let n: usize = groups.iter().map(|g| g.len()).sum();
                    emit(
                        &log,
                        tfg(
                            "log.dup_summary",
                            &[groups.len().to_string(), n.to_string()],
                        ),
                    );
                }
                Err(e) => emit(&log, tfg("log.error", &[e.to_string()])),
            }
        });
    }

    // ==================== ВКЛАДКА «ПЛАНИРОВАНИЕ» ====================

    fn schedule_tab(&mut self, ui: &mut egui::Ui) {
        let full_w = ui.available_width();
        let avail_h = ui.available_height();
        let mut add_task = false;
        let mut del_task = false;
        // Цвета режимов для списка заданий
        let mode_colors = [
            egui::Color32::from_rgb(220, 53, 69),
            egui::Color32::from_rgb(40, 167, 69),
            egui::Color32::from_rgb(33, 100, 220),
            egui::Color32::from_rgb(255, 159, 10),
            egui::Color32::from_rgb(108, 117, 125),
            egui::Color32::from_rgb(0, 123, 167),
            egui::Color32::from_rgb(154, 52, 185),
        ];
        ui.horizontal(|ui| {
            let list_w = 300.0;
            let right_margin = 12.0;
            let right_w = (full_w - list_w - 14.0 - right_margin).max(320.0);
            ui.vertical(|ui| {
                ui.set_width(list_w);
                ui.set_height(avail_h);
                ui.label(RichText::new(self.t("sch.title")).strong());
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button(self.t("sch.add")).clicked() {
                        add_task = true;
                    }
                    let can_del = !self.sched.tasks.is_empty();
                    if ui.add_enabled(can_del, egui::Button::new(self.t("sch.remove"))).clicked()
                    {
                        del_task = true;
                    }
                });
                ui.add_space(6.0);
                if self.sched.tasks.is_empty() {
                    ui.label(RichText::new(self.t("sch.empty")).weak());
                } else {
                    // Подписи собираются заранее: во время выбора нельзя
                    // держать заимствование `self` (см. self.t внутри цикла).
                    let labels: Vec<String> = self.sched.tasks.iter().map(|t| {
                            let mode_idx = mode_from_key(&t.mode)
                                .map(|m| match m {
                                    Mode::Delete => 0, Mode::Create => 1, Mode::Scan => 2,
                                    Mode::Compare => 3, Mode::Cleanup => 4, Mode::Copy => 5, Mode::Duplicates => 6,
                                })
                                .unwrap_or(0);
                            let mode_label = mode_from_key(&t.mode).map(|m| self.t(mode_key(m))).unwrap_or("?");
                            let _ = mode_idx;
                            let state = if t.enabled && t.remaining > 0 {
                                format!("  ({}, {})", t.time_label(), t.remaining)
                            } else {
                                format!("  ({}, —)", t.time_label())
                            };
                            format!("{mode_label}{state}")
                        }).collect();
                    for (i, label) in labels.into_iter().enumerate() {
                        let task = &self.sched.tasks[i];
                        let mode_idx = mode_from_key(&task.mode)
                            .map(|m| match m {
                                Mode::Delete => 0, Mode::Create => 1, Mode::Scan => 2,
                                Mode::Compare => 3, Mode::Cleanup => 4, Mode::Copy => 5, Mode::Duplicates => 6,
                            })
                            .unwrap_or(0);
                        let color = mode_colors[mode_idx];
                        let sel_style = if self.sched_sel == i {
                            RichText::new(label).color(color).size(13.0)
                        } else {
                            RichText::new(label).weak().size(12.0)
                        };
                        if ui.selectable_label(self.sched_sel == i, sel_style).clicked() {
                            self.sched_sel = i;
                        }
                    }
                }
                ui.add_space(4.0);
                if ui.button(self.t("btn.open_config")).clicked() {
                    self.sched.save();
                    open_config_file(&config_path("schedule.json"));
                }
                ui.add_space(8.0);
                ui.separator();
                ui.label(RichText::new(self.t("sch.hint")).weak());
                ui.label(
                    RichText::new(
                        self.tf(
                            "lbl.schedule_file",
                            &[config_path("schedule.json").display().to_string()],
                        ),
                    ).weak(),
                );
            });
            ui.separator();
            ui.vertical(|ui| {
                ui.set_width(right_w);
                ui.set_height(avail_h);
                self.schedule_editor(ui);
            });
        });

        if add_task {
            let id = self.sched.next_id();
            let mode = self.mode.map(mode_slug).unwrap_or("scan");
            self.sched.tasks.push(schedule::Task::new(id, mode, 9, 0, 1));
            self.sched_sel = self.sched.tasks.len() - 1;
            self.sched.save();
        }
        if del_task && !self.sched.tasks.is_empty() {
            let idx = self.sched_sel.min(self.sched.tasks.len() - 1);
            self.sched.tasks.remove(idx);
            self.sched_sel = idx.min(self.sched.tasks.len().saturating_sub(1));
            self.sched.save();
        }
    }

    fn schedule_editor(&mut self, ui: &mut egui::Ui) {
        if self.sched.tasks.is_empty() {
            ui.label(RichText::new(self.t("sch.empty")).weak());
            return;
        }
        let idx = self.sched_sel.min(self.sched.tasks.len() - 1);
        self.sched_sel = idx;

        // Тексты и подписи режимов — до мутабельного заимствования задания.
        let l_mode = self.t("sch.mode").to_string();
        let l_time = self.t("sch.time").to_string();
        let l_count = self.t("sch.count").to_string();
        let l_output = self.t("sch.output").to_string();
        let l_out_hint = self.t("sch.output_hint").to_string();
        let l_pick = self.t("sch.pick").to_string();
        let l_run = self.t("sch.run_now").to_string();
        let l_next = self.t("sch.next").to_string();
        let l_last = self.t("sch.last").to_string();
        let l_remaining = self.t("sch.remaining").to_string();
        let l_never = self.t("sch.never").to_string();
        let l_enabled = self.t("sch.enabled").to_string();
        let l_tab = self.t("tab.schedule").to_string();
        let cur_mode = self.sched.tasks[idx].mode.clone();
        let cur_label = mode_from_key(&cur_mode).map(|m| self.t(mode_key(m)).to_string()).unwrap_or_else(|| cur_mode.clone());
        let mode_items: Vec<(&'static str, String)> = MODES.iter().map(|d| (mode_slug(d.mode), self.t(d.title_key).to_string())).collect();

        let mut run_now = false;
        let mut browse = false;
        let mut dirty = false;
        {
            let task = &mut self.sched.tasks[idx];
            if ui.checkbox(&mut task.enabled, &l_enabled).changed() {
                dirty = true;
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(&l_mode);
                egui::ComboBox::from_id_salt("sch_mode").selected_text(cur_label.clone()).show_ui(ui, |ui| {
                        for (slug, title) in &mode_items {
                            let selected = task.mode == *slug;
                            if ui.selectable_label(selected, title).clicked() && !selected {
                                task.mode = slug.to_string();
                            }
                        }
                    });
            });
            ui.horizontal(|ui| {
                ui.label(&l_time);
                let mut hh = task.hour as i32;
                let mut mm = task.minute as i32;
                let r1 = ui.add(egui::DragValue::new(&mut hh).range(0..=23).speed(1));
                ui.label(":");
                let r2 = ui.add(egui::DragValue::new(&mut mm).range(0..=59).speed(1));
                if r1.changed() || r2.changed() {
                    task.hour = hh as u8;
                    task.minute = mm as u8;
                    dirty = true;
                }
                ui.label(
                    RichText::new(format!(
                        "{:02}:{:02}",
                        task.hour, task.minute
                    )).weak(),
                );
            });
            ui.horizontal(|ui| {
                ui.label(&l_count);
                let mut total = task.total as i32;
                let r = ui.add(egui::DragValue::new(&mut total).range(0..=9999).speed(1));
                if r.changed() {
                    apply_total_change(task, total.max(0) as u32);
                    dirty = true;
                }
                ui.label(RichText::new(l_remaining.replace("{}", &task.remaining.to_string())).weak());
            });
            ui.add_space(6.0);
            ui.label(&l_output);
            ui.horizontal(|ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut task.output_dir).desired_width(ui.available_width() - 120.0),
                );
                if r.changed() {
                    dirty = true;
                }
                if ui.button(&l_pick).clicked() {
                    browse = true;
                }
            });
            ui.label(RichText::new(&l_out_hint).weak());
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new(&l_run).strong()).clicked() {
                    run_now = true;
                }
            });
            ui.add_space(8.0);
            let last = if task.last_run.trim().is_empty() {
                l_never.clone()
            } else {
                task.last_run.clone()
            };
            ui.label(RichText::new(l_last.replace("{}", &last)).weak());
            let next = format!("{} {} ({})", l_tab, task.time_label(), task.remaining);
            ui.label(RichText::new(l_next.replace("{}", &next)).weak());
        }
        if browse {
            if let Some(p) = rfd::FileDialog::new().pick_folder() {
                self.sched.tasks[idx].output_dir = p.to_string_lossy().to_string();
                dirty = true;
            }
        }
        if dirty {
            self.sched.save();
        }
        if run_now {
            if self.running {
                let name = mode_from_key(&self.sched.tasks[idx].mode).map(|m| self.t(mode_key(m)).to_string()).unwrap_or_default();
                self.push_log(&self.tf("log.sch_busy", &[name]));
            } else {
                self.run_task_now(idx);
            }
        }
    }

    /// Проверка расписания: раз в секунду, с окном «догона» после запуска.
    fn scheduler_tick(&mut self, _ctx: &egui::Context) {
        let now = std::time::Instant::now();
        if let Some(prev) = self.sched_last_tick {
            if now.duration_since(prev).as_secs() < 1 {
                return;
            }
        }
        self.sched_last_tick = Some(now);

        let local = chrono::Local::now();
        let today = local.format("%Y-%m-%d").to_string();
        let cur = local.hour() * 60 + local.minute();

        // Время наступило (в пределах окна «догона»).
        if let Some(i) = self.sched.tasks.iter().position(|t| task_due(t, &today, cur)) {
            if self.running {
                // Идёт другая операция — повторим на следующем тике;
                // уведомление — не чаще одного раза в минуту.
                let notify = self.sched_busy_last.map(|ts| now.duration_since(ts) >= std::time::Duration::from_secs(60)).unwrap_or(true);
                if notify {
                    self.sched_busy_last = Some(now);
                    let name = mode_from_key(&self.sched.tasks[i].mode).map(|m| self.t(mode_key(m)).to_string()).unwrap_or_default();
                    self.push_log(&tfg("log.sch_busy", &[name]));
                }
            } else {
                self.run_task_now(i);
            }
        } else if let Some(i) = exhausted_index(&self.sched.tasks) {
            // Задание исчерпало выполнения — отключаем и сообщаем один раз.
            let name = mode_from_key(&self.sched.tasks[i].mode).map(|m| self.t(mode_key(m)).to_string()).unwrap_or_default();
            self.sched.tasks[i].enabled = false;
            self.sched.save();
            self.push_log(&self.tf("log.sch_exhausted", &[name]));
        }

        // Пропущенные на сегодня задания (время прошло, пока приложение
        // было закрыто или занято) — сообщаем о каждом один раз.
        // Списки считаются ДО обращения к self.t: внутри цикла нельзя
        // держать заимствование `self` (см. skill: borrow-конфликт egui).
        let notified: std::collections::HashSet<u64> = self.sched_missed_notified.iter().filter(|(_, d)| *d == today).map(|(id, _)| *id).collect();
        let missed: Vec<(u64, String)> = self.sched.tasks.iter().filter(|t| task_missed(t, &today, cur) && !notified.contains(&t.id)).map(|t| (t.id, t.mode.clone())).collect();
        for (id, mode) in missed {
            let name = mode_from_key(&mode).map(|m| self.t(mode_key(m)).to_string()).unwrap_or_default();
            self.push_log(&tfg("log.sch_missed", &[name]));
            self.sched_missed_notified.push((id, today.clone()));
        }
        self.sched_missed_notified.retain(|(_, d)| d == &today);
    }

    /// Запуск задания расписания (без диалогов, параметры — по умолчанию).
    fn run_task_now(&mut self, idx: usize) {
        let Some(task) = self.sched.tasks.get(idx).cloned() else {
            return;
        };
        let Some(mode) = mode_from_key(&task.mode) else {
            return;
        };
        let def = mode_def(mode);
        let cfg_path = config_path(def.config);
        let paths = read_paths_file(&cfg_path);
        let name = self.t(def.title_key).to_string();
        if paths.is_empty() && mode != Mode::Copy {
            self.push_log(&self.tf("log.sch_no_paths", &[name]));
            return;
        }
        if mode == Mode::Copy && !cfg_path.exists() {
            self.push_log(&self.tf("lbl.config_missing", &[config_path(def.config).display().to_string()]));
            return;
        }
        let out_dir = if task.output_dir.trim().is_empty() {
            PathBuf::from(".")
        } else {
            PathBuf::from(task.output_dir.trim())
        };
        let _ = fs::create_dir_all(&out_dir);
        let ts = timestamp();
        let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        if let Some(t) = self.sched.tasks.get_mut(idx) {
            t.remaining = t.remaining.saturating_sub(1);
            t.last_date = today;
            t.last_run = stamp.clone();
        }
        // Немедленное сохранение: списание выполнения не должно
        // зависеть от следующего тика (крэш/выход в интервале).
        self.sched.save();
        let remaining = self.sched.tasks[idx].remaining;
        self.push_log(&self.tf("log.sch_run", &[name.clone(), stamp]));

        let threads = self.scan_threads;
        let out = out_dir.clone();
        let task_name = name.clone();
        let meta = OpMeta {
            mode: task.mode.clone(),
            title: format!("{} ({})", name, self.t("tab.schedule")),
            paths: paths.iter().map(|p| p.display().to_string()).collect(),
            source: "schedule".into(),
        };
        self.spawn_op_meta(meta, move |log, stop| {
            match mode {
                Mode::Delete => {
                    let s = delete_paths(&paths, true, &stop, &log);
                    emit(
                        &log,
                        tfg(
                            "log.delete_summary",
                            &[
                                s.folders.to_string(),
                                s.files.to_string(),
                                s.parents.to_string(),
                            ],
                        ),
                    );
                }
                Mode::Create => {
                    let created = create_folders(&paths, &stop, &log);
                    emit(&log, tfg("log.create_summary", &[created.len().to_string()]));
                }
                Mode::Scan => {
                    let master = out.join(format!("BATCH_SCAN_{ts}.csv"));
                    let master_s = master.display().to_string();
                    emit(&log, tfg("log.scan_batch", std::slice::from_ref(&master_s)));
                    for p in &paths {
                        if stop.is_set() {
                            break;
                        }
                        scan_folder(p, Some(&master), threads, &stop, &log);
                    }
                    let master_s = remont_core::finalize_report(PathBuf::from(&master_s)).display().to_string();
                    if stop.is_set() {
                        emit(&log, tfg("log.scan_batch_stop", std::slice::from_ref(&master_s)));
                    } else {
                        emit(&log, tfg("log.scan_batch_done", std::slice::from_ref(&master_s)));
                    }
                }
                Mode::Compare => {
                    let out_csv = out.join(format!("compare_{ts}.csv"));
                    if let Err(e) = compare_folders(
                        &cfg_path,
                        CompareMethod::SizeDate,
                        Some(&out_csv),
                        &stop,
                        &log,
                    ) {
                        emit(&log, tfg("log.error", &[e.to_string()]));
                    }
                }
                Mode::Cleanup => {
                    let mut total = 0usize;
                    for p in &paths {
                        if stop.is_set() {
                            break;
                        }
                        total += cleanup_empty_folders(p, &stop, &log);
                    }
                    emit(&log, tfg("log.cleanup_done", &[total.to_string()]));
                }
                Mode::Copy => {
                    let cache = cfg_path.with_extension("resume.json");
                    let n = copy_files_from_list(&cfg_path, &cache, 16, true, &stop, &log);
                    emit(&log, tfg("log.copy_summary", &[n.to_string()]));
                }
                Mode::Duplicates => {
                    let out_csv = out.join(format!("duplicates_{ts}.csv"));
                    match find_duplicates_multi(
                        &paths,
                        HashAlgo::Sha256,
                        Some(&out_csv),
                        &stop,
                        &log,
                    ) {
                        Ok(groups) => {
                            let n: usize = groups.iter().map(|g| g.len()).sum();
                            emit(
                                &log,
                                tfg("log.dup_summary", &[groups.len().to_string(), n.to_string()]),
                            );
                        }
                        Err(e) => emit(&log, tfg("log.error", &[e.to_string()])),
                    }
                }
            }
            emit(
                &log,
                tfg("log.sch_finished", &[task_name, remaining.to_string()]),
            );
        });
    }

    // ==================== ВКЛАДКА «СПРАВКА» ====================

    // ==================== ВКЛАДКА «ИСТОРИЯ» ====================

    /// История выполнений: каждая операция (включая расписание и контекстное
    /// меню) попадает в журнал `Документы\RemontExplorer\logs\history.jsonl`.
    fn history_tab(&mut self, ui: &mut egui::Ui) {
        if !self.history_loaded {
            self.reload_history();
            self.history_loaded = true;
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new(self.t("hist.title")).strong().size(15.0));
            if ui.button(self.t("hist.refresh")).clicked() {
                self.reload_history();
            }
            if ui.button(self.t("hist.open_folder")).clicked() {
                let dir = remont_core::paths::logs_dir();
                let _ = std::fs::create_dir_all(&dir);
                open_result_file(&dir);
            }
            if ui.button(self.t("hist.export")).clicked() {
                let name = format!("history_{}.csv", chrono::Local::now().format("%Y%m%d_%H%M%S"));
                let path = remont_core::resolve_report_path(name);
                match remont_core::history::export_csv(&path) {
                    Ok(()) => {
                        let msg = self.tf("hist.exported", &[path.display().to_string()]);
                        self.push_log(&msg);
                        if self.ui_config.open_result {
                            open_result_file(&path);
                        }
                    }
                    Err(e) => {
                        let msg = self.tf("hist.failed", &[e.to_string()]);
                        self.push_log(&msg);
                    }
                }
            }
            if ui.button(self.t("hist.clear")).clicked() {
                self.pending = Some(Pending::Confirm {
                    title: self.t("hist.clear").into(),
                    prompt: self.t("hist.clear_confirm").into(),
                    kind: ConfirmKind::ClearHistory,
                });
            }
        });
        ui.add_space(6.0);

        let summary = self.history_summary.clone();
        ui.label(
            RichText::new(self.tf(
                "hist.count",
                &[
                    summary.total.to_string(),
                    summary.ok.to_string(),
                    summary.failed.to_string(),
                    summary.aborted.to_string(),
                ],
            ))
            .weak(),
        );
        ui.separator();

        if self.history.is_empty() {
            ui.label(self.t("hist.empty"));
            return;
        }

        // Свежие записи — сверху.
        let rows: Vec<remont_core::HistoryEntry> = self.history.iter().rev().cloned().collect();
        let mut to_open: Option<std::path::PathBuf> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("history_grid")
                .striped(true)
                .num_columns(6)
                .spacing([14.0, 4.0])
                .show(ui, |ui| {
                    for h in [
                        "hist.col.when",
                        "hist.col.mode",
                        "hist.col.paths",
                        "hist.col.outcome",
                        "hist.col.duration",
                        "hist.col.result",
                    ] {
                        ui.label(RichText::new(self.t(h)).strong());
                    }
                    ui.end_row();

                    for e in &rows {
                        let when = chrono::DateTime::parse_from_rfc3339(&e.when)
                            .map(|d| d.format("%d.%m.%Y %H:%M").to_string())
                            .unwrap_or_else(|_| e.when.clone());
                        ui.label(when);
                        ui.label(&e.title);
                        let paths = e.paths.join(", ");
                        let short = if paths.chars().count() > 60 {
                            format!("{}…", paths.chars().take(60).collect::<String>())
                        } else {
                            paths
                        };
                        ui.label(short).on_hover_text(e.paths.join("\n"));
                        let (color, key) = match e.outcome {
                            remont_core::Outcome::Ok => {
                                (egui::Color32::from_rgb(40, 167, 69), "hist.outcome.ok")
                            }
                            remont_core::Outcome::Failed => {
                                (egui::Color32::from_rgb(200, 60, 60), "hist.outcome.failed")
                            }
                            remont_core::Outcome::Aborted => {
                                (egui::Color32::from_rgb(150, 120, 40), "hist.outcome.aborted")
                            }
                        };
                        ui.label(RichText::new(self.t(key)).color(color));
                        ui.label(format!("{:.1}", e.seconds));
                        match &e.report {
                            Some(p) => {
                                if ui.button("📂").on_hover_text(p).clicked() {
                                    to_open = Some(std::path::PathBuf::from(p));
                                }
                            }
                            None => {
                                ui.label("—");
                            }
                        }
                        ui.end_row();
                    }
                });
        });
        if let Some(p) = to_open {
            open_result_file(&p);
        }
    }

    fn help_tab(&mut self, ui: &mut egui::Ui) {
        let full_w = ui.available_width();
        let avail_h = ui.available_height();
        ui.horizontal(|ui| {
            let list_w = 220.0;
            let right_margin = 12.0;
            let detail_w = (full_w - list_w - 14.0 - right_margin).max(320.0);
            ui.vertical(|ui| {
                ui.set_width(list_w);
                ui.set_height(avail_h);
                ui.label(RichText::new(self.t("lbl.modes")).strong());
                for (i, h) in i18n::help(self.ui_config.lang).iter().enumerate() {
                    ui.selectable_value(&mut self.help_sel, i, h.title);
                }
            });
            ui.separator();
            ui.vertical(|ui| {
                ui.set_width(detail_w);
                ui.set_height(avail_h);
                let h = &i18n::help(self.ui_config.lang)[self.help_sel];
                // Содержимое прокручивается: при небольшом окне длинное
                // описание не помещается целиком (раньше текст просто обрезался).
                ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .id_salt("help_scroll")
                    .show(ui, |ui| {
                        ui.set_width(detail_w - 8.0);
                        ui.heading(h.title);
                        ui.add_space(6.0);
                        ui.label(RichText::new(self.t("help.purpose")).strong());
                        ui.add(egui::Label::new(h.purpose).wrap());
                        ui.add_space(10.0);
                        ui.label(RichText::new(self.t("help.config_file")).strong());
                        ui.add(egui::Label::new(RichText::new(h.config).monospace()).wrap());
                        ui.add_space(10.0);
                        ui.label(RichText::new(self.t("help.format")).strong());
                        ui.add(egui::Label::new(h.cfg_format).wrap());
                        ui.add_space(10.0);
                        ui.label(RichText::new(self.t("help.details")).strong());
                        ui.add(egui::Label::new(h.details).wrap());
                        ui.add_space(10.0);
                        ui.label(RichText::new(self.t("help.example")).strong());
                        ui.group(|ui| {
                            ui.add(egui::Label::new(RichText::new(h.example).monospace()).wrap());
                        });
                        ui.add_space(10.0);
                        ui.label(RichText::new(self.t("help.result")).strong());
                        ui.group(|ui| {
                            ui.add(
                                egui::Label::new(RichText::new(h.result_example).monospace())
                                    .wrap(),
                            );
                        });
                        ui.add_space(8.0);
                    });
            });
        });
    }
}

/// Загрузка текста конфига (UTF-8 с BOM, затем cp1251).
fn load_text_file(path: impl AsRef<Path>) -> String {
    fs::read(path).ok().map(|b| remont_core::config::decode_config(&b)).unwrap_or_default()
}

fn save_text_file(path: impl AsRef<Path>, content: &str) -> Result<(), String> {
    let p = path.as_ref();
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(p, content.trim()).map_err(|e| e.to_string())
}

/// Разбор парного конфига в две колонки.
fn load_pair_file(path: impl AsRef<Path>) -> (String, String) {
    let bytes = match fs::read(path.as_ref()) {
        Ok(b) => b,
        Err(_) => return (String::new(), String::new()),
    };
    let text = remont_core::config::decode_config(&bytes);
    let mut left = Vec::new();
    let mut right = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut it = line.split(';');
        let a = it.next().unwrap_or("").trim().to_string();
        let b = it.next().unwrap_or("").trim().to_string();
        if !a.is_empty() || !b.is_empty() {
            left.push(a);
            right.push(b);
        }
    }
    (left.join("\n"), right.join("\n"))
}

/// Центрирование строки по горизонтали внутри авторазмерного окна.
/// `Ui::horizontal_centered` для этого не подходит: он запрашивает всю
/// доступную высоту, из-за чего модальное окно растягивается на весь экран.
/// Здесь желаемая высота — 0, поэтому окно остаётся по содержимому.
fn centered_row(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let w = ui.available_width();
    ui.allocate_ui_with_layout(
        egui::vec2(w, 0.0),
        egui::Layout::left_to_right(egui::Align::Center).with_main_align(egui::Align::Center),
        add,
    );
}

fn timestamp() -> String {
    chrono::Local::now().format("%Y%m%d_%H%M%S").to_string()
}

/// PNG приложения (общий для окна и значка в трее).
fn app_png() -> &'static [u8] {
    include_bytes!("../icons/app.png")
}

/// Иконка окна (заголовок/трей) — тот же рисунок, что и у файла exe:
/// PNG встроена в бинарник в момент сборки.
fn app_icon() -> egui::IconData {
    const PNG: &[u8] = include_bytes!("../icons/app.png");
    if let Ok(img) = image::load_from_memory(PNG) {
        let rgba = img.to_rgba8();
        let (w, h) = (rgba.width(), rgba.height());
        return egui::IconData {
            rgba: rgba.as_raw().to_vec(),
            width: w,
            height: h,
        };
    }
    // Фолбэк: маленькое однотонное изображение (иконка встроена всегда,
    // это на случай повреждённых ресурсов).
    egui::IconData {
        rgba: [30u8, 90, 200, 255].repeat(4).to_vec(),
        width: 4,
        height: 4,
    }
}

impl eframe::App for App {
    /// Финальное сохранение при закрытии окна: расписание и настройки
    /// интерфейса (главная защита — немедленные сохранения при каждой
    /// правке; здесь — подстраховка от потери последних изменений).
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.sched.save();
        save_ui_config(&self.ui_config);
    }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.poll_logs();
        // Дескриптор окна нужен трею, чтобы вернуть окно на экран, когда
        // egui не рисует кадры (окно скрыто в трее).
        if self.hwnd.load(Ordering::Relaxed) == 0 {
            if let Ok(handle) = frame.window_handle() {
                if let RawWindowHandle::Win32(w) = handle.as_raw() {
                    self.hwnd.store(w.hwnd.get(), Ordering::Relaxed);
                    // Заголовок окна: тёмный/светлый по текущей теме.
                    let dark = match self.applied_theme {
                        ThemeMode::Dark => true,
                        ThemeMode::Light => false,
                        ThemeMode::System => ctx.style().visuals.dark_mode,
                    };
                    winshell::set_dark_title(w.hwnd.get(), dark);
                }
            }
        }
        // Проверка обновлений: один раз за запуск, если включена в настройках.
        if self.ui_config.check_updates && !self.update_done && self.update_rx.is_none() {
            self.start_update_check();
        }
        self.poll_update(ctx);
        self.poll_update_download(ctx);
        self.ensure_tray(ctx);
        self.startup_actions();

        // Закрытие окна: вместо выхода — свернуть в трей (если включено).
        if ctx.input(|i| i.viewport().close_requested())
            && !self.exit_requested
            && self.ui_config.close_to_tray
            && self.tray.is_some()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            // Прячем окно средствами Win32: у скрытого окна egui не получает
            // кадров, поэтому показ выполняет поток трея.
            tray::hide_window(self.hwnd.load(Ordering::Relaxed));
            self.hidden_to_tray = true;
            self.push_log(self.t("log.tray_hint"));
            return;
        }
        // Глобальный язык — для сообщений, формируемых вне GUI-потока.
        set_global(self.ui_config.lang);

        // Планировщик: проверка раз в секунду (и перерисовка, чтобы он
        // работал даже при простое интерфейса).
        self.scheduler_tick(ctx);
        ctx.request_repaint_after(std::time::Duration::from_secs(1));

        // Применение темы при изменении.
        if self.ui_config.theme != self.applied_theme {
            self.applied_theme = self.ui_config.theme;
            ctx.set_theme(theme_preference(self.applied_theme));
            ctx.request_repaint();
        }

        // ---------- Кастомная визуальная тема ----------
        {
            let mut style = (*ctx.style()).clone();
            tune_style(&mut style);
            ctx.set_style(style);
        }

        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            // Одна строка: вкладки слева, «О приложении» — в правом краю.
            // Отступ сверху равен отступу снизу — строка стоит ровно.
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let tw = format!("  {}  ", self.t("tab.work"));
                let ts = format!("  {}  ", self.t("tab.settings"));
                let tp = format!("  {}  ", self.t("tab.schedule"));
                let ths = format!("  {}  ", self.t("tab.history"));
                let th = format!("  {}  ", self.t("tab.help"));
                // Подсветка активной вкладки
                let active = match self.top_tab {
                    TopTab::Work => Some("tab.work"),
                    TopTab::Settings => Some("tab.settings"),
                    TopTab::Schedule => Some("tab.schedule"),
                    TopTab::History => Some("tab.history"),
                    TopTab::Help => Some("tab.help"),
                };
                if self.running {
                    ui.label(RichText::new(tw).weak());
                    ui.label(RichText::new(ts).weak());
                    ui.label(RichText::new(tp).weak());
                    ui.label(RichText::new(ths).weak());
                    ui.label(RichText::new(th).weak());
                } else {
                    // Активная вкладка — только мягкий фон: без маркера слева
                    // и без цветного текста (синий текст читается странно).
                    let mut draw = |ui: &mut egui::Ui, key: &str, label: String| {
                        let selected = Some(key) == active;
                        if selected {
                            let bg = if ui.visuals().dark_mode {
                                egui::Color32::from_rgb(58, 62, 70)
                            } else {
                                egui::Color32::from_rgb(226, 232, 240)
                            };
                            ui.add(
                                egui::Button::new(RichText::new(label))
                                    .fill(bg)
                                    .stroke(egui::Stroke::NONE)
                                    .rounding(egui::Rounding::same(4.0)),
                            );
                        } else {
                            ui.selectable_value(&mut self.top_tab, match key {
                                "tab.work" => TopTab::Work,
                                "tab.settings" => TopTab::Settings,
                                "tab.schedule" => TopTab::Schedule,
                                "tab.history" => TopTab::History,
                                "tab.help" => TopTab::Help,
                                _ => TopTab::Work,
                            }, label);
                        }
                    };
                    draw(ui, "tab.work", tw.trim().to_string());
                    draw(ui, "tab.settings", ts.trim().to_string());
                    draw(ui, "tab.schedule", tp.trim().to_string());
                    draw(ui, "tab.history", ths.trim().to_string());
                    draw(ui, "tab.help", th.trim().to_string());
                }
                // Правая часть той же строки (right_to_left: первый элемент — правый).
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(self.t("btn.about")).clicked() {
                        self.pending = Some(Pending::About);
                    }
                    if let Some(info) = &self.update_info {
                        let label = self.tf("upd.banner", std::slice::from_ref(&info.version));
                        let url = info.url.clone();
                        if ui
                            .button(RichText::new(label).color(egui::Color32::from_rgb(200, 120, 0)))
                            .clicked()
                        {
                            open_url(&url);
                        }
                    }
                });
            });
            ui.add_space(6.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            match self.top_tab {
                TopTab::Work => self.work_tab(ui),
                TopTab::Settings => self.settings_tab(ui),
                TopTab::Schedule => self.schedule_tab(ui),
                TopTab::History => self.history_tab(ui),
                TopTab::Help => self.help_tab(ui),
            }
        });

        // Диалог рисуется ПОВЕРХ уже нарисованного интерфейса: прерывать кадр
        // нельзя, иначе окно приложения остаётся пустым (выглядело как
        // затемнение всего окна).
        self.pending_modal(ctx);
    }
}

/// Одноразовый перенос конфигов из папки exe (версии до 2.1 писали их рядом
/// с программой). Существующие файлы не перезаписываются.
fn migrate_legacy_config(target: &Path) {
    let legacy = Path::new("config");
    if !legacy.is_dir() {
        return;
    }
    let Ok(entries) = fs::read_dir(legacy) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let dst = target.join(entry.file_name());
        if path.is_file() && !dst.exists() {
            let _ = fs::copy(&path, &dst);
        }
    }
}

/// Аргументы командной строки.
/// `--scan "D:\\Папка"` — сразу просканировать папку (вызывается из
/// контекстного меню Проводника), `--minimized` — стартовать в трее.
fn parse_args() -> (Option<PathBuf>, bool, Option<TopTab>) {
    parse_args_from(std::env::args().skip(1))
}

/// Разбор аргументов (отдельно от `env::args`, чтобы покрыть тестами).
fn parse_args_from<I: Iterator<Item = String>>(args: I) -> (Option<PathBuf>, bool, Option<TopTab>) {
    let mut scan = None;
    let mut minimized = false;
    let mut tab = None;
    let mut args = args;
    while let Some(a) = args.next() {
        match a.as_str() {
            // Открыть приложение сразу на нужной вкладке:
            // `--tab settings|schedule|history|help|work`.
            "--tab" => {
                tab = args.next().and_then(|v| match v.trim().to_lowercase().as_str() {
                    "work" | "работа" => Some(TopTab::Work),
                    "settings" | "настройки" => Some(TopTab::Settings),
                    "schedule" | "планирование" => Some(TopTab::Schedule),
                    "history" | "история" => Some(TopTab::History),
                    "help" | "справка" => Some(TopTab::Help),
                    _ => None,
                });
            }
            "--scan" => {
                if let Some(p) = args.next() {
                    let p = p.trim_matches('"').to_string();
                    if !p.is_empty() {
                        scan = Some(PathBuf::from(p));
                    }
                }
            }
            "--minimized" => minimized = true,
            _ => {}
        }
    }
    (scan, minimized, tab)
}

/// Подключиться к консоли родительского процесса.
///
/// Приложение собрано как GUI (`windows_subsystem = "windows"`), поэтому
/// собственной консоли у него нет и `println!` в `--cli` уходил бы в никуда.
/// `AttachConsole(ATTACH_PARENT_PROCESS)` цепляется к консоли cmd/bat,
/// а `CONOUT$`/`CONIN$` переназначают стандартные потоки на неё.
#[cfg(windows)]
fn attach_parent_console() {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        AttachConsole, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE,
    };

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    // SAFETY: имена — валидные широкие строки с NUL; хэндлы проверяются.
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        let out = wide("CONOUT$");
        let h = CreateFileW(
            out.as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        if h != INVALID_HANDLE_VALUE && !h.is_null() {
            SetStdHandle(STD_OUTPUT_HANDLE, h);
            SetStdHandle(STD_ERROR_HANDLE, h);
        }
        let inp = wide("CONIN$");
        let hi = CreateFileW(
            inp.as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        if hi != INVALID_HANDLE_VALUE && !hi.is_null() {
            SetStdHandle(STD_INPUT_HANDLE, hi);
        }
    }
}

/// Вне Windows отдельная консоль не нужна.
#[cfg(not(windows))]
fn attach_parent_console() {}

fn main() -> eframe::Result<()> {
    // Консольный режим (`--cli ...`): выполняем операцию и выходим, окно
    // не создаётся. Без `--cli` аргументы обрабатывает GUI (контекстное меню).
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Режим установки обновления: новый exe заменяет старый файл и запускает
    // его. Окно не создаётся — процесс живёт несколько секунд.
    if let Some(i) = args.iter().position(|a| a == "--self-install") {
        let dst = args.get(i + 1).map(std::path::PathBuf::from);
        let pid = args.get(i + 2).and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
        if let Some(dst) = dst {
            std::process::exit(selfupdate::run_self_install(&dst, pid));
        }
        std::process::exit(2);
    }

    // Консольный разбор запускается только при --cli/--help: иначе параметры
    // окна (`--tab settings`, `--minimized`) считались бы ошибкой CLI.
    let wants_cli = args
        .iter()
        .any(|a| a == "--cli" || a == "--help" || a == "-h");
    if wants_cli {
        if args.iter().any(|a| a == "--cli") {
            attach_parent_console();
        }
        match remont_core::cli::parse(&args) {
            Ok(Some(opts)) => std::process::exit(remont_core::cli::run(&opts)),
            Ok(None) => {}
            Err(e) => {
                eprintln!("{e}\n\n{}", remont_core::cli::HELP);
                std::process::exit(2);
            }
        }
    }
    let (startup_scan, start_hidden, startup_tab) = parse_args();
    // Рабочий каталог — папка приложения (отчёты по умолчанию пишутся рядом).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let _ = std::env::set_current_dir(dir);
        }
    }
    // Конфиги живут в «Документах» пользователя (Документы\RemontExplorer\config),
    // а не рядом с exe: папка программы может быть доступна только для чтения,
    // и чистить её пользователю неудобно.
    let cfg_dir = config_dir();
    let _ = fs::create_dir_all(&cfg_dir);
    for def in &MODES {
        let p = cfg_dir.join(def.config);
        if !p.exists() {
            let _ = fs::write(&p, "");
        }
    }
    migrate_legacy_config(&cfg_dir);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1050.0, 800.0]).with_min_inner_size([800.0, 600.0]).with_icon(app_icon()),..Default::default()
    };
    eframe::run_native(
        "RemontExplorer",
        options,
        Box::new(|_cc| {
            // Тема из config/ui.json (по умолчанию Light).
            let cfg = load_ui_config();
            _cc.egui_ctx.set_theme(theme_preference(cfg.theme));
            // Глобальный язык для core (Display-ошибки) и формат отчётов.
            set_global(cfg.lang);
            remont_core::set_output_format(cfg.report_format);
            apply_output_dir(&cfg.output_dir);
            let mut app = App {
                startup_scan: startup_scan.clone(),
                start_hidden,
                startup_tab,..Default::default()
            };
            // Проверка: папка приложения доступна для записи (конфиги, отчёты,
            // архивы пишутся рядом с exe). Если нет — предупреждение в консоли.
            let probe = Path::new("config").join("__write_probe__");
            let probe_ok = fs::write(&probe, b"ok").and_then(|_| fs::remove_file(&probe)).is_ok();
            if !probe_ok {
                app.startup_warning =
                    Some(i18n::t(cfg.lang, "warn.write_failed").to_string());
            }
            Ok(Box::new(app))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(hour: u8, minute: u8, remaining: u32) -> schedule::Task {
        let mut t = schedule::Task::new(1, "scan", hour, minute, remaining);
        t.remaining = remaining;
        t
    }

    #[test]
    fn due_exactly_at_time() {
        let t = task(9, 0, 1);
        assert!(task_due(&t, "2026-11-01", 540)); // 09:00
    }

    #[test]
    fn due_within_catchup_window() {
        let t = task(9, 0, 1);
        assert!(task_due(&t, "2026-11-01", 540 + CATCHUP_MIN)); // 09:30
    }

    #[test]
    fn not_due_after_catchup_window() {
        let t = task(9, 0, 1);
        assert!(!task_due(&t, "2026-11-01", 540 + CATCHUP_MIN + 1)); // 09:31
    }

    #[test]
    fn not_due_before_time() {
        let t = task(9, 0, 1);
        assert!(!task_due(&t, "2026-11-01", 539)); // 08:59
    }

    #[test]
    fn not_due_twice_per_day() {
        let mut t = task(9, 0, 1);
        t.last_date = "2026-11-01".into();
        assert!(!task_due(&t, "2026-11-01", 545));
    }

    #[test]
    fn not_due_when_disabled_or_exhausted() {
        let mut t = task(9, 0, 1);
        t.enabled = false;
        assert!(!task_due(&t, "2026-11-01", 545));
        t.enabled = true;
        t.remaining = 0;
        assert!(!task_due(&t, "2026-11-01", 545));
    }

    #[test]
    fn schedule_roundtrip_keeps_tasks() {
        let mut s = schedule::Schedule::default();
        s.tasks.push(schedule::Task::new(7, "compare", 13, 45, 3));
        let json = serde_json::to_string(&s).unwrap();
        let back: schedule::Schedule = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tasks.len(), 1);
        assert_eq!(back.tasks[0].mode, "compare");
        assert_eq!(back.tasks[0].time_label(), "13:45");
        assert_eq!(back.tasks[0].remaining, 3);
    }

    #[test]
    fn mode_keys_roundtrip() {
        for d in MODES.iter() {
            assert_eq!(mode_from_key(mode_slug(d.mode)), Some(d.mode));
        }
        assert_eq!(mode_from_key("unknown"), None);
    }

    #[test]
    fn missed_task_detection() {
        let t = schedule::Task::new(1, "scan", 9, 30, 1);
        let today = "2026-11-01";
        // До времени запуска — не пропущено.
        assert!(!task_missed(&t, today, 9 * 60 + 29));
        // В пределах окна «догона» (ровно 30 минут) — ещё не пропущено.
        assert!(!task_missed(&t, today, 9 * 60 + 30 + 30));
        // Окно прошло — пропущено.
        assert!(task_missed(&t, today, 9 * 60 + 30 + 31));
        // Запущено сегодня — больше не пропущено.
        let mut t2 = t.clone();
        t2.last_date = today.to_string();
        assert!(!task_missed(&t2, today, 12 * 60));
        // Отключено — не пропущено.
        let mut t3 = t.clone();
        t3.enabled = false;
        assert!(!task_missed(&t3, today, 12 * 60));
    }

    #[test]
    fn exhausted_index_finds_first_enabled() {
        let mut a = schedule::Task::new(1, "scan", 9, 0, 2);
        a.remaining = 1;
        let mut b = schedule::Task::new(2, "delete", 10, 0, 2);
        b.remaining = 0;
        let mut c = schedule::Task::new(3, "copy", 11, 0, 2);
        c.remaining = 0;
        c.enabled = false;
        let tasks = [a, b.clone(), c.clone()];
        assert_eq!(exhausted_index(&tasks), Some(1));
        // Все исчерпанные отключены — ничего не находим.
        b.enabled = false;
        let tasks2 = [b, c];
        assert_eq!(exhausted_index(&tasks2), None);
    }

    #[test]
    fn total_change_keeps_remaining() {
        let mut t = schedule::Task::new(1, "scan", 9, 0, 5);
        t.remaining = 3;
        // Увеличение: остаток растёт на дельту.
        apply_total_change(&mut t, 10);
        assert_eq!(t.total, 10);
        assert_eq!(t.remaining, 8);
        // Уменьшение ниже остатка: остаток ужимается.
        apply_total_change(&mut t, 4);
        assert_eq!(t.total, 4);
        assert_eq!(t.remaining, 4);
        // До нуля: всё списано.
        apply_total_change(&mut t, 0);
        assert_eq!(t.total, 0);
        assert_eq!(t.remaining, 0);
    }

    #[test]
    fn every_translation_key_has_english() {
        // Ни один ключ, используемый в GUI, не должен остаться без перевода.
        for key in ["tab.work", "tab.schedule", "sch.title", "sch.run_now", "log.sch_run"] {
            assert!(i18n::has_en(key), "нет английского перевода: {key}");
        }
    }

    /// Разбор строковых литералов в строке кода (с учётом экранирования).
    fn literals(line: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut chars = line.char_indices().peekable();
        while let Some((_, c)) = chars.next() {
            if c != '"' {
                continue;
            }
            let mut cur = String::new();
            while let Some((_, c2)) = chars.next() {
                match c2 {
                    '\\' => {
                        if let Some((_, esc)) = chars.next() {
                            cur.push('\\');
                            cur.push(esc);
                        }
                    }
                    '"' => break,
                    _ => cur.push(c2),
                }
            }
            out.push(cur);
        }
        out
    }

    fn has_cyrillic(s: &str) -> bool {
        s.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c))
    }

    /// В коде GUI не должно оставаться русских литералов вне переводов:
    /// все строки интерфейса берутся из i18n по ключу.
    #[test]
    fn ui_strings_are_translated() {
        let src = include_str!("main.rs");
        let code = src.split("#[cfg(test)]").next().unwrap();
        // Допустимые исключения: самоназвание языка в переключателе и
        // русские алиасы аргумента `--tab` (это не тексты интерфейса).
        let allowed = [
            "Русский",
            "работа",
            "настройки",
            "планирование",
            "история",
            "справка",
        ];
        let mut bad = Vec::new();
        for (n, line) in code.lines().enumerate() {
            if line.trim_start().starts_with("//") || line.trim_start().starts_with("//!") {
                continue;
            }
            for lit in literals(line) {
                if has_cyrillic(&lit) && !allowed.contains(&lit.as_str()) {
                    bad.push(format!("{}: {}", n + 1, lit));
                }
            }
        }
        assert!(bad.is_empty(), "непереведённые строки: {:#?}", bad);
    }


    /// Расписание — один файл schedule.json на процесс: тесты, которые его
    /// читают или пишут, выполняются по одному.
    fn schedule_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// История — один файл на процесс: тесты, пишущие её, идут по одному.
    fn history_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Тестовый каталог данных: изолирует тесты от реальных «Документов».
    fn test_data_dir() -> std::path::PathBuf {
        static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        DIR.get_or_init(|| {
            let p = std::env::temp_dir().join(format!("fm_test_data_{}", std::process::id()));
            let _ = fs::create_dir_all(p.join("config"));
            std::env::set_var("REMONTEXPLORER_DATA_DIR", &p);
            p
        }).clone()
    }

    /// Тесты, меняющие рабочий каталог, выполняются строго по одному:
    /// `set_current_dir` — глобальное состояние процесса.
    static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Срез строки по длине в символах (без паники на середине UTF-8).
    fn head_of(src: &str, len: usize) -> &str {
        let mut end = len.min(src.len());
        while end > 0 && !src.is_char_boundary(end) {
            end -= 1;
        }
        &src[..end]
    }

    /// Хвост строки от позиции (позиция — начало символа).
    fn tail_of(src: &str, start: usize, len: usize) -> &str {
        let rest = &src[start..];
        head_of(rest, len)
    }

    fn cwd_lock() -> std::sync::MutexGuard<'static, ()> {
        CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Восстановление рабочего каталога при любом исходе теста.
    struct CwdGuard(std::path::PathBuf);
    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }

    /// Все ключи, реально используемые в интерфейсе, имеют английский перевод.
    #[test]
    fn every_used_key_has_english() {
        let src = include_str!("main.rs");
        let mut missing = Vec::new();
        let mut checked = 0usize;
        for (n, line) in src.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            for lit in literals(line) {
                // ключи вида "xxx.yyy" / "tab.work"
                let mut parts = lit.split('.');
                let (Some(a), Some(b), None) = (parts.next(), parts.next(), parts.next()) else {
                    continue;
                };
                if a.is_empty() || b.is_empty() || !a.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                {
                    continue;
                }
                // ключи в i18n статические; в тесте допустимо «утечь» строкой
                let key: &'static str = Box::leak(lit.clone().into_boxed_str());
                if i18n::known(key) {
                    checked += 1;
                    if !i18n::has_en(key) {
                        missing.push(format!("{}: {}", n + 1, lit));
                    }
                }
            }
        }
        assert!(missing.is_empty(), "нет английского перевода: {:#?}", missing);
        assert!(checked >= 100, "проверено всего {checked} ключей — тест не работает");
    }

    /// Тема из настроек действительно применяется к контексту egui.
    #[test]
    fn theme_preference_applies() {
        for (mode, dark) in [
            (ThemeMode::Light, false),
            (ThemeMode::Dark, true),
        ] {
            let ctx = egui::Context::default();
            let pref = match mode {
                ThemeMode::Light => egui::ThemePreference::Light,
                ThemeMode::Dark => egui::ThemePreference::Dark,
                ThemeMode::System => egui::ThemePreference::System,
            };
            ctx.set_theme(pref);
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),..Default::default()
            };
            let mut seen = None;
            let _ = ctx.run(input, |ctx| {
                seen = Some(ctx.style().visuals.dark_mode);
            });
            assert_eq!(seen, Some(dark), "тема {mode:?} не применилась");
        }
    }

    /// Все вкладки на обоих языках строятся без паник.
    #[test]
    fn all_tabs_render_in_both_languages() {
        let _d = test_data_dir();
        for lang in [Lang::Ru, Lang::En] {
            for tab in [
                TopTab::Work,
                TopTab::Settings,
                TopTab::Schedule,
                TopTab::History,
                TopTab::Help,
            ] {
                let mut app = App::default();
                app.ui_config.lang = lang;
                app.top_tab = tab;
                app.mode = Some(Mode::Scan);
                // задание, чтобы вкладка планирования рисовала редактор
                app.sched.tasks.push(schedule::Task::new(1, "scan", 9, 30, 2));
                app.sched_sel = 0;

                let ctx = egui::Context::default();
                for _ in 0..2 {
                    let input = egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(1000.0, 720.0),
                        )),..Default::default()
                    };
                    let _ = ctx.run(input, |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| match tab {
                            TopTab::Work => app.work_tab(ui),
                            TopTab::Settings => app.settings_tab(ui),
                            TopTab::Schedule => app.schedule_tab(ui),
                            TopTab::History => app.history_tab(ui),
                            TopTab::Help => app.help_tab(ui),
                        });
                    });
                }
            }
        }
    }

    /// Планировщик реально выполняет задание: сканирование пишет отчёт
    /// в указанную папку, списывает выполнение и пишет логи на выбранном языке.
    #[test]
    fn scheduler_executes_task_end_to_end() {
        let _d = test_data_dir();
        let _sched = schedule_lock();
        let _lock = cwd_lock();
        let old = std::env::current_dir().unwrap();
        let tmp = std::env::temp_dir().join(format!("fm_sched_e2e_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("config")).unwrap();
        let src_dir = tmp.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("a.txt"), b"hello").unwrap();
        fs::write(src_dir.join("b.txt"), b"world!").unwrap();
        fs::write(config_path("folder_to_list.txt"), src_dir.display().to_string()).unwrap();
        let out = tmp.join("out");
        fs::create_dir_all(&out).unwrap();
        std::env::set_current_dir(&tmp).unwrap();
        let _guard = CwdGuard(old);

        let _ = fs::remove_file(config_path("schedule.json"));
        let mut app = App::default();
        app.ui_config.lang = Lang::En;
        let mut task = schedule::Task::new(1, "scan", 9, 0, 1);
        task.output_dir = out.display().to_string();
        app.sched.tasks.push(task);

        app.run_task_now(0);
        let began = std::time::Instant::now();
        while app.running && began.elapsed() < std::time::Duration::from_secs(60) {
            app.poll_logs();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        app.poll_logs();
        assert!(!app.running, "операция не завершилась за 60 с");

        assert_eq!(app.sched.tasks[0].remaining, 0, "выполнение не списано");
        assert!(
            !app.sched.tasks[0].last_run.is_empty(),
            "не проставлена отметка последнего запуска"
        );

        let reports: Vec<_> = fs::read_dir(&out).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| n.starts_with("BATCH_SCAN_")).collect();
        assert_eq!(reports.len(), 1, "ожидался один отчёт в папке результата, есть: {reports:?}");
        let body = fs::read_to_string(out.join(&reports[0])).unwrap();
        assert!(body.contains("a.txt") && body.contains("b.txt"), "в отчёте нет файлов: {body}");

        let log_text = app.logs.iter().cloned().collect::<Vec<_>>().join("\n");
        assert!(
            !log_text.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c)),
            "при английском языке в логах осталась кириллица:\n{log_text}"
        );
        assert!(
            log_text.contains("Scheduled") || log_text.contains("Scheduler"),
            "нет сообщений планировщика:\n{log_text}"
        );
        assert!(
            log_text.contains("[DONE]") || log_text.contains("[START]"),
            "нет сообщений сканирования:\n{log_text}"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    /// Каждый ключ, передаваемый в t()/tf()/tg()/tfg(), существует в словаре.
    /// Ловит опечатки: иначе вместо текста в интерфейсе показался бы сам ключ.
    #[test]
    fn used_keys_exist() {
        let src = include_str!("main.rs");
        let mut missing = Vec::new();
        let mut checked = 0usize;
        for (n, line) in src.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            for pat in ["self.t(", "self.tf(", "tfg(", "tg(", "i18n::t("] {
                let mut rest = line;
                while let Some(pos) = rest.find(pat) {
                    let after = &rest[pos + pat.len()..];
                    if let Some(stripped) = after.strip_prefix('"') {
                        if let Some(endq) = stripped.find('"') {
                            let key = &stripped[..endq];
                            if key.contains('.') {
                                checked += 1;
                                let k: &'static str =
                                    Box::leak(key.to_string().into_boxed_str());
                                if !i18n::known(k) {
                                    missing.push(format!("{}: {}", n + 1, key));
                                }
                            }
                        }
                    }
                    rest = after;
                }
            }
        }
        assert!(missing.is_empty(), "неизвестные ключи (опечатки?): {:#?}", missing);
        assert!(checked >= 100, "тест нашёл всего {checked} ключей — парсер не работает");
    }

    /// Английская справка полностью переведена (ни одной кириллической буквы).
    #[test]
    fn english_help_is_translated() {
        for (i, h) in i18n::help(Lang::En).iter().enumerate() {
            let fields = [
                ("title", h.title),
                ("purpose", h.purpose),
                ("cfg_format", h.cfg_format),
                ("details", h.details),
                ("example", h.example),
                ("result_example", h.result_example),
            ];
            for (name, val) in fields {
                assert!(!has_cyrillic(val), "режим {i}, поле {name}: {val}");
            }
        }
        // И русская справка на месте (не подменена английской).
        assert!(has_cyrillic(i18n::help(Lang::Ru)[0].purpose));
    }

    /// Реальный диалог (не копия): окно должно быть компактным, а не на весь
    /// экран. Проверяются все три вида модальных окон.
    #[test]
    fn modal_dialogs_are_compact() {
        let _d = test_data_dir();
        let cases: Vec<(&str, Pending)> = vec![
            (
                "confirm",
                Pending::Confirm {
                    title: "Архивирование".into(),
                    prompt: "Упаковать созданные папки в ZIP и удалить оригиналы?".into(),
                    kind: ConfirmKind::CreateZip,
                },
            ),
            ("compare", Pending::CompareMethod { selected: 0 }),
            (
                "dups",
                Pending::DupAlgo {
                    paths: vec![std::path::PathBuf::from("/tmp")],
                },
            ),
            ("about", Pending::About),
        ];
        for (name, pending) in cases {
            let mut app = App {
                pending: Some(pending),..Default::default()
            };
            let ctx = egui::Context::default();
            for _ in 0..3 {
                let input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1200.0, 800.0),
                    )),..Default::default()
                };
                let _ = ctx.run(input, |ctx| {
                    assert!(app.pending_modal(ctx), "диалог {name} не показан");
                });
            }
            let rect = ctx.memory(|m| m.area_rect(egui::Id::new("fm_modal"))).unwrap_or_else(|| panic!("окно диалога {name} не найдено"));
            println!(
                "диалог {name}: {:.0} x {:.0}",
                rect.width(),
                rect.height()
            );
            assert!(
                rect.height() < 300.0,
                "диалог {name} растянут по вертикали: {:.0} px (экран 800)",
                rect.height()
            );
            assert!(
                rect.width() >= 380.0 && rect.width() < 900.0,
                "диалог {name} имеет странную ширину: {:.0} px",
                rect.width()
            );
        }
    }

    /// Требования к кнопкам: «Старт» зелёная, «Остановить» ярко-красная и
    /// показывается всегда (яркая — когда её можно нажать).
    #[test]
    fn buttons_have_required_style() {
        let src = include_str!("main.rs");
        assert!(
            src.contains("fill(egui::Color32::from_rgb(40, 167, 69))"),
            "у кнопки «Старт» пропала зелёная заливка"
        );
        assert!(
            src.contains("fill(egui::Color32::from_rgb(225, 20, 20))"),
            "у кнопки «Остановить» пропала ярко-красная заливка"
        );
        assert!(
            src.contains("let stop_enabled = self.running;"),
            "«Остановить» должна активироваться во время операции"
        );
        assert!(
            src.contains("let start_enabled = !self.running && self.mode.is_some();"),
            "«Старт» должна быть активна только при выбранном режиме и простое"
        );
        // Обе кнопки присутствуют в панели одновременно (не взаимоисключающие).
        let rest = &src[src.find("fn work_tab").expect("work_tab не найден")..];
        let work = &rest[..rest.find("if let Some(m) = self.mode").expect("конец панели кнопок не найден")];
        assert!(work.contains("btn.start"), "в панели нет кнопки «Старт»");
        assert!(work.contains("btn.stop"), "в панели нет кнопки «Остановить»");
    }

    /// Название приложения и автор в окне «О приложении».
    #[test]
    fn app_is_renamed_and_credits_author() {
        assert_eq!(i18n::t(Lang::Ru, "app.title"), "RemontExplorer");
        assert_eq!(i18n::t(Lang::En, "app.title"), "RemontExplorer");
        assert!(i18n::t(Lang::Ru, "about.author").contains("Иван Евсеев"));
        assert!(i18n::t(Lang::Ru, "about.author").contains("Ivan Evseev"));
        assert!(i18n::t(Lang::En, "about.author").contains("Ivan Evseev"));
        // Заголовок окна ОС — тоже новое имя.
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет блока тестов")];
        assert!(
            code.contains("\"RemontExplorer\","),
            "заголовок окна не переименован"
        );
        // старое название собирается из частей, чтобы не встречаться в коде
        let old_name = ["Ivan Evseev", "'s File", "Manager"].concat();
        assert!(!code.contains(&old_name), "осталось старое название");
        // Версия убрана из правого верхнего угла (осталась в «О приложении»).
        assert!(!code.contains("app.version"), "версия снова в шапке интерфейса");
        assert!(code.contains("about.version"), "версия пропала из «О приложении»");
    }

    /// Разбор командной строки (вызов из контекстного меню Проводника).
    #[test]
    fn parses_command_line() {
        let (scan, min, _tab) = parse_args_from(
            vec!["--scan".to_string(), "\"D:\\Data\\Temp\"".to_string()].into_iter(),
        );
        assert_eq!(scan, Some(PathBuf::from("D:\\Data\\Temp")), "кавычки не сняты");
        assert!(!min);

        let (scan, min, _tab) = parse_args_from(vec!["--minimized".to_string()].into_iter());
        assert_eq!(scan, None);
        assert!(min);

        let (scan, _, _tab) = parse_args_from(
            vec!["--scan".to_string(), "D:\\Папка с пробелом".to_string()].into_iter(),
        );
        assert_eq!(scan, Some(PathBuf::from("D:\\Папка с пробелом")));

        // Без аргументов — обычный запуск.
        let (scan, min, _tab) = parse_args_from(vec!["что-то".to_string()].into_iter());
        assert_eq!(scan, None);
        assert!(!min);

        // Ключ без значения не должен ломать запуск.
        let (scan, _, _tab) = parse_args_from(vec!["--scan".to_string()].into_iter());
        assert_eq!(scan, None);
    }

    /// Настройка «сворачивать в трей» сохраняется в ui.json.
    #[test]
    fn tray_setting_persists() {
        let cfg = UiConfig {
            theme: ThemeMode::Dark,
            lang: Lang::En,
            close_to_tray: false,
            report_format: OutputFormat::Xlsx,
            output_dir: "D:\\Reports".to_string(),
            open_result: true,
            check_updates: false,
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: UiConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.theme, ThemeMode::Dark);
        assert_eq!(back.lang, Lang::En);
        assert!(!back.close_to_tray);
        assert_eq!(back.report_format, OutputFormat::Xlsx);
        assert_eq!(back.output_dir, "D:\\Reports");
        assert!(back.open_result);
        assert!(!back.check_updates);
        // Старый файл без поля читается с значением по умолчанию.
        let old: UiConfig = serde_json::from_str(r#"{"theme":"light","lang":"ru"}"#).unwrap();
        assert!(old.close_to_tray, "по умолчанию сворачивание включено");
    }

    // ==================== ПЛАНИРОВЩИК ====================

    fn wait_op(app: &mut App, secs: u64) {
        let began = std::time::Instant::now();
        while app.running && began.elapsed() < std::time::Duration::from_secs(secs) {
            app.poll_logs();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        app.poll_logs();
        assert!(!app.running, "операция не завершилась за {secs} с");
    }

    fn sched_task(mode: &str, out: &std::path::Path) -> schedule::Task {
        let mut t = schedule::Task::new(1, mode, 9, 0, 1);
        t.output_dir = out.display().to_string();
        t
    }

    fn logs_of(app: &App) -> String {
        app.logs.iter().cloned().collect::<Vec<_>>().join("\n")
    }

    fn find_report(dir: &std::path::Path, prefix: &str) -> Option<std::path::PathBuf> {
        fs::read_dir(dir).ok()?.filter_map(|e| e.ok()).map(|e| e.path()).find(|p| {
                p.file_name().map(|n| n.to_string_lossy().starts_with(prefix)).unwrap_or(false)
            })
    }

    /// Каждый из семи режимов реально выполняется заданием планировщика.
    #[test]
    fn scheduler_runs_each_mode() {
        let _d = test_data_dir();
        let _sched = schedule_lock();
        let _lock = cwd_lock();
        let old = std::env::current_dir().unwrap();
        let tmp = std::env::temp_dir().join(format!("fm_sched_modes_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("config")).unwrap();
        let out = tmp.join("out");
        fs::create_dir_all(&out).unwrap();
        std::env::set_current_dir(&tmp).unwrap();
        let _guard = CwdGuard(old);

        // Каждый режим — с чистым состоянием: run_task_now штатно сохраняет
        // задания в config/schedule.json, и следующий App их подхватил бы.
        let run = |mode: &str| -> App {
            let _ = fs::remove_file(config_path("schedule.json"));
            let mut app = App::default();
            app.sched.tasks.push(sched_task(mode, &out));
            app.run_task_now(0);
            wait_op(&mut app, 60);
            app
        };

        // ---------- Удаление ----------
        let victim = tmp.join("del").join("a.txt");
        fs::create_dir_all(victim.parent().unwrap()).unwrap();
        fs::write(&victim, b"x").unwrap();
        fs::write(config_path("folders_to_delete.txt"), victim.display().to_string()).unwrap();
        let app = run("delete");
        assert!(!victim.exists(), "удаление: файл остался. Логи:\n{}", logs_of(&app));
        assert_eq!(app.sched.tasks[0].remaining, 0, "удаление: выполнение не списано");

        // ---------- Создание ----------
        let made = tmp.join("create").join("A").join("B");
        fs::write(config_path("folders_to_create.txt"), made.display().to_string()).unwrap();
        let app = run("create");
        assert!(made.is_dir(), "создание: папка не создана. Логи:\n{}", logs_of(&app));

        // ---------- Сканирование ----------
        let src_dir = tmp.join("scan_src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("one.txt"), b"1").unwrap();
        fs::write(src_dir.join("two.txt"), b"22").unwrap();
        fs::write(config_path("folder_to_list.txt"), src_dir.display().to_string()).unwrap();
        let app = run("scan");
        assert_eq!(app.sched.tasks[0].remaining, 0, "сканирование: выполнение не списано");
        let scan_report = find_report(&out, "BATCH_SCAN_").expect("сканирование: отчёт не создан");
        let body = fs::read_to_string(&scan_report).unwrap();
        assert!(body.contains("one.txt") && body.contains("two.txt"), "сканирование: нет файлов в отчёте");

        // ---------- Сравнение ----------
        let a_dir = tmp.join("cmp_a");
        let b_dir = tmp.join("cmp_b");
        fs::create_dir_all(&a_dir).unwrap();
        fs::create_dir_all(&b_dir).unwrap();
        fs::write(a_dir.join("same.txt"), b"equal").unwrap();
        fs::write(b_dir.join("same.txt"), b"equal").unwrap();
        fs::write(a_dir.join("only_a.txt"), b"a").unwrap();
        fs::write(
            config_path("folders_to_compare.txt"),
            format!("{};{}", a_dir.display(), b_dir.display()),
        ).unwrap();
        let app = run("compare");
        assert_eq!(app.sched.tasks[0].remaining, 0, "сравнение: выполнение не списано");
        let cmp_report = find_report(&out, "compare_").expect("сравнение: отчёт не создан");
        let cmp_body = fs::read_to_string(&cmp_report).unwrap();
        assert!(cmp_body.contains("only_a.txt"), "сравнение: нет различий в отчёте");

        // ---------- Очистка ----------
        let tree = tmp.join("cleanup");
        let empty_sub = tree.join("empty_sub");
        fs::create_dir_all(&empty_sub).unwrap();
        fs::write(tree.join("keep.txt"), b"k").unwrap();
        fs::write(config_path("folders_to_cleanup.txt"), tree.display().to_string()).unwrap();
        let app = run("cleanup");
        assert_eq!(app.sched.tasks[0].remaining, 0, "очистка: выполнение не списано");
        assert!(!empty_sub.exists(), "очистка: пустая подпапка осталась");
        assert!(tree.join("keep.txt").exists(), "очистка: удалила файл");

        // ---------- Копирование ----------
        let copy_src = tmp.join("copy_src").join("c.txt");
        fs::create_dir_all(copy_src.parent().unwrap()).unwrap();
        fs::write(&copy_src, b"payload").unwrap();
        let copy_dst = tmp.join("copy_dst").join("c.txt");
        fs::create_dir_all(copy_dst.parent().unwrap()).unwrap();
        fs::write(
            config_path("from_to.csv"),
            format!("{};{}", copy_src.display(), copy_dst.display()),
        ).unwrap();
        let app = run("copy");
        assert_eq!(app.sched.tasks[0].remaining, 0, "копирование: выполнение не списано");
        assert_eq!(
            fs::read(&copy_dst).ok().as_deref(),
            Some(&b"payload"[..]),
            "копирование: файл не скопирован"
        );

        // ---------- Дубликаты ----------
        let dup_a = tmp.join("dup_a");
        let dup_b = tmp.join("dup_b");
        fs::create_dir_all(&dup_a).unwrap();
        fs::create_dir_all(&dup_b).unwrap();
        fs::write(dup_a.join("photo.jpg"), vec![7u8; 4096]).unwrap();
        fs::write(dup_b.join("photo_copy.jpg"), vec![7u8; 4096]).unwrap();
        fs::write(dup_a.join("unique.jpg"), vec![1u8; 100]).unwrap();
        fs::write(
            config_path("folders_to_duplicates.txt"),
            format!("{}\n{}", dup_a.display(), dup_b.display()),
        ).unwrap();
        let app = run("duplicates");
        assert_eq!(app.sched.tasks[0].remaining, 0, "дубликаты: выполнение не списано");
        let dup_report = find_report(&out, "duplicates_").expect("дубликаты: отчёт не создан");
        let dup_body = fs::read_to_string(&dup_report).unwrap();
        assert!(
            dup_body.contains("photo.jpg") && dup_body.contains("photo_copy.jpg"),
            "дубликаты: пара не найдена"
        );

        // ---------- Пустой список: задание пропускается ----------
        fs::write(config_path("folders_to_cleanup.txt"), "").unwrap();
        let _ = fs::remove_file(config_path("schedule.json"));
        let mut app = App::default();
        app.sched.tasks.push(sched_task("cleanup", &out));
        app.run_task_now(0);
        assert!(!app.running, "задание с пустым списком не должно запускаться");
        assert_eq!(app.sched.tasks[0].remaining, 1, "выполнение списано зря");
        assert!(
            logs_of(&app).contains("skipped") || logs_of(&app).contains("пропущено"),
            "нет сообщения о пропуске: {}",
            logs_of(&app)
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    /// Срабатывание по времени: один раз в сутки, с расходом выполнений.
    #[test]
    fn scheduler_tick_fires_once_per_day() {
        let _d = test_data_dir();
        let _sched = schedule_lock();
        let _lock = cwd_lock();
        use chrono::Timelike;
        let old = std::env::current_dir().unwrap();
        let tmp = std::env::temp_dir().join(format!("fm_sched_tick_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("config")).unwrap();
        let src_dir = tmp.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("f.txt"), b"z").unwrap();
        let out = tmp.join("out");
        fs::create_dir_all(&out).unwrap();
        std::env::set_current_dir(&tmp).unwrap();
        let _guard = CwdGuard(old);
        fs::write(config_path("folder_to_list.txt"), src_dir.display().to_string()).unwrap();

        // Время задания — минуту назад: гарантированно в окне «догона».
        let now = chrono::Local::now();
        let (h, m) = if now.minute() == 0 {
            ((now.hour() + 23) % 24, 59)
        } else {
            (now.hour(), now.minute() - 1)
        };

        let _ = fs::remove_file(config_path("schedule.json"));
        let mut app = App::default();
        let mut t = sched_task("scan", &out);
        t.hour = h as u8;
        t.minute = m as u8;
        t.remaining = 2;
        t.total = 2;
        app.sched.tasks.push(t);

        let ctx = egui::Context::default();
        app.scheduler_tick(&ctx);
        assert!(app.running, "задание не сработало по расписанию");
        wait_op(&mut app, 30);
        assert_eq!(app.sched.tasks[0].remaining, 1, "выполнение не списано");

        // Главное: задание не просто «стартовало», а создало отчёт.
        let reports: Vec<_> = fs::read_dir(&out)
            .expect("нет папки результата")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("BATCH_SCAN_"))
            .collect();
        assert_eq!(reports.len(), 1, "по расписанию не создан отчёт: {reports:?}");
        let body = fs::read_to_string(out.join(&reports[0])).unwrap();
        assert!(body.contains("f.txt"), "в отчёте нет файла источника: {body}");
        assert_eq!(
            app.sched.tasks[0].last_date,
            chrono::Local::now().format("%Y-%m-%d").to_string(),
            "не записана дата запуска"
        );

        // Повторный тик в тот же день — второй запуск недопустим.
        app.sched_last_tick = None;
        app.scheduler_tick(&ctx);
        assert!(!app.running, "задание запустилось повторно в тот же день");
        assert_eq!(app.sched.tasks[0].remaining, 1);

        // Исчерпание: включённое задание без остатка отключается.
        app.sched.tasks[0].remaining = 0;
        app.sched.tasks[0].enabled = true;
        app.sched_last_tick = None;
        app.scheduler_tick(&ctx);
        assert!(!app.sched.tasks[0].enabled, "исчерпанное задание не отключилось");
        assert!(
            logs_of(&app).contains("Runs remaining") || logs_of(&app).contains("выполнения"),
            "нет сообщения об исчерпании"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    /// Пока выполняется другая операция, задание не стартует.
    #[test]
    fn scheduler_does_not_start_while_busy() {
        let _d = test_data_dir();
        let _sched = schedule_lock();
        use chrono::Timelike;
        // В общем тестовом каталоге могли остаться задания других тестов.
        let _ = fs::remove_file(config_path("schedule.json"));
        let mut app = App::default();
        let now = chrono::Local::now();
        let mut t = schedule::Task::new(1, "scan", now.hour() as u8, now.minute() as u8, 1);
        t.remaining = 1;
        app.sched.tasks.push(t);
        app.running = true; // имитация активной операции

        let ctx = egui::Context::default();
        app.scheduler_tick(&ctx);
        assert_eq!(
            app.sched.tasks[0].remaining, 1,
            "задание запустилось во время другой операции"
        );
        assert!(
            app.sched.tasks[0].last_run.is_empty(),
            "у задания появилась отметка запуска"
        );
    }

    /// Конфиги живут в «Документах» пользователя, а не рядом с exe.
    #[test]
    fn configs_live_in_user_documents() {
        let _lock = cwd_lock();
        let _d = test_data_dir();
        let dir = config_dir();
        assert!(
            dir.starts_with(data_dir()),
            "каталог конфигов вне каталога данных: {}",
            dir.display()
        );
        assert_eq!(
            dir.file_name().and_then(|n| n.to_str()),
            Some("config"),
            "каталог конфигов должен называться config"
        );
        // Путь конфига — абсолютный и внутри каталога данных.
        let p = config_path("folders_to_delete.txt");
        assert!(p.is_absolute(), "путь конфига относительный: {}", p.display());
        assert!(p.starts_with(&dir));

        // В коде не должно остаться записи конфигов рядом с exe.
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет блока тестов")];
        assert!(
            code.contains("dirs::document_dir()"),
            "каталог данных не привязан к «Документам»"
        );
        assert!(
            !code.contains("create_dir_all(\"config\")"),
            "конфиг снова создаётся в текущем каталоге (рядом с exe)"
        );
        assert!(
            !code.contains("fs::write(\"config/"),
            "запись конфига по относительному пути"
        );
    }

    /// Сохранение настроек реально попадает в каталог данных.
    #[test]
    fn settings_are_saved_into_data_dir() {
        let _lock = cwd_lock();
        let _d = test_data_dir();
        let cfg = UiConfig {
            close_to_tray: true,..Default::default()
        };
        save_ui_config(&cfg);
        let file = config_path("ui.json");
        assert!(file.exists(), "ui.json не создан: {}", file.display());
        let body = fs::read_to_string(&file).unwrap();
        assert!(body.contains("close_to_tray"), "в файле нет настроек: {body}");
        // и читается обратно
        let back = load_ui_config();
        assert!(back.close_to_tray);
    }

    /// Кнопка «Открыть файл» есть у всех конфигов и создаёт файл, если его нет.
    #[test]
    fn open_config_button_available_for_all_configs() {
        let _d = test_data_dir();
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет блока тестов")];
        // 1) кнопка есть в редакторе одиночного и парного конфигов,
        //    в секции «Интерфейс» (ui.json) и в планировщике (schedule.json)
        assert!(
            code.matches("btn.open_config").count() >= 4,
            "кнопка «Открыть файл» есть не везде"
        );
        assert!(code.contains("config_path(\"ui.json\")"), "нет кнопки для ui.json");
        assert!(
            code.contains("config_path(\"schedule.json\")"),
            "нет кнопки для schedule.json"
        );

        // 2) отсутствующий файл создаётся (иначе редактор откроет пустоту с ошибкой)
        let p = config_path("folders_to_delete.txt");
        let _ = fs::remove_file(&p);
        ensure_config_file(&p);
        assert!(p.exists(), "файл конфигурации не создан: {}", p.display());
        assert_eq!(fs::read(&p).unwrap().len(), 0);
        ensure_config_file(&p); // повторный вызов ничего не ломает
        assert!(p.exists());
    }

    /// Пункт трея «Закрыть приложение» действительно закрывает приложение,
    /// а показ окна работает без участия egui (у скрытого окна нет кадров).
    #[test]
    fn tray_has_close_application_item() {
        assert_eq!(i18n::t(Lang::Ru, "tray.exit"), "Закрыть приложение");
        assert_eq!(i18n::t(Lang::En, "tray.exit"), "Close application");
        assert_eq!(i18n::t(Lang::Ru, "tray.open"), "Открыть RemontExplorer");

        let tray_src = include_str!("tray.rs");
        // События забирает отдельный поток — иначе при скрытом окне их никто
        // не обработает (egui не рисует кадры).
        assert!(tray_src.contains("std::thread::spawn"), "нет потока трея");
        assert!(tray_src.contains("try_recv"), "поток не читает события");
        // Показ окна — напрямую через Win32.
        assert!(tray_src.contains("ShowWindow"), "окно не показывается средствами Win32");
        assert!(tray_src.contains("show_window"), "нет функции показа окна");
        // Закрытие приложения — гарантированное завершение процесса.
        let close_pos = tray_src.find("std::process::exit(0)").expect("пункт «Закрыть приложение» не завершает процесс");
        let open_pos = tray_src.find("reveal(&hwnd, &ctx)").expect("нет показа окна из меню");
        assert!(open_pos < close_pos, "порядок обработки пунктов меню нарушен");

        // Сворачивание прячет окно напрямую, а не командой egui
        // (иначе поток трея показал бы окно, а egui снова его спрятал).
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет блока тестов")];
        assert!(code.contains("tray::hide_window"), "окно не прячется в трей");
        assert!(
            !code.contains("ViewportCommand::Visible(false)"),
            "окно прячется командой egui — конфликтует с показом из трея"
        );
        // Дескриптор окна берётся у eframe.
        assert!(code.contains("window_handle()"), "не получен дескриптор окна");
    }

    /// Исправления интерфейса: тема, диалог поверх, загрузка файла, «Конфиг».
    #[test]
    fn ui_responsiveness_fixes() {
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет блока тестов")];

        // Тема применяется сразу при клике (иначе переключение «не работало»).
        assert!(
            code.contains("ui.ctx().set_theme"),
            "тема не применяется из настроек"
        );
        assert!(
            code.contains("fn theme_preference"),
            "нет единой функции выбора темы"
        );
        // Ничто не должно сбрасывать applied_theme до применения.
        assert!(
            !code.contains(
                "if self.ui_config.theme != self.applied_theme {\n            self.applied_theme = self.ui_config.theme;\n        }"
            ),
            "applied_theme сбрасывается без применения темы"
        );

        // Диалог рисуется поверх интерфейса: кадр не прерывается.
        assert!(
            !code.contains("if self.pending_modal(ctx) {"),
            "кадр прерывается — интерфейс не рисуется под диалогом"
        );
        assert!(
            code.contains("self.pending_modal(ctx);"),
            "диалог не вызывается после отрисовки интерфейса"
        );

        // «Загрузить из файла» открывает выбор файла, а не перечитывает конфиг.
        assert!(code.contains("pick_file()"), "нет диалога выбора файла");
        assert!(
            code.matches("pick_file()").count() >= 4,
            "кнопка загрузки не открывает выбор файла"
        );

        // Кнопка называется «Конфиг».
        assert_eq!(i18n::t(Lang::Ru, "btn.open_config"), "📄 Конфиг");
        assert_eq!(i18n::t(Lang::En, "btn.open_config"), "📄 Config");
    }

    /// Замечания по интерфейсу: без подсказки, без заголовка, один блок настроек,
    /// подпись меню Проводника на языке интерфейса.
    #[test]
    fn ui_layout_follows_latest_requirements() {
        let _d = test_data_dir();
        // 1) подсказка про профиль пользователя удалена
        assert!(!i18n::known("win.hint"), "подсказка всё ещё в словаре");

        // 2) подпись пункта меню Проводника зависит от языка
        assert_eq!(
            winshell::menu_caption(true),
            "Просканировать папку в RemontExplorer"
        );
        assert_eq!(winshell::menu_caption(false), "Scan with RemontExplorer");

        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет блока тестов")];

        // 3) заголовок приложения в левом верхнем углу убран
        assert!(
            !code.contains(r#"ui.heading(self.t("app.title"))"#),
            "заголовок RemontExplorer всё ещё в шапке"
        );

        // 4) настройки одним блоком: нет секций «Интерфейс»/«Windows»
        assert!(!code.contains(r#"self.t("ui.section")"#), "осталась секция «Интерфейс»");
        assert!(!code.contains(r#"self.t("win.section")"#), "осталась секция «Windows»");
        assert!(code.contains("fn settings_options"), "нет единого блока настроек");

        // 5) папка результатов и галочка автопоказа
        assert_eq!(i18n::t(Lang::Ru, "ui.output_dir"), "Папка для результатов:");
        assert_eq!(
            i18n::t(Lang::En, "ui.open_result"),
            "Open the result file after the operation"
        );
        assert!(code.contains("apply_output_dir"), "папка не применяется");
        assert!(
            code.contains("remont_core::last_report()"),
            "результат не открывается после операции"
        );
    }

    /// Файл результата открывается через ShellExecute с абсолютным путём:
    /// `cmd /C start` искажал кириллицу в именах (Windows «не находит файл»).
    #[test]
    fn result_file_opens_without_cmd() {
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет блока тестов")];
        assert!(
            code.contains("ShellExecuteW"),
            "файл открывается не через ShellExecute"
        );
        assert!(
            code.contains("canonicalize"),
            "путь не приводится к абсолютному"
        );
        // В функции открытия результата не должно быть вызова cmd.
        let start = code.find("fn open_result_file").expect("нет функции");
        let end = start + code[start..].find("\n}\n").unwrap_or(600);
        let body = &code[start..end];
        assert!(
            !body.contains("cmd"),
            "открытие всё ещё идёт через командную строку"
        );
    }

    /// «О приложении» — в одной строке с вкладками, у правого края:
    /// отдельной верхней строки больше нет, кнопка идёт после вкладок
    /// внутри right_to_left-раскладки.
    #[test]
    fn about_button_shares_row_with_tabs() {
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет блока тестов")];
        let start = code
            .find("TopBottomPanel::top(\"header\")")
            .expect("нет шапки");
        let end = start + code[start..].find("CentralPanel::default()").expect("нет конца шапки");
        let header = &code[start..end];

        assert_eq!(
            header.matches("ui.horizontal(").count(),
            1,
            "в шапке должна быть ровно одна строка"
        );
        let tabs = header.find("tab.work").expect("нет вкладок");
        let about = header.find("btn.about").expect("нет кнопки «О приложении»");
        assert!(
            about > tabs,
            "кнопка «О приложении» должна быть в строке вкладок, после них"
        );
        let rtl = header.find("right_to_left").expect("нет правого выравнивания");
        assert!(rtl < about, "кнопка не прижата к правому краю");
    }

    // ==================== НОВЫЕ ВОЗМОЖНОСТИ ====================

    /// Настройки новых функций сохраняются и читаются из ui.json.
    #[test]
    fn new_settings_roundtrip() {
        let _d = test_data_dir();
        let cfg = UiConfig {
            notify: false,
            delete_mode: remont_core::DeleteMode::Archive,
            verify_copy: true,
            check_space: false,
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: UiConfig = serde_json::from_str(&json).unwrap();
        assert!(!back.notify);
        assert_eq!(back.delete_mode, remont_core::DeleteMode::Archive);
        assert!(back.verify_copy);
        assert!(!back.check_space);

        // Старый файл без новых полей читается с безопасными значениями.
        let old = r#"{"theme":"light","lang":"ru","close_to_tray":true,"report_format":"csv","output_dir":"","open_result":false,"check_updates":true}"#;
        let back: UiConfig = serde_json::from_str(old).unwrap();
        assert!(back.notify, "уведомления должны быть включены по умолчанию");
        assert_eq!(back.delete_mode, remont_core::DeleteMode::Permanent);
        assert!(!back.verify_copy);
        assert!(back.check_space);
    }

    /// Завершение операции пишет историю: режим, пути, итог, длительность.
    #[test]
    fn finish_op_writes_history_entry() {
        let _d = test_data_dir();
        let _g = history_lock();
        let _ = remont_core::history::clear();
        let mut app = App {
            op_meta: Some(OpMeta {
                mode: "scan".into(),
                title: "Сканирование".into(),
                paths: vec![r"D:\ОПС 1".into()],
                source: "gui".into(),
            }),
            ..Default::default()
        };
        app.op_started = Some(std::time::Instant::now() - std::time::Duration::from_millis(1500));
        app.push_log("Готово: 10 файлов");
        app.ui_config.notify = false; // уведомление не проверяем в тесте

        app.finish_op();

        let all = remont_core::history::read_all();
        assert_eq!(all.len(), 1, "запись не создана: {all:?}");
        let e = &all[0];
        assert_eq!(e.mode, "scan");
        assert_eq!(e.title, "Сканирование");
        assert_eq!(e.paths, vec![r"D:\ОПС 1".to_string()]);
        assert_eq!(e.outcome, remont_core::Outcome::Ok);
        assert!(e.seconds >= 1.0, "длительность не записана: {}", e.seconds);
        assert_eq!(app.history.len(), 1, "история в памяти не обновилась");
        let _ = remont_core::history::clear();
    }

    /// Ошибка в журнале помечает запуск как неуспешный.
    #[test]
    fn finish_op_marks_failures() {
        let _d = test_data_dir();
        let _g = history_lock();
        let _ = remont_core::history::clear();
        let mut app = App {
            op_meta: Some(OpMeta {
                mode: "copy".into(),
                title: "Копирование".into(),
                paths: Vec::new(),
                source: "schedule".into(),
            }),
            ..Default::default()
        };
        app.op_started = Some(std::time::Instant::now());
        app.push_log("Ошибка копирования D:\\a: нет доступа");
        app.ui_config.notify = false;
        app.finish_op();

        let all = remont_core::history::read_all();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].outcome, remont_core::Outcome::Failed);
        assert_eq!(all[0].source, "schedule");
        assert!(all[0].errors >= 1);
        let _ = remont_core::history::clear();
    }

    /// Операции из расписания и контекстного меню тоже попадают в историю.
    #[test]
    fn all_sources_are_tracked() {
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет тестов")];
        assert!(
            code.contains(r#"source: "schedule".into()"#),
            "расписание не передаёт метаданные операции"
        );
        assert!(
            code.contains(r#"source: "context-menu".into()"#),
            "контекстное меню не передаёт метаданные"
        );
        assert!(
            code.contains("spawn_op_meta("),
            "нет запуска операции с метаданными"
        );
    }

    /// Вкладка «История» есть в интерфейсе и читает журнал с диска.
    #[test]
    fn history_tab_is_wired() {
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет тестов")];
        assert!(code.contains("TopTab::History"));
        assert!(code.contains("fn history_tab"));
        assert!(code.contains("remont_core::history::read_all()"));
        assert!(code.contains("history::export_csv"));
        assert!(code.contains("fn reload_history"));
    }

    /// Настройка уведомлений управляет показом: при выключении notify не
    /// вызывается, при включении — вызывается с итогом операции.
    #[test]
    fn notifications_follow_setting() {
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет тестов")];
        let start = code.find("fn finish_op").expect("нет finish_op");
        let block = &code[start..start + code[start..].find("\n    fn reload_history").unwrap_or(3000)];
        assert!(block.contains("if self.ui_config.notify"));
        assert!(block.contains("notify::notify("));
        // Текст уведомления различает успех, ошибку и остановку.
        for key in ["notify.done", "notify.error", "notify.aborted"] {
            assert!(block.contains(key), "нет уведомления {key}");
        }
    }

    /// Тёмный заголовок окна применяется при смене темы и при получении hwnd.
    #[test]
    fn dark_title_follows_theme() {
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет тестов")];
        assert!(code.contains("winshell::set_dark_title"), "заголовок не настраивается");
        assert!(
            code.matches("winshell::set_dark_title").count() >= 2,
            "тема заголовка не применяется при старте окна и при переключении"
        );
        // На не-Windows вызов безопасен.
        winshell::set_dark_title(0, true);
    }

    /// Консольный режим включён в точку входа: `--cli` выполняет операцию
    /// без окна, ошибка аргументов даёт код 2.
    #[test]
    fn cli_is_wired_into_main() {
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет тестов")];
        assert!(code.contains("remont_core::cli::parse"));
        assert!(code.contains("remont_core::cli::run"));
        assert!(code.contains("std::process::exit(2)"), "нет кода ошибки аргументов");
    }

    /// Режим «Сравнение» содержит сравнение отчётов (два файла + запуск).
    #[test]
    fn report_diff_ui_exists() {
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет тестов")];
        assert!(code.contains("fn start_diff"));
        assert!(code.contains(r#"self.t("cmp.reports")"#));
        assert!(code.contains("report_diff::diff_reports"));
        assert!(code.contains("diff_old") && code.contains("diff_new"));
    }

    /// Способ удаления из настроек доходит до операции.
    #[test]
    fn delete_mode_is_passed_to_core() {
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет тестов")];
        assert!(code.contains("delete_paths_opts"));
        assert!(code.contains("let mode = self.ui_config.delete_mode"));
        assert!(
            code.contains("DeleteMode::ALL"),
            "в настройках нет выбора способа удаления"
        );
    }

    /// Проверки копирования (сверка и место) передаются в ядро.
    #[test]
    fn copy_options_are_passed() {
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет тестов")];
        assert!(code.contains("CopyOptions {"));
        assert!(code.contains("verify: self.ui_config.verify_copy"));
        assert!(code.contains("check_space: self.ui_config.check_space"));
        assert!(code.contains("copy_files_from_list_opts"));
    }


    /// Консольный режим подключается к консоли родителя: без этого при
    /// GUI-подсистеме Windows вывод `--cli` некуда писать.
    #[test]
    fn cli_attaches_parent_console() {
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет тестов")];
        assert!(code.contains("fn attach_parent_console"));
        assert!(code.contains("AttachConsole"));
        assert!(
            code.contains(r#"args.iter().any(|a| a == "--cli")"#),
            "консоль не подключается при --cli"
        );
        // Вне Windows вызов — заглушка (компилируется и ничего не делает).
        attach_parent_console();
    }


    /// Активная вкладка выделяется только фоном: без маркера «▎» и без
    /// синего текста (пользователь просил убрать оба признака).
    #[test]
    fn active_tab_is_background_only() {
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет тестов")];
        assert!(!code.contains('\u{258E}'), "маркер активной вкладки остался");
        let start = code.find("let mut draw = |ui: &mut egui::Ui").expect("нет отрисовки вкладок");
        let block = &code[start..start + 900];
        assert!(!block.contains(".color(accent)"), "текст активной вкладки всё ещё цветной");
        assert!(block.contains(".fill(bg)"), "нет выделения фоном");
        assert!(block.contains("Stroke::NONE"), "у выделения не убран контур");
    }


    /// Обычный текст интерфейса не окрашивается акцентом: иначе синими
    /// становятся все вкладки, кнопки и консоль.
    #[test]
    fn body_text_is_not_accent_blue() {
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет тестов")];
        let start = code
            .find("widgets.inactive.fg_stroke.color")
            .expect("не задан цвет обычного текста");
        let block = &code[start..start + 320];
        assert!(
            !block.contains("accent"),
            "цвет обычного текста снова акцентный (синий)"
        );
        // Нейтральные цвета заданы явно для обеих тем.
        assert!(block.contains("Color32::from_rgb(45, 50, 60)"));
        assert!(block.contains("Color32::from_rgb(200, 205, 215)"));
    }


    /// Стиль проверяется функционально: цвет обычного текста в обеих темах
    /// не акцентный, панели и фон заданы, выделение читаемое.
    #[test]
    fn tuned_style_has_neutral_text() {
        let accent = egui::Color32::from_rgb(33, 100, 220);

        let mut light = egui::Style::default();
        light.visuals.dark_mode = false;
        tune_style(&mut light);
        assert_ne!(
            light.visuals.widgets.inactive.fg_stroke.color, accent,
            "текст вкладок и кнопок снова синий"
        );
        assert_ne!(light.visuals.widgets.noninteractive.fg_stroke.color, accent);

        let mut dark = egui::Style::default();
        dark.visuals.dark_mode = true;
        tune_style(&mut dark);
        assert_ne!(dark.visuals.widgets.inactive.fg_stroke.color, accent);

        // Текст обычных кнопок заметно темнее акцента в светлой теме.
        let c = light.visuals.widgets.inactive.fg_stroke.color;
        assert!(
            (c.r() as u16 + c.g() as u16 + c.b() as u16) < 400,
            "текст недостаточно тёмный: {c:?}"
        );
    }


    /// Под шапкой нет лишней линии, а блок консоли отделён мягким фоном,
    /// а не контуром (пользователь просил убрать двойную линию).
    #[test]
    fn header_and_console_have_no_extra_borders() {
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет тестов")];
        let start = code.find("TopBottomPanel::top(\"header\")").expect("нет шапки");
        let header = &code[start..start + 2600];
        assert!(
            !header.contains("ui.separator()"),
            "под вкладками остался разделитель"
        );

        let c = code.find("let console_bg").expect("нет оформления консоли");
        let block = &code[c..c + 400];
        assert!(block.contains("Stroke::NONE"), "у консоли остался контур");
        assert!(block.contains(".fill(console_bg)"), "нет мягкого фона консоли");
    }


    /// Все флажки в блоке Windows — одного вида: без цветных статус-точек
    /// перед текстом (они сбивали выравнивание).
    #[test]
    fn windows_checkboxes_are_uniform() {
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет тестов")];
        let start = code.find("let mut menu_on = self.ctx_menu").expect("нет блока Windows");
        let block = &code[start..start + 500];
        assert!(!block.contains("●"), "осталась статус-точка у флажка");
        assert!(!block.contains("○"), "остался пустой индикатор у флажка");
        assert!(block.contains("ui.checkbox(&mut menu_on, &l_menu)"));
        assert!(block.contains("ui.checkbox(&mut auto_on, &l_auto)"));
        assert!(block.contains("ui.checkbox(&mut self.ui_config.close_to_tray, &l_tray)"));
    }

    /// Папка открывается Проводником ("explore") и без префикса \\?\:
    /// с префиксом оболочка Windows путь не открывает.
    #[test]
    fn folders_open_via_explorer_without_verbatim_prefix() {
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет тестов")];
        assert!(code.contains("fn unverbatim"));
        assert!(code.contains("open_with_shell(&abs, abs.is_dir())"));
        assert!(code.contains(r#"if is_dir { "explore" } else { "open" }"#));
        // Префикс снимается для обоих вариантов — локального и UNC.
        assert!(code.contains(r#"strip_prefix(r"\\?\UNC\")"#));
        assert!(code.contains(r#"strip_prefix(r"\\?\")"#));
    }


    /// В настройках нет заголовков секций и полос над ними — все опции
    /// одним блоком (пользователь просил убрать «Интерфейс» и «Windows»).
    #[test]
    fn settings_have_no_section_headers() {
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет тестов")];
        let start = code.find("fn settings_options").expect("нет настроек");
        let block = &code[start..start + 9000];
        assert!(!block.contains("ui.section_interface"), "остался заголовок «Интерфейс»");
        assert!(!block.contains(r#"RichText::new("Windows")"#), "остался заголовок «Windows»");
        assert!(!block.contains("theme_style"), "остались полосы-разделители секций");
    }

    /// Строка вкладок стоит с одинаковым отступом сверху и снизу.
    #[test]
    fn tabs_row_has_symmetric_padding() {
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет тестов")];
        let start = code.find("TopBottomPanel::top(\"header\")").expect("нет шапки");
        let header = &code[start..start + 400];
        assert!(
            header.contains("ui.add_space(6.0);\n            ui.horizontal"),
            "нет отступа над строкой вкладок"
        );
        let end = code[start..].find("ui.separator()").unwrap_or(header.len());
        assert!(
            code[start..start + end.min(3000)].contains("ui.add_space(6.0);"),
            "нет отступа под строкой вкладок"
        );
    }


    /// Три правки по интерфейсу: нейтральный заголовок истории, отступ
    /// блока настроек и прокрутка описания в справке.
    #[test]
    fn ui_polish_round() {
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет тестов")];

        // 1) Заголовок «История выполнений» не окрашен.
        let h = code.find("fn history_tab").expect("нет вкладки истории");
        let block = tail_of(code, h, 1500);
        let title = block.find("hist.title").expect("нет заголовка истории");
        let line = &block[title..title + 140];
        assert!(!line.contains(".color("), "заголовок истории всё ещё цветной");

        // 2) Отделён именно нижний раздел, а не верхние настройки.
        let s = code.find("fn settings_tab").expect("нет настроек");
        let head = &code[s..s + 200];
        assert!(
            !head.contains("ui.add_space(10.0)"),
            "отступ снова отрывает верхние настройки от меню"
        );
        let tab = tail_of(code, s, 3000);
        let lower = tab
            .find("ui.add_space(12.0)")
            .expect("нет отступа перед разделом режимов");
        assert!(
            tail_of(tab, lower, 200).contains("ui.horizontal"),
            "отступ стоит не перед разделом «Выберите режим»"
        );

        // 3) Описание в справке прокручивается.
        let hp = code.find("fn help_tab").expect("нет справки");
        let help = &code[hp..hp + 3000];
        assert!(help.contains("ScrollArea::vertical()"), "в справке нет прокрутки");
        assert!(help.contains(r#"id_salt("help_scroll")"#), "нет отдельного состояния прокрутки");
    }


    /// Справка рисуется в минимальном окне (800×600): длинный текст
    /// прокручивается, а не обрезается и не паникует.
    #[test]
    fn help_renders_in_small_window() {
        let _d = test_data_dir();
        let mut app = App {
            top_tab: TopTab::Help,
            help_sel: 2, // «Сканирование» — длинный раздел
            ..Default::default()
        };
        let ctx = egui::Context::default();
        for _ in 0..2 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| app.help_tab(ui));
            });
        }
        assert_eq!(app.help_sel, 2);
    }


    /// В справке есть отдельный раздел про CLI — с описанием каждого режима
    /// и примерами команд, на обоих языках.
    #[test]
    fn cli_help_section_exists() {
        let ru = i18n::help(Lang::Ru);
        let en = i18n::help(Lang::En);
        assert_eq!(ru.len(), en.len(), "разделов справки разное количество");
        let last = ru.last().expect("справка пуста");
        assert_eq!(last.title, "CLI-режим");
        assert_eq!(en.last().unwrap().title, "CLI mode");

        // Перечислены все режимы и ключевые параметры.
        for needle in [
            "--cli",
            "--scan",
            "--compare",
            "--duplicates",
            "--delete",
            "--copy",
            "--cleanup",
            "--create",
            "--diff",
            "--history",
            "--quiet",
            "--recycle",
            "--archive",
            "--verify",
            "--format",
            "--out",
        ] {
            assert!(last.purpose.contains(needle) || last.details.contains(needle) || last.cfg_format.contains(needle),
                "в разделе CLI нет упоминания {needle}");
        }
        // Есть примеры и коды возврата.
        assert!(last.example.contains("--cli --scan"));
        assert!(last.cfg_format.contains("0"));
        // Английская версия без кириллицы.
        assert!(!en.last().unwrap().details.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c)));
    }

    /// Раздел CLI рисуется и прокручивается в небольшом окне.
    #[test]
    fn cli_help_renders() {
        let _d = test_data_dir();
        let mut app = App {
            top_tab: TopTab::Help,
            help_sel: i18n::help(Lang::Ru).len() - 1,
            ..Default::default()
        };
        let ctx = egui::Context::default();
        for _ in 0..2 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| app.help_tab(ui));
            });
        }
        assert_eq!(app.help_sel, i18n::help(Lang::Ru).len() - 1);
    }


    /// Редактор настроек не наезжает на правый край: есть запас справа,
    /// поле ввода не растягивается «в бесконечность», кнопки переносятся.
    #[test]
    fn settings_editor_keeps_right_margin() {
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет тестов")];
        let s = code.find("fn settings_tab").expect("нет настроек");
        let tab = tail_of(code, s, 3600);
        assert!(
            tab.contains("let right_margin = 28.0;"),
            "мал запас справа у редактора настроек"
        );
        let e = code.find("fn single_editor").expect("нет редактора");
        let block = tail_of(code, e, 2500);
        assert!(!block.contains("f32::INFINITY"), "поле ввода снова растягивается за край");
        assert!(block.contains("ui.horizontal_wrapped"), "кнопки не переносятся");
    }


    /// Кнопки «Сохранить в файл» и «Загрузить из файла» в настройках
    /// действительно пишут и читают конфиг (пары «откуда;куда»).
    #[test]
    fn editor_save_and_load_roundtrip() {
        let _d = test_data_dir();
        let _lock = cwd_lock();
        let tmp = std::env::temp_dir().join(format!("fm_editor_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let old = std::env::current_dir().unwrap();
        std::env::set_current_dir(&tmp).unwrap();
        let _guard = CwdGuard(old);

        let mut app = App::default();
        // Режим «Копирование» — парный редактор.
        let idx = MODES.iter().position(|d| d.mode == Mode::Copy).unwrap();
        app.settings_sel = idx;
        app.settings_loaded = usize::MAX;
        app.editor_left = "D:\\src\\a.txt\nD:\\src\\b.txt".into();
        app.editor_right = "D:\\dst\\a.txt\nD:\\dst\\b.txt".into();
        app.save_pair_config(true);

        let cfg = config_path("from_to.csv");
        let body = fs::read_to_string(&cfg).expect("конфиг не сохранён");
        assert!(body.contains("a.txt"), "нет первой пары: {body}");
        assert!(body.contains("b.txt"), "нет второй пары: {body}");
        assert_eq!(body.lines().count(), 2, "строк должно быть 2: {body}");

        // Чтение обратно — тем же путём, что и кнопка «Загрузить из файла».
        let (l, r) = load_pair_file(&cfg);
        assert_eq!(l.trim(), app.editor_left);
        assert_eq!(r.trim(), app.editor_right);

        let _ = fs::remove_dir_all(&tmp);
    }

    /// Одиночный редактор: сохранение списка путей и чтение обратно.
    #[test]
    fn single_editor_save_roundtrip() {
        let _d = test_data_dir();
        let _lock = cwd_lock();
        let tmp = std::env::temp_dir().join(format!("fm_editor1_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let cfg = config_path("folders_to_delete.txt");
        let text = "D:\\one\nD:\\two\n";
        save_text_file(&cfg, text).expect("не сохранено");
        assert_eq!(load_text_file(&cfg).trim(), "D:\\one\nD:\\two");

        let _ = fs::remove_dir_all(&tmp);
    }


    /// Кнопка «Остановить» гасит запущенную операцию, и следующая запускается
    /// заново (стоп-флаг сбрасывается).
    #[test]
    fn stop_button_halts_operation() {
        let _d = test_data_dir();
        let _lock = cwd_lock();
        let tmp = std::env::temp_dir().join(format!("fm_stop_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        // Много файлов: операция должна идти достаточно долго.
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();
        for i in 0..4000 {
            fs::write(src.join(format!("f{i}.txt")), b"x").unwrap();
        }
        let old = std::env::current_dir().unwrap();
        std::env::set_current_dir(&tmp).unwrap();
        let _guard = CwdGuard(old);

        let mut app = App {
            mode: Some(Mode::Scan),
            scan_threads: 1,
            ..Default::default()
        };
        let p = src.clone();
        app.spawn_op(move |log, stop| {
            let _ = scan_folder(&p, None, 1, &stop, &log);
        });
        assert!(app.running, "операция не стартовала");

        // Нажатие «Остановить».
        app.stop.stop();
        let began = std::time::Instant::now();
        while app.running && began.elapsed() < std::time::Duration::from_secs(40) {
            app.poll_logs();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        app.poll_logs();
        assert!(!app.running, "операция не остановилась");

        // Флаг сбрасывается при следующем запуске (а не сразу после остановки):
        // иначе новая операция моментально завершилась бы.
        let p2 = src.clone();
        app.spawn_op(move |log, stop| {
            let _ = scan_folder(&p2, None, 2, &stop, &log);
        });
        assert!(app.running, "после остановки новая операция не запускается");
        assert!(!app.stop.is_set(), "стоп-флаг не сброшен при новом запуске");
        wait_op(&mut app, 60);
        assert!(!app.running);

        let _ = fs::remove_dir_all(&tmp);
    }

    /// Добавление путей в редактор: пустой → первый, далее — с новой строки.
    #[test]
    fn append_path_appends_lines() {
        let mut s = String::new();
        App::append_path(&mut s, "D:\\one");
        assert_eq!(s, "D:\\one");
        App::append_path(&mut s, "D:\\two");
        assert_eq!(s, "D:\\one\nD:\\two");
    }

    /// Задания планировщика сохраняются и читаются (добавление/удаление
    /// через интерфейс идут через этот файл).
    #[test]
    fn schedule_persists_tasks() {
        let _d = test_data_dir();
        let _sched = schedule_lock();
        let dir = config_path("schedule.json");
        let _ = fs::remove_file(&dir);

        let mut sc = schedule::Schedule::default();
        let mut t = schedule::Task::new(7, "copy", 3, 15, 5);
        t.output_dir = "D:\\out".into();
        sc.tasks.push(t);
        sc.save();

        let back = schedule::Schedule::load();
        assert_eq!(back.tasks.len(), 1, "задание не сохранилось");
        assert_eq!(back.tasks[0].mode, "copy");
        assert_eq!(back.tasks[0].hour, 3);
        assert_eq!(back.tasks[0].minute, 15);
        assert_eq!(back.tasks[0].remaining, 5);
        assert_eq!(back.tasks[0].output_dir, "D:\\out");

        let _ = fs::remove_file(&dir);
    }


    /// Настройка «автоматическая загрузка и установка» сохраняется и читается.
    #[test]
    fn auto_update_settings_roundtrip() {
        let _d = test_data_dir();
        let cfg = UiConfig {
            auto_download: false,
            auto_install: true,
            ..Default::default()
        };
        let back: UiConfig = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert!(back.auto_install);
        assert!(!back.auto_download);

        // Старый ui.json без этих полей: загрузка включена, установка — нет.
        let old = r#"{"theme":"light","lang":"ru"}"#;
        let back: UiConfig = serde_json::from_str(old).unwrap();
        assert!(back.auto_download);
        assert!(!back.auto_install, "автоустановка не должна включаться сама");
    }

    /// Автоустановка включает автозагрузку и ставит обновление сама,
    /// не обрывая выполняющуюся операцию.
    #[test]
    fn auto_install_behaviour() {
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет тестов")];
        // Автозагрузка включается вместе с автоустановкой.
        assert!(code.contains("self.ui_config.auto_download = true;"));
        // Загрузка стартует и при выключенной автозагрузке, если включена автоустановка.
        assert!(code.contains("auto_download || self.ui_config.auto_install"));
        // Установка откладывается до конца операции.
        assert!(code.contains("install_after_op"));
        assert!(code.contains("if self.install_after_op && !self.running"));
        // Установка сохраняет состояние и заменяет файл.
        let start = code.find("fn apply_update").expect("нет установки");
        let block = &code[start..start + 1600];
        assert!(block.contains("save_ui_config"));
        assert!(block.contains("self.sched.save()"));
        assert!(block.contains("selfupdate::apply"));
    }

    /// Прямая ссылка на релиз строится по шаблону (нужна при недоступном API).
    #[test]
    fn asset_url_is_built_from_version() {
        use updates::asset_url_for;
        assert_eq!(
            asset_url_for("2.3.0"),
            "https://github.com/IntruderHere/RemontExplorer/releases/download/v2.3.0/RemontExplorer-windows-x64.zip"
        );
        assert_eq!(asset_url_for("v2.3.0"), asset_url_for("2.3.0"));
    }


    /// Флажки в настройках идут одним списком: между блоками нет отбивки,
    /// из-за которой казалось, что настройки разделены.
    #[test]
    fn checkboxes_are_not_split_by_spacing() {
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет тестов")];
        let start = code.find("if is_win {").expect("нет блока Windows");
        let block = &code[start..start + 260];
        assert!(
            !block.contains("ui.add_space("),
            "перед флажками Windows осталась отбивка"
        );
    }


    /// Иконки режимов есть в шрифте egui: 🧹 (U+1F9F9) отсутствовала и
    /// рисовалась пустым квадратом. Ряд режимов переносится, а не обрезается.
    #[test]
    fn mode_icons_render_and_row_wraps() {
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет тестов")];
        assert!(!code.contains('\u{1F9F9}'), "вернулась иконка без глифа (🧹)");
        assert!(!code.contains("🧹"), "вернулась иконка без глифа");
        let start = code.find("modes_row").unwrap_or(0);
        let panel = &code[code.find("fn work_tab").unwrap()..];
        assert!(
            panel.contains("ui.horizontal_wrapped(|ui|"),
            "ряд режимов не переносится — кнопки обрезаются"
        );
        let _ = start;
    }


    /// Права администратора не нужны: конфиги и реестр — в профиле пользователя.
    /// Единственное место, где может помешать ACL, — запись exe при обновлении;
    /// об этом приложение предупреждает заранее.
    #[test]
    fn no_admin_rights_required_anywhere() {
        let src = include_str!("main.rs");
        let code = &src[..src.find("#[cfg(test)]").expect("нет тестов")];
        // Ни манифеста с requireAdministrator, ни запуска с "runas".
        assert!(!code.contains("requireAdministrator"));
        assert!(!code.contains(r#""runas""#));

        // Проверка записи в папку приложения перед установкой.
        let start = code.find("fn apply_update").expect("нет установки");
        let block = &code[start..start + 1200];
        assert!(block.contains("dir_writable"), "нет проверки прав на папку");
        assert!(block.contains("upd.no_permission"), "нет понятного сообщения");
    }

    /// Проверка записи: в обычном каталоге — да, в несуществующем — нет.
    #[test]
    fn dir_writable_detects_permissions() {
        let dir = std::env::temp_dir().join(format!("re_perm_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(dir_writable(&dir));
        assert!(!dir_writable(&dir.join("нет-такого-каталога")));
        let _ = std::fs::remove_dir_all(&dir);
    }

}
