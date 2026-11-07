//! Единая папка результатов: отчёты всех операций пишутся туда,
//! а путь последнего отчёта доступен GUI (для настройки «открыть файл»).

use remont_core::{
    clear_last_report, last_report, resolve_report_path, scan_folder, set_output_dir, Sink,
    StopToken,
};
use std::fs;
use std::sync::{Arc, Mutex};

fn quiet() -> remont_core::LogSink {
    Arc::new(Mutex::new(Sink::text_only(|_| {})))
}

#[test]
fn reports_go_to_configured_folder() {
    let tmp = std::env::temp_dir().join(format!("fm_outdir_{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    let src = tmp.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("a.txt"), b"1").unwrap();
    let reports = tmp.join("Reports");

    set_output_dir(Some(reports.clone()));
    clear_last_report();
    let (path, stats) = scan_folder(&src, None, 1, &StopToken::new(), &quiet()).expect("скан");
    assert_eq!(stats.files, 1);

    assert!(
        path.starts_with(&reports),
        "отчёт не в выбранной папке: {}",
        path.display()
    );
    assert!(path.exists(), "файл отчёта не создан");
    assert!(reports.is_dir(), "папка результатов не создана автоматически");

    // GUI открывает результат по этому пути.
    assert_eq!(
        last_report().as_deref(),
        Some(path.as_path()),
        "последний отчёт не запомнен"
    );

    // Сброс настройки возвращает файлы рядом с приложением.
    set_output_dir(None);
    clear_last_report();
    assert!(last_report().is_none(), "последний отчёт не сбрасывается");

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn default_report_path_is_absolute() {
    set_output_dir(None);
    let p = resolve_report_path("files_info_260885_Новые_материалы_20261106_164503.xlsx");
    assert!(
        p.is_absolute(),
        "путь отчёта должен быть абсолютным: {}",
        p.display()
    );
    assert!(
        p.to_string_lossy().contains("Новые_материалы"),
        "кириллица в имени искажена: {}",
        p.display()
    );
}
