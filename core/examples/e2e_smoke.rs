//! сквозной e2e-смоук всех 7 режимов FileManager (core).
//!
//! Создаёт в каталоге temp реальные файлы/папки, запускает каждую
//! функцию приложения на них и проверяет результаты (структуру,
//! CSV-отчёты с BOM/разделителем, статистику, возобновление копирования).
//!
//! Запуск: `cargo run --example e2e_smoke -p filemanager-core`
//! Выходит с кодом 1, если хоть одна проверка упала.

use std::fs;
use std::path::{Path, PathBuf};

use filemanager_core::{
    archive_folders, cleanup_empty_folders, compare_folders, copy_files_from_list,
    create_folders, delete_paths, find_duplicates_multi, scan_folder, stdout_sink,
    CompareMethod, HashAlgo, StopToken,
};

fn check(label: &str, ok: bool) -> bool {
    println!("  [{}] {label}", if ok { "PASS" } else { "FAIL" });
    ok
}

fn write(path: &Path, content: &str) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    fs::write(path, content).unwrap();
}

/// Читает CSV-отчёт: проверяет BOM, возвращает строки без BOM.
fn read_report(path: &Path) -> Vec<String> {
    let bytes = fs::read(path).unwrap();
    assert!(
        bytes.starts_with(b"\xEF\xBB\xBF"),
        "{path:?}: отсутствует UTF-8 BOM"
    );
    String::from_utf8_lossy(&bytes[3..])
        .lines()
        .map(|l| l.trim_end_matches('\r').to_string())
        .collect()
}

/// Первое вхождение ровно 6 цифр (та же rule, что в scan::extract_index_ops) —
/// чтобы проверить отчёт в «загрязнённых» окружениях (macOS temp-путь сам
/// содержит 6-значные последовательности).
fn first_6digit_run(s: &str) -> String {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i - start == 6 {
                return s[start..i].to_string();
            }
        } else {
            i += 1;
        }
    }
    String::new()
}

fn main() {
    let tmp =
        std::env::temp_dir().join(format!("fm_e2e_smoke_{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    let log = stdout_sink();
    let stop = StopToken::new();
    let cwd_backup = std::env::current_dir().unwrap();
    // Отчёты/архивы пишутся в текущий каталог — работаем в temp.
    std::env::set_current_dir(&tmp).unwrap();
    let mut all_ok = true;

    // ============================================================ 1. СОЗДАНИЕ
    println!("\n== 1. СОЗДАНИЕ (create_folders + archive_folders) ==");
    let t1 = tmp.join("Проекты/2024/Отчёт");
    let t2 = tmp.join("Проекты/2024/Документы/Счета");
    let created = create_folders(&[t1.clone(), t2.clone()], &stop, &log);
    all_ok &= check("целевые папки созданы", t1.is_dir() && t2.is_dir());
    all_ok &= check(
        "созданы все промежуточные уровни (>=4)",
        created.len() >= 4 && tmp.join("Проекты/2024").is_dir()
            && tmp.join("Проекты/2024/Документы").is_dir(),
    );
    write(&t1.join("файл1.txt"), "один");
    write(&t2.join("fact-123456.csv"), "a;b\n");
    let zip = archive_folders(&[t1.clone(), t2.clone()], true, &stop, &log);
    let zip = match zip {
        Some(z) => z,
        None => {
            all_ok &= check("архив создан", false);
            PathBuf::from("")
        }
    };
    if !zip.as_os_str().is_empty() {
        all_ok &= check("архив существует", zip.exists());
        {
            let mut ar = zip::ZipArchive::new(fs::File::open(&zip).unwrap()).unwrap();
            let names: Vec<String> = (0..ar.len())
                .map(|i| ar.by_index(i).unwrap().name().to_string())
                .collect();
            all_ok &= check(
                "в zip есть файл1.txt",
                names.iter().any(|n| n.ends_with("файл1.txt")),
            );
            all_ok &= check(
                "в zip есть fact-123456.csv",
                names.iter().any(|n| n.contains("fact-123456.csv")),
            );
        }
        all_ok &= check(
            "оригиналы удалены (delete_originals=true)",
            !t1.exists() && !t2.exists(),
        );
    }

    // ============================================================ 2. СКАНИРОВАНИЕ
    println!("\n== 2. СКАНИРОВАНИЕ (scan_folder) ==");
    let sroot = tmp.join("Data");
    fs::create_dir_all(sroot.join("123456")).unwrap();
    fs::create_dir_all(sroot.join("misc")).unwrap();
    fs::write(sroot.join("123456/photo.bin"), vec![0u8; 1_048_576]).unwrap();
    write(&sroot.join("misc/readme.txt"), "hello world");
    let scanned = scan_folder(&sroot, None, 4, &stop, &log);
    match scanned {
        Some((csv_path, stats)) => {
            all_ok &= check("просканировано 2 файла", stats.files == 2);
            let lines = read_report(&csv_path);
            all_ok &= check(
                "заголовок отчёта",
                lines.first().map(String::as_str)
                    == Some("source_root;full_path;parent_dir;name;extension;size_mb;index_ops"),
            );
            all_ok &= check("2 строки данных", lines.len() == 3);
            // ПРАВИЛО index_ops проверяем self-consistently: у каждой строки
            // index_ops обязан равняться первому 6-значному числу в ЕЁ full_path.
            // (Временный путь macOS сам содержит 6 цифр, например 840000, —
            //  поэтому сравниваем с реальным путём, а не с жёстким 123456.)
            let mut rule_ok = true;
            let mut token_row = false;
            for l in lines.iter().skip(1) {
                let cols: Vec<&str> = l.split(';').collect();
                if cols.len() != 7 {
                    rule_ok = false;
                    break;
                }
                let expected = first_6digit_run(cols[1]);
                if cols[6] != expected {
                    rule_ok = false;
                    break;
                }
                if cols[1].contains("123456") && !cols[6].is_empty() {
                    token_row = true;
                }
            }
            all_ok &= check("index_ops: правило «первое 6-значное число в full_path»", rule_ok);
            all_ok &= check("index_ops: папка 123456 даёт непустой токен", token_row);
            // Пустой токен проверяем, только если сам корневой путь 6 цифр не содержит.
            let canon = sroot.canonicalize().unwrap();
            if first_6digit_run(&canon.to_string_lossy()).is_empty() {
                let empty_ok = lines
                    .iter()
                    .skip(1)
                    .filter(|l| l.contains("/misc/"))
                    .all(|l| l.ends_with(';'));
                all_ok &= check("index_ops: пусто, когда токена в пути нет", empty_ok);
            } else {
                println!("  [SKIP] index_ops пустой случай: в корне пути уже есть 6 цифр");
            }
            all_ok &= check(
                "size_mb: 1.00 для 1 МБ файла",
                lines.iter().any(|l| l.contains(";1.00;")),
            );
        }
        None => all_ok &= check("сканирование завершилось", false),
    }

    // ============================================================ 3. СРАВНЕНИЕ
    println!("\n== 3. СРАВНЕНИЕ (compare_folders) ==");
    let a = tmp.join("Compare/A");
    let b = tmp.join("Compare/B");
    write(&a.join("same.txt"), "identical");
    write(&b.join("same.txt"), "identical");
    write(&a.join("only_a.txt"), "only in a");
    write(&a.join("changed.txt"), "version 1");
    write(&b.join("changed.txt"), "version 2, longer content");
    let ccfg = tmp.join("compare_pairs.txt");
    write(&ccfg, &format!("{};{}\n", a.display(), b.display()));
    let rep = compare_folders(
        &ccfg,
        CompareMethod::SizeDate,
        Some(&tmp.join("cmp_sizedate.csv")),
        &stop,
        &log,
    )
    .unwrap();
    all_ok &= check("отчёт сравнения создан", rep.len() == 1 && rep[0].exists());
    let lines = read_report(&rep[0]);
    all_ok &= check(
        "заголовок отчёта сравнения",
        lines.first().map(String::as_str)
            == Some("file_name;path_in_a;path_in_b;size_a_mb;size_b_mb;modified_a;modified_b;different"),
    );
    all_ok &= check("3 файла в отчёте", lines.len() == 4);
    let only_a = lines.iter().find(|l| l.starts_with("only_a.txt;")).unwrap();
    all_ok &= check("только в A: different=Yes", only_a.ends_with(";Yes"));
    all_ok &= check("только в A: path_in_b = N/A (как в Python)", only_a.split(';').nth(2) == Some("N/A"));
    let same = lines.iter().find(|l| l.starts_with("same.txt;")).unwrap();
    all_ok &= check("идентичные: different=No", same.ends_with(";No"));
    let changed = lines.iter().find(|l| l.starts_with("changed.txt;")).unwrap();
    all_ok &= check("разный размер: different=Yes", changed.ends_with(";Yes"));
    // Метод по хешу
    let rep2 = compare_folders(
        &ccfg,
        CompareMethod::Hash(HashAlgo::Sha256),
        Some(&tmp.join("cmp_hash.csv")),
        &stop,
        &log,
    )
    .unwrap();
    let lines2 = read_report(&rep2[0]);
    let changed2 = lines2.iter().find(|l| l.starts_with("changed.txt;")).unwrap();
    all_ok &= check("SHA-256: различное содержимое", changed2.ends_with(";Yes"));
    let same2 = lines2.iter().find(|l| l.starts_with("same.txt;")).unwrap();
    all_ok &= check("SHA-256: идентичное содержимое", same2.ends_with(";No"));

    // ============================================================ 4. КОПИРОВАНИЕ
    println!("\n== 4. КОПИРОВАНИЕ (copy_files_from_list) ==");
    let src = tmp.join("Copy/Src");
    write(&src.join("a.txt"), "copy me");
    write(&src.join("folder/inner.txt"), "nested");
    let dst = tmp.join("Copy/Dst");
    let kcfg = tmp.join("from_to.csv");
    write(
        &kcfg,
        &format!(
            "{};{}\n{};{}\n",
            src.join("a.txt").display(),
            dst.join("a.txt").display(),
            src.join("folder").display(),
            dst.join("folder").display()
        ),
    );
    let rlog = tmp.join("from_to.resume.json");
    // resume=true в обоих запусках: кэш возобновления пишется только в этом
    // режиме (как в Python), и повторный запуск обязан всё пропустить.
    let copied = copy_files_from_list(&kcfg, &rlog, 16, true, &stop, &log);
    all_ok &= check("a.txt на месте", dst.join("a.txt").exists());
    all_ok &= check(
        "содержимое совпадает",
        fs::read(dst.join("a.txt")).unwrap() == fs::read(src.join("a.txt")).unwrap(),
    );
    #[cfg(windows)]
    {
        // На Windows fs::copy копирует каталоги рекурсивно (улучшение к shutil.copy2)
        all_ok &= check("скопировано 2 задачи (включая папку)", copied == 2);
        all_ok &= check("вложенный inner.txt на месте", dst.join("folder/inner.txt").exists());
    }
    #[cfg(not(windows))]
    {
        // Парность с Python: shutil.copy2 НЕ копирует каталоги (ошибка в логе)
        all_ok &= check(
            "скопировано 1 задача (папка не копируется на не-Windows — парность с shutil.copy2)",
            copied == 1,
        );
    }
    let again = copy_files_from_list(&kcfg, &rlog, 16, true, &stop, &log);
    all_ok &= check("повторный запуск: 0 новых (кэш возобновления)", again == 0);

    // ============================================================ 5. ДУБЛИКАТЫ
    println!("\n== 5. ДУБЛИКАТЫ (find_duplicates_multi) ==");
    let dup = tmp.join("Dup");
    write(&dup.join("a.jpg"), "SAME-CONTENT-123");
    write(&dup.join("copy_a.jpg"), "SAME-CONTENT-123");
    write(&dup.join("scan (2).jpg"), "SAME-CONTENT-123");
    write(&dup.join("unique1.txt"), "unique-1");
    write(&dup.join("unique2.txt"), "unique-2-aa");
    let groups = find_duplicates_multi(
        std::slice::from_ref(&dup),
        HashAlgo::Sha256,
        Some(&tmp.join("dups.csv")),
        &stop,
        &log,
    )
    .unwrap();
    all_ok &= check("найдена 1 группа дубликатов", groups.len() == 1);
    all_ok &= check("в группе 3 файла", groups.first().map(|g| g.len() == 3) == Some(true));
    let dlines = read_report(&tmp.join("dups.csv"));
    all_ok &= check(
        "заголовок отчёта дубликатов",
        dlines.first().map(String::as_str) == Some("group_id;full_path;size_bytes;hash"),
    );
    all_ok &= check("отчёт: заголовок + 3 строки", dlines.len() == 4);
    all_ok &= check(
        "все строки группы с group_id=1",
        dlines[1..].iter().all(|l| l.starts_with("1;")),
    );

    // ============================================================ 6. УДАЛЕНИЕ
    println!("\n== 6. УДАЛЕНИЕ (delete_paths) ==");
    // Случай A: плоский список — после удаления всех пунктов temp опустела,
    // финальный проход удаляет её; Del НЕ удаляется (есть keep.txt).
    let del = tmp.join("Del");
    write(&del.join("temp/old_report.csv"), "report");
    fs::create_dir_all(del.join("temp/cache")).unwrap();
    write(&del.join("keep.txt"), "keep");
    let del_paths = vec![del.join("temp/old_report.csv"), del.join("temp/cache")];
    let stats = delete_paths(&del_paths, true, &stop, &log);
    all_ok &= check("удалён 1 файл", stats.files == 1);
    all_ok &= check("удалена 1 папка", stats.folders == 1);
    all_ok &= check(
        "1 пустой родитель: temp опустела после удаления пунктов списка",
        stats.parents == 1,
    );
    all_ok &= check("temp очищена финальным проходом (опустела)", !del.join("temp").exists());
    all_ok &= check("корневой Del сохранён (есть keep.txt)", del.exists());
    all_ok &= check("чужой файл сохранён", del.join("keep.txt").exists());
    // Случай B: вложенный список — промежуточная пустая папка МЕЖДУ путями
    // списка очищается, Del2 опустел после удаления X — тоже очищается.
    let x = tmp.join("Del2/X");
    fs::create_dir_all(x.join("a/b")).unwrap();
    let nested = vec![x.join("a/b"), x.join("a"), x.clone()];
    let s2 = delete_paths(&nested, true, &stop, &log);
    all_ok &= check("вложенный список: удалено 2 папки (b и X)", s2.folders == 2);
    all_ok &= check(
        "вложенный список: очищены 2 пустых родителя (a и Del2)",
        s2.parents == 2,
    );
    all_ok &= check("промежуточная папка a очищена", !x.join("a").exists());
    all_ok &= check("X удалён", !x.exists());
    all_ok &= check(
        "Del2 очищена (опустела после удаления X)",
        !tmp.join("Del2").exists(),
    );

    // ============================================================ 7. ОЧИСТКА
    println!("\n== 7. ОЧИСТКА (cleanup_empty_folders) ==");
    let cl = tmp.join("Cleanup/EmptyTree");
    fs::create_dir_all(cl.join("a/b")).unwrap();
    fs::create_dir_all(cl.join("c")).unwrap();
    write(&cl.join("d.txt"), "file");
    let n = cleanup_empty_folders(&cl, &stop, &log);
    all_ok &= check("удалено 2 пустые папки", n == 2);
    all_ok &= check("a/b удалена", !cl.join("a/b").exists());
    all_ok &= check("c удалена", !cl.join("c").exists());
    all_ok &= check("a сохранена (родитель не чистится в том же проходе)", cl.join("a").exists());
    all_ok &= check("файл d.txt сохранён", cl.join("d.txt").exists());

    // ------------------------------------------------------- ИТОГ
    std::env::set_current_dir(&cwd_backup).unwrap();
    let _ = fs::remove_dir_all(&tmp);
    if all_ok {
        println!("\n=== E2E SMOKE: ВСЕ ПРОВЕРКИ ПРОЙДЕНЫ (7 режимов) ===");
    } else {
        println!("\n=== E2E SMOKE: ЕСТЬ ПРОВАЛЫ — СМОТРИТЕ [FAIL] ВЫШЕ ===");
        std::process::exit(1);
    }
}
