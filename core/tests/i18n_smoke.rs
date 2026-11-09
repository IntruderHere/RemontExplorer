use remont_core::i18n::{en, fill, tr, Lang};

#[test]
fn runtime_fmt_str() {
    let t = tr(Lang::Ru, "cleanup_none_found");
    let s = fill(t, &["/tmp/x".to_string()]);
    assert_eq!(s, "Пустых папок в /tmp/x не найдено.");
}

/// Ключ из строки кода: первый строковый литерал после первого аргумента
/// вызова `em!(<сток>, "key", …)`. Возвращает `None`, если в строке
/// нет вызова `em!` или ключ не строковый литерал.
fn em_key_in_line(line: &str) -> Option<String> {
    let rel = line.find("em!")?;
    let rest = &line[rel..];
    let mut chars = rest.char_indices().peekable();
    // "em!("
    for _ in 0..4 {
        chars.next()?;
    }
    // Пропустить пробельные символы.
    while let Some((_, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else {
            break;
        }
    }
    // Первый аргумент (выражение-сток) — до запятой.
    for (_, c) in chars.by_ref() {
        if c == ',' {
            break;
        }
    }
    // Пропустить пробельные символы.
    while let Some((_, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else {
            break;
        }
    }
    let (_, c) = chars.next()?;
    if c != '"' {
        return None;
    }
    let mut key = String::new();
    for (_, c) in chars {
        if c == '"' {
            return Some(key);
        }
        key.push(c);
    }
    None
}

/// Покрытие i18n: каждый ключ, реально используемый в логах core
/// (через `em!`), должен иметь английский перевод — иначе английский
/// интерфейс молча покажет русскую строку.
#[test]
fn every_emitted_key_has_english() {
    let mut checked = 0usize;
    let mut missing = Vec::new();
    for entry in std::fs::read_dir("src").expect("core/src") {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        for (n, line) in src.lines().enumerate() {
            let t = line.trim_start();
            if t.starts_with("//") {
                continue;
            }
            if let Some(key) = em_key_in_line(line) {
                if key.is_empty() {
                    continue;
                }
                checked += 1;
                if en(&key).is_none() {
                    missing.push(format!("{}:{}: {key}", path.display(), n + 1));
                }
            }
        }
    }
    // Сканер обязан найти вызовы em! — если мало, сломался сам парсер.
    assert!(checked > 50, "сканер em! нашёл мало вызовов: {checked}");
    assert!(
        missing.is_empty(),
        "ключи без английского перевода: {:#?}",
        missing
    );
}
