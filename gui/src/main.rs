//! RemontExplorer GUI (egui/eframe) — порт интерфейса `filemanager 6.py`.
//! Целевая ОС — Windows. Сборка на других ОС используется только для
//! прогона тестов логики и в поставку не входит.
//! Структура 1:1: вкладки «Работа» / «Настройки»; в «Работе» — ряд кнопок
//! режимов, «Старт/Остановить», описание режима, «Консоль вывода»; в
//! «Настройках» — секция «Интерфейс» (тема, язык) + список режимов и
//! редакторы config/*.txt.
#![cfg_attr(windows, windows_subsystem = "windows")] // без консольного окна

mod i18n;
mod schedule;
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
use filemanager_core::i18n::Lang;
use filemanager_core::OutputFormat;
use filemanager_core::{
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
        }
    }
}

fn default_true() -> bool {
    true
}

fn load_ui_config() -> UiConfig {
    fs::read_to_string(config_path("ui.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_ui_config(cfg: &UiConfig) {
    if let Ok(json) = serde_json::to_string_pretty(cfg) {
        let _ = fs::create_dir_all(config_dir());
        let _ = fs::write(config_path("ui.json"), json);
    }
}

/// Режимы — порядок, файлы и описания как в Python (MODES_CONFIG).
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
    i18n::t(filemanager_core::i18n::global(), key)
}

/// Перевод с подстановкой `{}` — вариант `tg` для форматированных строк.
fn tfg(key: &'static str, args: &[String]) -> String {
    filemanager_core::i18n::fill(tg(key), args)
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
    dirs::document_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR)
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
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", ""])
            .arg(url)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
}

/// Открыть полученный файл (отчёт) в приложении по умолчанию.
fn open_result_file(path: &Path) {
    if !path.exists() {
        return;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // `start` открывает файл ассоциированным приложением (Excel для xlsx).
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", ""])
            .arg(path)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("open").arg(path).spawn();
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

/// Настройка темы egui по выбору пользователя.
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
        filemanager_core::set_output_dir(None);
    } else {
        filemanager_core::set_output_dir(Some(PathBuf::from(trimmed)));
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
    Help,
}

/// Модальные вопросы (порт messagebox/диалогов Python).
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
    /// Стартовые действия выполнены (один раз).
    startup_done: bool,
    /// Найденное обновление (версия новее текущей).
    update_info: Option<updates::UpdateInfo>,
    /// Текст ошибки проверки обновлений (показывается в «О приложении»).
    update_error: Option<String>,
    /// Проверка обновлений уже выполнялась/выполняется.
    update_done: bool,
    /// Канал результата проверки обновлений.
    update_rx: Option<mpsc::Receiver<Result<Option<updates::UpdateInfo>, String>>>,
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
            startup_done: false,
            update_info: None,
            update_error: None,
            update_done: false,
            update_rx: None,
            hwnd: std::sync::Arc::new(AtomicIsize::new(0)),
        }
    }
}

/// Потоки сканирования: config/scan_threads.txt (1..=256), иначе число ядер.
fn load_scan_threads() -> usize {
    let def = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    fs::read_to_string(config_path("scan_threads.txt"))
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|n| (1..=256).contains(n))
        .unwrap_or(def)
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
                self.update_info = info;
                self.update_error = None;
                self.update_done = true;
                self.update_rx = None;
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
        self.spawn_op(move |log, stop| {
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
            egui::Window::new(egui::RichText::new(""))
                .id(egui::Id::new("fm_modal"))
                .title_bar(false)
                .resizable(false)
                .collapsible(false)
                .anchor(egui::Align2::CENTER_CENTER, (0.0, 0.0))
                .min_size(egui::vec2(420.0, 0.0))
                .show(ctx, |ui| {
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
                                    ))
                                    .weak(),
                                );
                            });
                            ui.add_space(12.0);
                            ui.add(egui::Label::new(self.t("about.desc")).wrap());
                            ui.add_space(10.0);
                            ui.label(RichText::new(self.t("about.author")).strong());
                            ui.add_space(10.0);

                            // ---- Проверка обновлений ----
                            if let Some(info) = &self.update_info {
                                let text = self.tf("upd.available", &[info.version.clone()]);
                                ui.label(
                                    RichText::new(text)
                                        .strong()
                                        .color(egui::Color32::from_rgb(200, 120, 0)),
                                );
                                ui.add_space(6.0);
                                if ui.button(self.t("upd.download")).clicked() {
                                    open_url(&info.url);
                                }
                            } else if self.update_rx.is_some() {
                                ui.label(RichText::new(self.t("upd.checking")).weak());
                            } else if let Some(err) = &self.update_error {
                                ui.label(
                                    RichText::new(self.tf("upd.error", &[err.clone()]))
                                        .weak()
                                        .color(egui::Color32::from_rgb(180, 60, 60)),
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
        filemanager_core::i18n::fill(self.t(key), args)
    }

    /// Запуск операции в отдельном потоке (журнал и прогресс — через каналы).
    /// Паника в операции перехватывается: приложение живёт, в консоль
    /// выводится текст внутренней ошибки.
    fn spawn_op(&mut self, f: impl FnOnce(LogSink, StopToken) + Send + 'static) {
        if self.running {
            return;
        }
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
            )
            .with_lang(lang),
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
        filemanager_core::clear_last_report();
        let h = thread::spawn(move || {
            let log2 = log.clone();
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(log, stop)));
            if let Err(e) = r {
                let msg = e
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "unknown reason".into());
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
                // Настройка «открывать полученный файл после выполнения».
                if self.ui_config.open_result {
                    if let Some(path) = filemanager_core::last_report() {
                        open_result_file(&path);
                    }
                }
            }
        }
        while self.logs.len() > MAX_LOG {
            self.logs.pop_front();
        }
    }

    // ==================== ВКЛАДКА «РАБОТА» ====================

    fn work_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            for def in &MODES {
                let label = self.t(mode_key(def.mode));
                if self.running {
                    ui.label(RichText::new(label).weak());
                } else {
                    ui.selectable_value(&mut self.mode, Some(def.mode), label);
                }
            }
            ui.separator();
            // «Старт» активна, когда выбран режим и операция не выполняется.
            let start_enabled = !self.running && self.mode.is_some();
            if ui
                .add_enabled(
                    start_enabled,
                    egui::Button::new(
                        RichText::new(self.t("btn.start")).strong().color(egui::Color32::WHITE),
                    )
                    .fill(egui::Color32::from_rgb(40, 167, 69))
                    .rounding(egui::Rounding::same(4.0)),
                )
                .clicked()
            {
                self.start_action();
            }
            // «Остановить»: ярко-красная, когда её можно нажать (идёт операция).
            let stop_enabled = self.running;
            let stop_btn = if stop_enabled {
                egui::Button::new(
                    RichText::new(self.t("btn.stop")).strong().color(egui::Color32::WHITE),
                )
                .fill(egui::Color32::from_rgb(225, 20, 20))
                .rounding(egui::Rounding::same(4.0))
            } else {
                egui::Button::new(self.t("btn.stop"))
            };
            if ui.add_enabled(stop_enabled, stop_btn).clicked() {
                self.stop.stop();
                self.push_log(self.t("lbl.stopped_log"));
            }
        });

        if let Some(m) = self.mode {
            ui.add(egui::Label::new(RichText::new(self.t(desc_key(m))).weak()).wrap());
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
            ui.add(egui::ProgressBar::new(frac).text(label));
        }
        ui.add_space(2.0);

        ui.group(|ui| {
            ui.set_height(ui.available_height().max(100.0));
            if let Some(w) = &self.startup_warning {
                ui.add(
                    egui::Label::new(RichText::new(w.as_str()).color(egui::Color32::from_rgb(200, 60, 0)))
                        .wrap(),
                );
                ui.add_space(4.0);
            }
            ui.label(RichText::new(self.t("lbl.console")));
            ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    if self.logs.is_empty() {
                        ui.label(
                            RichText::new(self.t("lbl.console_empty"))
                                .weak(),
                        );
                    }
                    for line in &self.logs {
                        ui.label(RichText::new(line).monospace().size(12.0));
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

    /// Порт `start_action` из Python-GUI.
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
                    match rfd::FileDialog::new()
                        .set_title(self.t("dlg.pick_scan"))
                        .pick_folder()
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
                        let master = filemanager_core::finalize_report(PathBuf::from(&master))
                            .display()
                            .to_string();
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
                    match rfd::FileDialog::new()
                        .set_title(self.t("dlg.pick_cleanup"))
                        .pick_folder()
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
                    prompt: self.t("dlg.resume_prompt")
                        .into(),
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
                    match rfd::FileDialog::new()
                        .set_title(self.t("dlg.pick_dup"))
                        .pick_folder()
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
                let paths = read_paths_file(Path::new("config/folders_to_delete.txt"));
                self.spawn_op(move |log, stop| {
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
                });
            }
            ConfirmKind::CreateZip => {
                let paths = read_paths_file(Path::new("config/folders_to_create.txt"));
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
                self.spawn_op(move |log, stop| {
                    let cache = cfg.with_extension("resume.json");
                    let n = copy_files_from_list(&cfg, &cache, 16, true, &stop, &log);
                    emit(&log, tfg("log.copy_summary", &[n.to_string()]));
                });
            }
            ConfirmKind::SavePairMismatch => {
                self.save_pair_config(true);
            }
        }
    }

    /// Ответ «Нет» в модальном окне.
    fn confirm_no(&mut self, kind: ConfirmKind) {
        match kind {
            ConfirmKind::DeleteRecursive => {
                let paths = read_paths_file(Path::new("config/folders_to_delete.txt"));
                self.spawn_op(move |log, stop| {
                    let s = delete_paths(&paths, false, &stop, &log);
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
                let paths = read_paths_file(Path::new("config/folders_to_create.txt"));
                self.spawn_op(move |log, stop| {
                    let created = create_folders(&paths, &stop, &log);
                    emit(&log, tfg("log.create_summary", &[created.len().to_string()]));
                });
            }
            ConfirmKind::CopyResume => {
                let cfg = config_path("from_to.csv");
                self.spawn_op(move |log, stop| {
                    let cache = cfg.with_extension("resume.json");
                    let n = copy_files_from_list(&cfg, &cache, 16, false, &stop, &log);
                    emit(&log, tfg("log.copy_summary", &[n.to_string()]));
                });
            }
            ConfirmKind::SavePairMismatch => {
                // «Нет» = не сохранять
            }
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
                    egui::TextEdit::singleline(&mut self.ui_config.output_dir)
                        .desired_width(ui.available_width() - 180.0)
                        .hint_text(&l_output_hint),
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
            ui.checkbox(&mut self.ui_config.check_updates, &l_check_updates);

            if is_win {
                let mut menu_on = self.ctx_menu.unwrap_or(false);
                if ui
                    .add(egui::Checkbox::new(&mut menu_on, &l_menu))
                    .changed()
                {
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
                let mut auto_on = self.autostart.unwrap_or(false);
                if ui
                    .add(egui::Checkbox::new(&mut auto_on, &l_auto))
                    .changed()
                {
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
                ui.add(egui::Checkbox::new(
                    &mut self.ui_config.close_to_tray,
                    &l_tray,
                ));
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
            filemanager_core::set_output_format(self.ui_config.report_format);
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
            ui.ctx().request_repaint();
        }

        // Перезагрузка редактора при смене режима (как создание виджетов в Python)
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
        let full_w = ui.available_width();
        let avail_h = ui.available_height();
        ui.horizontal(|ui| {
            let list_w = 220.0;
            let right_margin = 12.0;
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
        )
        .weak());
        ui.horizontal(|ui| {
            if ui.button(self.t("btn.load")).clicked() {
                // Выбор файла на диске (раньше кнопка просто перечитывала
                // уже открытый конфиг и выглядела «неработающей»).
                if let Some(p) = rfd::FileDialog::new().pick_file() {
                    match fs::read(&p) {
                        Ok(bytes) => {
                            self.editor_single = filemanager_core::config::decode_config(&bytes);
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
        ui.add_sized(
            [f32::INFINITY, h],
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
        )
        .weak());
        ui.horizontal(|ui| {
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

    /// Сохранение парного конфига (порт `settings_save_pair`): выравнивание,
    /// проверка неполных пар (с подтверждением), запись CSV `a;b`.
    fn save_pair_config(&mut self, force: bool) {
        let def = &MODES[self.settings_sel];
        let mut lines1: Vec<String> = self.editor_left.lines().map(|l| l.trim().to_string()).collect();
        let mut lines2: Vec<String> = self
            .editor_right
            .lines()
            .map(|l| l.trim().to_string())
            .collect();
        let max_len = lines1.len().max(lines2.len());
        while lines1.len() < max_len {
            lines1.push(String::new());
        }
        while lines2.len() < max_len {
            lines2.push(String::new());
        }
        let mismatched: Vec<usize> = lines1
            .iter()
            .zip(lines2.iter())
            .enumerate()
            .filter(|(_, (a, b))| (a.is_empty() && !b.is_empty()) || (!a.is_empty() && b.is_empty()))
            .map(|(i, _)| i + 1)
            .collect();
        if !mismatched.is_empty() && !force {
            let sample = mismatched
                .iter()
                .take(5)
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(", ");
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
                    if ui
                        .add_enabled(can_del, egui::Button::new(self.t("sch.remove")))
                        .clicked()
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
                    let labels: Vec<String> = self
                        .sched
                        .tasks
                        .iter()
                        .map(|t| {
                            let mode_label = mode_from_key(&t.mode)
                                .map(|m| self.t(mode_key(m)))
                                .unwrap_or("?");
                            let state = if t.enabled && t.remaining > 0 {
                                format!("  ({}, {})", t.time_label(), t.remaining)
                            } else {
                                format!("  ({}, —)", t.time_label())
                            };
                            format!("{mode_label}{state}")
                        })
                        .collect();
                    for (i, label) in labels.into_iter().enumerate() {
                        ui.selectable_value(&mut self.sched_sel, i, label);
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
                    )
                    .weak(),
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
            let mode = self
                .mode
                .map(mode_slug)
                .unwrap_or("scan");
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
        let cur_label = mode_from_key(&cur_mode)
            .map(|m| self.t(mode_key(m)).to_string())
            .unwrap_or_else(|| cur_mode.clone());
        let mode_items: Vec<(&'static str, String)> = MODES
            .iter()
            .map(|d| (mode_slug(d.mode), self.t(d.title_key).to_string()))
            .collect();

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
                egui::ComboBox::from_id_salt("sch_mode")
                    .selected_text(cur_label.clone())
                    .show_ui(ui, |ui| {
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
                    ))
                    .weak(),
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
                    egui::TextEdit::singleline(&mut task.output_dir)
                        .desired_width(ui.available_width() - 120.0),
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
                let name = mode_from_key(&self.sched.tasks[idx].mode)
                    .map(|m| self.t(mode_key(m)).to_string())
                    .unwrap_or_default();
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
                let notify = self
                    .sched_busy_last
                    .map(|ts| now.duration_since(ts) >= std::time::Duration::from_secs(60))
                    .unwrap_or(true);
                if notify {
                    self.sched_busy_last = Some(now);
                    let name = mode_from_key(&self.sched.tasks[i].mode)
                        .map(|m| self.t(mode_key(m)).to_string())
                        .unwrap_or_default();
                    self.push_log(&tfg("log.sch_busy", &[name]));
                }
            } else {
                self.run_task_now(i);
            }
        } else if let Some(i) = exhausted_index(&self.sched.tasks) {
            // Задание исчерпало выполнения — отключаем и сообщаем один раз.
            let name = mode_from_key(&self.sched.tasks[i].mode)
                .map(|m| self.t(mode_key(m)).to_string())
                .unwrap_or_default();
            self.sched.tasks[i].enabled = false;
            self.sched.save();
            self.push_log(&self.tf("log.sch_exhausted", &[name]));
        }

        // Пропущенные на сегодня задания (время прошло, пока приложение
        // было закрыто или занято) — сообщаем о каждом один раз.
        // Списки считаются ДО обращения к self.t: внутри цикла нельзя
        // держать заимствование `self` (см. skill: borrow-конфликт egui).
        let notified: std::collections::HashSet<u64> = self
            .sched_missed_notified
            .iter()
            .filter(|(_, d)| *d == today)
            .map(|(id, _)| *id)
            .collect();
        let missed: Vec<(u64, String)> = self
            .sched
            .tasks
            .iter()
            .filter(|t| task_missed(t, &today, cur) && !notified.contains(&t.id))
            .map(|t| (t.id, t.mode.clone()))
            .collect();
        for (id, mode) in missed {
            let name = mode_from_key(&mode)
                .map(|m| self.t(mode_key(m)).to_string())
                .unwrap_or_default();
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
        self.spawn_op(move |log, stop| {
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
                    let master_s = filemanager_core::finalize_report(PathBuf::from(&master_s))
                        .display()
                        .to_string();
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
                // Без прокрутки: описание целиком, от верхнего края.
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
            });
        });
    }
}

/// Загрузка текста конфига (utf-8 с BOM -> cp1251, как в Python).
fn load_text_file(path: impl AsRef<Path>) -> String {
    fs::read(path)
        .ok()
        .map(|b| filemanager_core::config::decode_config(&b))
        .unwrap_or_default()
}

fn save_text_file(path: impl AsRef<Path>, content: &str) -> Result<(), String> {
    let p = path.as_ref();
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(p, content.trim()).map_err(|e| e.to_string())
}

/// Разбор парного конфига в две колонки (порт `settings_load_pair`).
fn load_pair_file(path: impl AsRef<Path>) -> (String, String) {
    let bytes = match fs::read(path.as_ref()) {
        Ok(b) => b,
        Err(_) => return (String::new(), String::new()),
    };
    let text = filemanager_core::config::decode_config(&bytes);
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
                }
            }
        }
        // Проверка обновлений: один раз за запуск, если включена в настройках.
        if self.ui_config.check_updates && !self.update_done && self.update_rx.is_none() {
            self.start_update_check();
        }
        self.poll_update(ctx);
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


        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(self.t("btn.about")).clicked() {
                        self.pending = Some(Pending::About);
                    }
                    if let Some(info) = &self.update_info {
                        let label = self.tf("upd.banner", &[info.version.clone()]);
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
            ui.separator();
            ui.horizontal(|ui| {
                let tw = format!("  {}  ", self.t("tab.work"));
                let ts = format!("  {}  ", self.t("tab.settings"));
                let tp = format!("  {}  ", self.t("tab.schedule"));
                let th = format!("  {}  ", self.t("tab.help"));
                if self.running {
                    ui.label(RichText::new(tw).weak());
                    ui.label(RichText::new(ts).weak());
                    ui.label(RichText::new(tp).weak());
                    ui.label(RichText::new(th).weak());
                } else {
                    ui.selectable_value(&mut self.top_tab, TopTab::Work, tw);
                    ui.selectable_value(&mut self.top_tab, TopTab::Settings, ts);
                    ui.selectable_value(&mut self.top_tab, TopTab::Schedule, tp);
                    ui.selectable_value(&mut self.top_tab, TopTab::Help, th);
                }
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            match self.top_tab {
                TopTab::Work => self.work_tab(ui),
                TopTab::Settings => self.settings_tab(ui),
                TopTab::Schedule => self.schedule_tab(ui),
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
fn parse_args() -> (Option<PathBuf>, bool) {
    parse_args_from(std::env::args().skip(1))
}

/// Разбор аргументов (отдельно от `env::args`, чтобы покрыть тестами).
fn parse_args_from<I: Iterator<Item = String>>(args: I) -> (Option<PathBuf>, bool) {
    let mut scan = None;
    let mut minimized = false;
    let mut args = args;
    while let Some(a) = args.next() {
        match a.as_str() {
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
    (scan, minimized)
}

fn main() -> eframe::Result<()> {
    let (startup_scan, start_hidden) = parse_args();
    // Как в Python main(): относительные пути (config/, отчёты) — рядом с приложением.
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
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1050.0, 800.0])
            .with_min_inner_size([800.0, 600.0])
            .with_icon(app_icon()),
        ..Default::default()
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
            filemanager_core::set_output_format(cfg.report_format);
            apply_output_dir(&cfg.output_dir);
            let mut app = App {
                startup_scan: startup_scan.clone(),
                start_hidden,
                ..Default::default()
            };
            // Проверка: папка приложения доступна для записи (конфиги, отчёты,
            // архивы пишутся рядом с exe). Если нет — предупреждение в консоли.
            let probe = Path::new("config")
                .join("__write_probe__");
            let probe_ok = fs::write(&probe, b"ok")
                .and_then(|_| fs::remove_file(&probe))
                .is_ok();
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
        // Допустимые исключения: самоназвание языка в переключателе.
        let allowed = ["Русский"];
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


    /// Тестовый каталог данных: изолирует тесты от реальных «Документов».
    fn test_data_dir() -> std::path::PathBuf {
        static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        DIR.get_or_init(|| {
            let p = std::env::temp_dir().join(format!("fm_test_data_{}", std::process::id()));
            let _ = fs::create_dir_all(p.join("config"));
            std::env::set_var("REMONTEXPLORER_DATA_DIR", &p);
            p
        })
        .clone()
    }

    /// Тесты, меняющие рабочий каталог, выполняются строго по одному:
    /// `set_current_dir` — глобальное состояние процесса.
    static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
                )),
                ..Default::default()
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
            for tab in [TopTab::Work, TopTab::Settings, TopTab::Schedule, TopTab::Help] {
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
                        )),
                        ..Default::default()
                    };
                    let _ = ctx.run(input, |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| match tab {
                            TopTab::Work => app.work_tab(ui),
                            TopTab::Settings => app.settings_tab(ui),
                            TopTab::Schedule => app.schedule_tab(ui),
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

        let reports: Vec<_> = fs::read_dir(&out)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("BATCH_SCAN_"))
            .collect();
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
                pending: Some(pending),
                ..Default::default()
            };
            let ctx = egui::Context::default();
            for _ in 0..3 {
                let input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1200.0, 800.0),
                    )),
                    ..Default::default()
                };
                let _ = ctx.run(input, |ctx| {
                    assert!(app.pending_modal(ctx), "диалог {name} не показан");
                });
            }
            let rect = ctx
                .memory(|m| m.area_rect(egui::Id::new("fm_modal")))
                .unwrap_or_else(|| panic!("окно диалога {name} не найдено"));
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
        let work = &rest[..rest
            .find("if let Some(m) = self.mode")
            .expect("конец панели кнопок не найден")];
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
        assert!(
            !code.contains("Ivan Evseev's FileManager"),
            "осталось старое название"
        );
        // Версия убрана из правого верхнего угла (осталась в «О приложении»).
        assert!(!code.contains("app.version"), "версия снова в шапке интерфейса");
        assert!(code.contains("about.version"), "версия пропала из «О приложении»");
    }

    /// Разбор командной строки (вызов из контекстного меню Проводника).
    #[test]
    fn parses_command_line() {
        let (scan, min) = parse_args_from(
            vec!["--scan".to_string(), "\"D:\\Data\\Temp\"".to_string()].into_iter(),
        );
        assert_eq!(scan, Some(PathBuf::from("D:\\Data\\Temp")), "кавычки не сняты");
        assert!(!min);

        let (scan, min) = parse_args_from(vec!["--minimized".to_string()].into_iter());
        assert_eq!(scan, None);
        assert!(min);

        let (scan, _) = parse_args_from(
            vec!["--scan".to_string(), "D:\\Папка с пробелом".to_string()].into_iter(),
        );
        assert_eq!(scan, Some(PathBuf::from("D:\\Папка с пробелом")));

        // Без аргументов — обычный запуск.
        let (scan, min) = parse_args_from(vec!["что-то".to_string()].into_iter());
        assert_eq!(scan, None);
        assert!(!min);

        // Ключ без значения не должен ломать запуск.
        let (scan, _) = parse_args_from(vec!["--scan".to_string()].into_iter());
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
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: UiConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.theme, ThemeMode::Dark);
        assert_eq!(back.lang, Lang::En);
        assert!(!back.close_to_tray);
        assert_eq!(back.report_format, OutputFormat::Xlsx);
        assert_eq!(back.output_dir, "D:\\Reports");
        assert!(back.open_result);
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
        fs::read_dir(dir)
            .ok()?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().starts_with(prefix))
                    .unwrap_or(false)
            })
    }

    /// Каждый из семи режимов реально выполняется заданием планировщика.
    #[test]
    fn scheduler_runs_each_mode() {
        let _d = test_data_dir();
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
        )
        .unwrap();
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
        )
        .unwrap();
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
        )
        .unwrap();
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
        use chrono::Timelike;
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
            close_to_tray: true,
            ..Default::default()
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
        let close_pos = tray_src
            .find("std::process::exit(0)")
            .expect("пункт «Закрыть приложение» не завершает процесс");
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
            code.contains("filemanager_core::last_report()"),
            "результат не открывается после операции"
        );
    }
}
