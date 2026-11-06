//! Чтение конфигурационных файлов: списки путей и пары «откуда;куда».
//!
//! Порт `read_paths_from_file` и CSV-чтека из Python-версии.
//! ВАЖНО: utf-8-sig фактически пробует ПЕРЕ простым utf-8 (BOM срезается) —
//! иначе BOM уезжает в первое поле как `\u{feff}`.

use std::fs;
use std::path::{Path, PathBuf};

use encoding_rs::WINDOWS_1251;

/// Декодирование с fallback: валидный UTF-8 (с отсечением BOM),
/// иначе cp1251 (односимвольная — всегда декодируется).
/// Повторяет поведение Python: первая декодируемая кодировка побеждает;
/// cp1251 никогда не падает, поэтому latin-1 фактически не достигается.
pub fn decode_config(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.strip_prefix('\u{feff}').unwrap_or(s).to_string(),
        Err(_) => WINDOWS_1251.decode(bytes).0.into_owned(),
    }
}

/// Читает список путей по одному в строке (`read_paths_from_file`).
pub fn read_paths_file(path: &Path) -> Vec<PathBuf> {
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    let text = decode_config(&bytes);
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Строка «откуда;куда».
#[derive(Debug, Clone)]
pub struct Pair {
    pub from: PathBuf,
    pub to: PathBuf,
}

/// Читает CSV с парами (разделитель `;`).
/// Пропускает шапку, если в первой ячейке первой строки нет символов пути
/// (порт эвристики Python: нет ни `:`, ни `\`, ни `/`).
pub fn read_pairs_csv(path: &Path) -> Vec<Pair> {
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    let text = decode_config(&bytes);
    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(b';')
        .flexible(true)
        .has_headers(false)
        .from_reader(text.as_bytes());

    let mut pairs = Vec::new();
    for (i, result) in rdr.records().enumerate() {
        let Ok(rec) = result else { continue };
        if rec.len() < 2 {
            continue;
        }
        if i == 0 {
            let first = rec.get(0).unwrap_or("");
            if !first.contains([':', '\\', '/']) {
                continue; // шапка
            }
        }
        let from = rec.get(0).unwrap_or("").trim();
        let to = rec.get(1).unwrap_or("").trim();
        if from.is_empty() && to.is_empty() {
            continue;
        }
        pairs.push(Pair {
            from: from.into(),
            to: to.into(),
        });
    }
    pairs
}
