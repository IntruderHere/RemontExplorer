//! Сравнение двух отчётов сканирования: что изменилось между прогонами.
//!
//! Отчёт сканирования — таблица с колонками
//! `source_root; full_path; parent_dir; name; extension; size_mb; index_ops`.
//! Сравнение отвечает на вопрос «что появилось, что исчезло, что изменилось
//! по размеру» — например, после передачи папки подрядчику.
//!
//! Отчёты читаются и в CSV, и в XLSX: пользователь может сохранять оба
//! формата, а сравнивать нужно любые два файла.
//!
//! Ключ сопоставления — путь. Если у сравниваемых отчётов разные корни
//! (`source_root`), сравниваются относительные пути: так можно сопоставить
//! «D:\Ремонт\ОПС 1» и «E:\Копия\ОПС 1» после переноса.

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Строка отчёта сканирования.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportRow {
    pub source_root: String,
    pub full_path: String,
    pub parent_dir: String,
    pub name: String,
    pub extension: String,
    pub size_mb: f64,
}

impl ReportRow {
    /// Относительный путь внутри корня (для сопоставления при разных корнях).
    pub fn relative(&self) -> String {
        let root = self.source_root.trim_end_matches(['\\', '/']);
        let full = &self.full_path;
        let rel = full
            .strip_prefix(root)
            .unwrap_or(full)
            .trim_start_matches(['\\', '/']);
        normalize_key(rel)
    }

    fn key(&self) -> String {
        normalize_key(&self.full_path)
    }
}

fn normalize_key(s: &str) -> String {
    s.replace('\\', "/").to_lowercase()
}

/// Тип изменения.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    /// Файл появился в новом отчёте.
    Added,
    /// Файл исчез.
    Removed,
    /// Размер изменился.
    Changed,
    /// Без изменений (в отчёт не попадает, используется в статистике).
    Same,
}

impl DiffKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DiffKind::Added => "added",
            DiffKind::Removed => "removed",
            DiffKind::Changed => "changed",
            DiffKind::Same => "same",
        }
    }

    /// Заголовок на русском (для отчёта).
    pub fn title_ru(self) -> &'static str {
        match self {
            DiffKind::Added => "Добавлен",
            DiffKind::Removed => "Удалён",
            DiffKind::Changed => "Изменён",
            DiffKind::Same => "Без изменений",
        }
    }
}

/// Одно различие.
#[derive(Debug, Clone)]
pub struct DiffRow {
    pub kind: DiffKind,
    pub name: String,
    pub path: String,
    pub extension: String,
    pub old_size_mb: f64,
    pub new_size_mb: f64,
}

impl DiffRow {
    /// Изменение размера в МБ (для изменённых).
    pub fn delta_mb(&self) -> f64 {
        self.new_size_mb - self.old_size_mb
    }
}

/// Сводка сравнения.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DiffStats {
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
    pub same: usize,
    pub old_files: usize,
    pub new_files: usize,
    pub bytes_added: u64,
    pub bytes_removed: u64,
    /// Изменился ли корень (сравнение шло по относительным путям).
    pub normalized_by_root: bool,
}

/// Ошибка сравнения — с текстом, который можно показать пользователю.
#[derive(Debug, Clone)]
pub struct DiffError(pub String);

impl std::fmt::Display for DiffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for DiffError {}

/// Прочитать отчёт (CSV или XLSX) в строки.
pub fn read_report(path: &Path) -> Result<Vec<ReportRow>, DiffError> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let table = match ext.as_str() {
        "xlsx" => read_xlsx(path)?,
        _ => read_csv(path)?,
    };
    rows_from_table(&table, path)
}

/// Разобрать таблицу (первая строка — заголовок) в строки отчёта.
fn rows_from_table(table: &[Vec<String>], path: &Path) -> Result<Vec<ReportRow>, DiffError> {
    let Some(header) = table.first() else {
        return Err(DiffError(format!("файл пуст: {}", path.display())));
    };
    let col = |name: &str| {
        header
            .iter()
            .position(|h| h.trim().trim_start_matches('\u{feff}').eq_ignore_ascii_case(name))
    };
    let (Some(i_root), Some(i_full)) = (col("source_root"), col("full_path")) else {
        return Err(DiffError(format!(
            "это не отчёт сканирования (нет колонок source_root/full_path): {}",
            path.display()
        )));
    };
    let i_parent = col("parent_dir");
    let i_name = col("name");
    let i_ext = col("extension");
    let i_size = col("size_mb");

    let mut rows = Vec::with_capacity(table.len().saturating_sub(1));
    for r in &table[1..] {
        let get = |i: Option<usize>| i.and_then(|i| r.get(i)).cloned().unwrap_or_default();
        let full = r.get(i_full).cloned().unwrap_or_default();
        if full.trim().is_empty() {
            continue;
        }
        let name = {
            let n = get(i_name);
            if n.is_empty() {
                Path::new(&full)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            } else {
                n
            }
        };
        rows.push(ReportRow {
            source_root: r.get(i_root).cloned().unwrap_or_default(),
            full_path: full,
            parent_dir: get(i_parent),
            name,
            extension: get(i_ext),
            size_mb: get(i_size).replace(',', ".").parse().unwrap_or(0.0),
        });
    }
    Ok(rows)
}

fn read_csv(path: &Path) -> Result<Vec<Vec<String>>, DiffError> {
    let bytes = std::fs::read(path).map_err(|e| DiffError(format!("не читается {}: {e}", path.display())))?;
    let text = decode_text(&bytes);
    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(b';')
        .has_headers(false)
        .flexible(true)
        .from_reader(text.as_bytes());
    let mut out = Vec::new();
    for rec in rdr.records().flatten() {
        out.push(rec.iter().map(|s| s.to_string()).collect());
    }
    Ok(out)
}

/// UTF-8 (с BOM или без), иначе cp1251 — как в остальных конфигах.
fn decode_text(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => {
            let (cow, _, _) = encoding_rs::WINDOWS_1251.decode(bytes);
            cow.into_owned()
        }
    }
}

/// Прочитать первый лист XLSX.
///
/// Свой минимальный разбор: нужны только значения ячеек, а полноценная
/// библиотека электронных таблиц ради этого — лишние мегабайты в сборке.
fn read_xlsx(path: &Path) -> Result<Vec<Vec<String>>, DiffError> {
    let file = std::fs::File::open(path).map_err(|e| DiffError(format!("не читается {}: {e}", path.display())))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| DiffError(format!("не XLSX {}: {e}", path.display())))?;

    let mut shared: Vec<String> = Vec::new();
    if let Ok(mut f) = zip.by_name("xl/sharedStrings.xml") {
        let mut xml = String::new();
        f.read_to_string(&mut xml).map_err(|e| DiffError(e.to_string()))?;
        shared = parse_shared_strings(&xml);
    }

    // Первый лист по порядку в книге; обычно sheet1.xml.
    let mut sheet_name = None;
    for name in ["xl/worksheets/sheet1.xml", "xl/worksheets/sheet.xml"] {
        if zip.by_name(name).is_ok() {
            sheet_name = Some(name.to_string());
            break;
        }
    }
    let sheet_name = sheet_name.ok_or_else(|| DiffError("в книге нет листа".into()))?;
    let mut f = zip.by_name(&sheet_name).map_err(|e| DiffError(e.to_string()))?;
    let mut xml = String::new();
    f.read_to_string(&mut xml).map_err(|e| DiffError(e.to_string()))?;
    Ok(parse_sheet(&xml, &shared))
}

/// Раскрыть базовые XML-сущности в тексте ячейки.
fn unescape_xml(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// `xl/sharedStrings.xml` → список строк (каждая `<si>` может состоять из
/// нескольких `<t>` — склеиваем).
fn parse_shared_strings(xml: &str) -> Vec<String> {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut out = Vec::new();
    let mut cur: Option<String> = None;
    loop {
        match reader.read_event() {
            Ok(quick_xml::events::Event::Start(e)) if e.name().as_ref() == b"si" => {
                cur = Some(String::new());
            }
            Ok(quick_xml::events::Event::End(e)) if e.name().as_ref() == b"si" => {
                out.push(cur.take().unwrap_or_default());
            }
            Ok(quick_xml::events::Event::Text(t)) if cur.is_some() => {
                if let Ok(s) = t.decode() {
                    if let Some(c) = cur.as_mut() {
                        c.push_str(&unescape_xml(&s));
                    }
                }
            }
            Ok(quick_xml::events::Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

/// Разбор листа: `<row>` → ячейки, `t="s"` — ссылка на sharedStrings.
///
/// Позиция ячейки берётся из атрибута `r` (`C12` → колонка 2): пустые ячейки
/// в XLSX не пишутся вовсе, и без этого колонки «съезжают» влево.
fn parse_sheet(xml: &str, shared: &[String]) -> Vec<Vec<String>> {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cell_type = String::new();
    let mut cur_col = 0usize;
    let mut in_cell = false;
    let mut value = String::new();

    loop {
        match reader.read_event() {
            Ok(quick_xml::events::Event::Start(e)) => match e.name().as_ref() {
                b"row" => row.clear(),
                b"c" => {
                    in_cell = true;
                    value.clear();
                    cell_type.clear();
                    cur_col = row.len();
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"t" => cell_type = String::from_utf8_lossy(&attr.value).into_owned(),
                            b"r" => {
                                if let Some(i) = col_index(&String::from_utf8_lossy(&attr.value)) {
                                    cur_col = i;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            },
            Ok(quick_xml::events::Event::Text(t)) if in_cell => {
                if let Ok(s) = t.decode() {
                    value.push_str(&unescape_xml(&s));
                }
            }
            Ok(quick_xml::events::Event::End(e)) => match e.name().as_ref() {
                b"c" => {
                    in_cell = false;
                    let v = if cell_type == "s" {
                        value
                            .trim()
                            .parse::<usize>()
                            .ok()
                            .and_then(|i| shared.get(i))
                            .cloned()
                            .unwrap_or_default()
                    } else {
                        value.clone()
                    };
                    if row.len() <= cur_col {
                        row.resize(cur_col + 1, String::new());
                    }
                    row[cur_col] = v;
                }
                b"row" => rows.push(std::mem::take(&mut row)),
                _ => {}
            },
            Ok(quick_xml::events::Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    rows
}

/// `"C12"` → 2 (A = 0). `None`, если ссылка не по шаблону.
fn col_index(cell_ref: &str) -> Option<usize> {
    let mut n = 0usize;
    let mut any = false;
    for ch in cell_ref.chars() {
        if ch.is_ascii_alphabetic() {
            n = n * 26 + (ch.to_ascii_uppercase() as usize - 'A' as usize + 1);
            any = true;
        } else {
            break;
        }
    }
    any.then(|| n - 1)
}

/// Сравнить два отчёта: `old` — предыдущий, `new` — текущий.
pub fn diff_reports(old: &Path, new: &Path) -> Result<(Vec<DiffRow>, DiffStats), DiffError> {
    let old_rows = read_report(old)?;
    let new_rows = read_report(new)?;
    Ok(diff_rows(&old_rows, &new_rows))
}

/// Сравнить два набора строк отчёта.
pub fn diff_rows(old_rows: &[ReportRow], new_rows: &[ReportRow]) -> (Vec<DiffRow>, DiffStats) {
    let old_roots: HashSet<&str> = old_rows.iter().map(|r| r.source_root.as_str()).collect();
    let new_roots: HashSet<&str> = new_rows.iter().map(|r| r.source_root.as_str()).collect();
    // Разные корни — сравниваем по относительным путям, иначе по полным.
    let by_relative = old_roots != new_roots;

    let key_of = |r: &ReportRow| if by_relative { r.relative() } else { r.key() };

    let old_map: BTreeMap<String, &ReportRow> =
        old_rows.iter().map(|r| (key_of(r), r)).collect();
    let new_map: BTreeMap<String, &ReportRow> =
        new_rows.iter().map(|r| (key_of(r), r)).collect();

    let mut out = Vec::new();
    let mut stats = DiffStats {
        old_files: old_rows.len(),
        new_files: new_rows.len(),
        normalized_by_root: by_relative,
        ..Default::default()
    };

    for (key, nr) in &new_map {
        match old_map.get(key) {
            None => {
                stats.added += 1;
                stats.bytes_added += (nr.size_mb * 1024.0 * 1024.0) as u64;
                out.push(DiffRow {
                    kind: DiffKind::Added,
                    name: nr.name.clone(),
                    path: nr.full_path.clone(),
                    extension: nr.extension.clone(),
                    old_size_mb: 0.0,
                    new_size_mb: nr.size_mb,
                });
            }
            Some(or) => {
                if (or.size_mb - nr.size_mb).abs() > 0.005 {
                    stats.changed += 1;
                    if nr.size_mb > or.size_mb {
                        stats.bytes_added +=
                            ((nr.size_mb - or.size_mb) * 1024.0 * 1024.0) as u64;
                    } else {
                        stats.bytes_removed +=
                            ((or.size_mb - nr.size_mb) * 1024.0 * 1024.0) as u64;
                    }
                    out.push(DiffRow {
                        kind: DiffKind::Changed,
                        name: nr.name.clone(),
                        path: nr.full_path.clone(),
                        extension: nr.extension.clone(),
                        old_size_mb: or.size_mb,
                        new_size_mb: nr.size_mb,
                    });
                } else {
                    stats.same += 1;
                }
            }
        }
    }
    for (key, or) in &old_map {
        if !new_map.contains_key(key) {
            stats.removed += 1;
            stats.bytes_removed += (or.size_mb * 1024.0 * 1024.0) as u64;
            out.push(DiffRow {
                kind: DiffKind::Removed,
                name: or.name.clone(),
                path: or.full_path.clone(),
                extension: or.extension.clone(),
                old_size_mb: or.size_mb,
                new_size_mb: 0.0,
            });
        }
    }

    // Сначала добавленные, затем изменённые, затем удалённые — так отчёт
    // читается сверху вниз без перескакивания.
    out.sort_by_key(|r| match r.kind {
        DiffKind::Added => 0,
        DiffKind::Changed => 1,
        DiffKind::Removed => 2,
        DiffKind::Same => 3,
    });
    (out, stats)
}

/// Записать отчёт сравнения в CSV (UTF-8 с BOM, `;`, CRLF — Excel).
pub fn write_diff_csv(rows: &[DiffRow], stats: &DiffStats, path: &Path) -> Result<(), DiffError> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut wtr = csv::WriterBuilder::new()
        .delimiter(b';')
        .terminator(csv::Terminator::CRLF)
        .from_path(path)
        .map_err(|e| DiffError(e.to_string()))?;

    wtr.write_record([
        "change",
        "name",
        "full_path",
        "extension",
        "old_size_mb",
        "new_size_mb",
        "delta_mb",
    ])
    .map_err(|e| DiffError(e.to_string()))?;

    for r in rows {
        wtr.write_record([
            r.kind.title_ru().to_string(),
            r.name.clone(),
            r.path.clone(),
            r.extension.clone(),
            format!("{:.2}", r.old_size_mb),
            format!("{:.2}", r.new_size_mb),
            format!("{:+.2}", r.delta_mb()),
        ])
        .map_err(|e| DiffError(e.to_string()))?;
    }
    // Итоговая строка — чтобы отчёт можно было читать без пересчёта.
    wtr.write_record([
        format!(
            "ИТОГО: +{} / -{} / ~{} (без изменений {})",
            stats.added, stats.removed, stats.changed, stats.same
        ),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
    ])
    .map_err(|e| DiffError(e.to_string()))?;
    wtr.flush().map_err(|e| DiffError(e.to_string()))?;

    // BOM — иначе Excel не увидит UTF-8.
    let body = std::fs::read(path).map_err(|e| DiffError(e.to_string()))?;
    let mut with_bom = Vec::with_capacity(body.len() + 3);
    with_bom.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    with_bom.extend_from_slice(&body);
    std::fs::write(path, with_bom).map_err(|e| DiffError(e.to_string()))?;
    Ok(())
}

/// Сформировать отчёт сравнения: CSV, при включённом формате — XLSX.
/// Возвращает путь к созданному файлу.
pub fn save_diff_report(
    rows: &[DiffRow],
    stats: &DiffStats,
    csv_path: PathBuf,
) -> Result<PathBuf, DiffError> {
    write_diff_csv(rows, stats, &csv_path)?;
    Ok(crate::finalize_report(csv_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(root: &str, full: &str, name: &str, mb: f64) -> ReportRow {
        ReportRow {
            source_root: root.into(),
            full_path: full.into(),
            parent_dir: Path::new(full)
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            name: name.into(),
            extension: Path::new(name)
                .extension()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_default(),
            size_mb: mb,
        }
    }

    fn write_csv(path: &Path, rows: &[ReportRow]) {
        let mut w = csv::WriterBuilder::new().delimiter(b';').from_path(path).unwrap();
        w.write_record([
            "source_root",
            "full_path",
            "parent_dir",
            "name",
            "extension",
            "size_mb",
            "index_ops",
        ])
        .unwrap();
        for r in rows {
            w.write_record([
                r.source_root.clone(),
                r.full_path.clone(),
                r.parent_dir.clone(),
                r.name.clone(),
                r.extension.clone(),
                format!("{:.2}", r.size_mb),
                String::new(),
            ])
            .unwrap();
        }
        w.flush().unwrap();
    }

    #[test]
    fn detects_added_removed_and_changed() {
        let old = vec![
            row(r"D:\ОПС", r"D:\ОПС\a.txt", "a.txt", 1.0),
            row(r"D:\ОПС", r"D:\ОПС\b.txt", "b.txt", 2.0),
            row(r"D:\ОПС", r"D:\ОПС\c.txt", "c.txt", 3.0),
        ];
        let new = vec![
            row(r"D:\ОПС", r"D:\ОПС\a.txt", "a.txt", 1.0),
            row(r"D:\ОПС", r"D:\ОПС\b.txt", "b.txt", 5.5),
            row(r"D:\ОПС", r"D:\ОПС\d.txt", "d.txt", 4.0),
        ];
        let (diff, stats) = diff_rows(&old, &new);
        assert_eq!(stats.added, 1);
        assert_eq!(stats.removed, 1);
        assert_eq!(stats.changed, 1);
        assert_eq!(stats.same, 1);
        assert_eq!(stats.old_files, 3);
        assert_eq!(stats.new_files, 3);

        let d = diff.iter().find(|r| r.name == "b.txt").unwrap();
        assert_eq!(d.kind, DiffKind::Changed);
        assert!((d.delta_mb() - 3.5).abs() < 1e-9);

        // Порядок: добавленные, изменённые, удалённые.
        assert_eq!(diff[0].kind, DiffKind::Added);
        assert_eq!(diff[1].kind, DiffKind::Changed);
        assert_eq!(diff[2].kind, DiffKind::Removed);
    }

    #[test]
    fn compares_by_relative_path_when_roots_differ() {
        let old = vec![row(r"D:\Ремонт", r"D:\Ремонт\ОПС 1\a.txt", "a.txt", 1.0)];
        let new = vec![row(r"E:\Копия", r"E:\Копия\ОПС 1\a.txt", "a.txt", 1.0)];
        let (diff, stats) = diff_rows(&old, &new);
        assert!(stats.normalized_by_root);
        assert!(diff.is_empty(), "одинаковые файлы не должны различаться");
        assert_eq!(stats.same, 1);
    }

    #[test]
    fn size_rounding_does_not_create_noise() {
        let old = vec![row(r"D:\X", r"D:\X\a.bin", "a.bin", 1.00)];
        let new = vec![row(r"D:\X", r"D:\X\a.bin", "a.bin", 1.001)];
        let (diff, stats) = diff_rows(&old, &new);
        assert!(diff.is_empty());
        assert_eq!(stats.changed, 0);
    }

    #[test]
    fn reads_csv_report_with_bom() {
        let dir = std::env::temp_dir().join(format!("remont_diff_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("r.csv");
        write_csv(&p, &[row(r"D:\ОПС", r"D:\ОПС\файл.txt", "файл.txt", 1.5)]);
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend(std::fs::read(&p).unwrap());
        std::fs::write(&p, bytes).unwrap();

        let rows = read_report(&p).expect("чтение CSV");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "файл.txt");
        assert!((rows[0].size_mb - 1.5).abs() < 1e-9);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn xlsx_roundtrip_is_readable() {
        let dir = std::env::temp_dir().join(format!("remont_diff_x_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let csv = dir.join("r.csv");
        write_csv(
            &csv,
            &[
                row(r"D:\ОПС", r"D:\ОПС\a.txt", "a.txt", 1.0),
                row(r"D:\ОПС", r"D:\ОПС\б.bin", "б.bin", 2.25),
            ],
        );
        let xlsx = dir.join("r.xlsx");
        crate::xlsx::csv_to_xlsx(&csv, &xlsx).expect("конвертация");

        let rows = read_report(&xlsx).expect("чтение XLSX");
        assert_eq!(rows.len(), 2, "строки: {rows:#?}");
        assert_eq!(rows[0].name, "a.txt");
        assert_eq!(rows[1].name, "б.bin");
        assert!((rows[1].size_mb - 2.25).abs() < 1e-9);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn diff_report_is_excel_friendly() {
        let dir = std::env::temp_dir().join(format!("remont_diff_out_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("old.csv");
        let new = dir.join("new.csv");
        write_csv(&old, &[row(r"D:\X", r"D:\X\a.txt", "a.txt", 1.0)]);
        write_csv(&new, &[row(r"D:\X", r"D:\X\b.txt", "b.txt", 2.0)]);
        let (diff, stats) = diff_reports(&old, &new).expect("сравнение");

        let out = dir.join("diff.csv");
        write_diff_csv(&diff, &stats, &out).unwrap();
        let bytes = std::fs::read(&out).unwrap();
        assert_eq!(&bytes[..3], &[0xEF, 0xBB, 0xBF]);
        let text = String::from_utf8(bytes[3..].to_vec()).unwrap();
        assert!(text.contains("Добавлен"), "{text}");
        assert!(text.contains("Удалён"), "{text}");
        assert!(text.contains("ИТОГО: +1 / -1 / ~0"), "{text}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parses_column_refs() {
        assert_eq!(col_index("A1"), Some(0));
        assert_eq!(col_index("C12"), Some(2));
        assert_eq!(col_index("AA2"), Some(26));
        assert_eq!(col_index(""), None);
    }

    #[test]
    fn rejects_foreign_csv() {
        let dir = std::env::temp_dir().join(format!("remont_diff_bad_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bad.csv");
        std::fs::write(&p, "a;b;c\n1;2;3\n").unwrap();
        assert!(read_report(&p).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
