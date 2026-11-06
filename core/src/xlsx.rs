//! Конвертация CSV-отчётов в XLSX.
//!
//! Формат отчётов выбирается пользователем в настройках (CSV или XLSX).
//! При выборе XLSX приложение переводит готовый CSV в книгу Excel и удаляет
//! промежуточный CSV — для пользователя это происходит незаметно.

use std::path::Path;

/// Максимальная ширина колонки при автоподборе (в символах Excel).
const MAX_COL_WIDTH: f64 = 60.0;

/// Перевести CSV-отчёт (разделитель `;`, UTF-8 с BOM) в XLSX.
///
/// Числовые значения записываются числами (их можно суммировать в Excel),
/// остальные — текстом. Заголовок выделяется жирным, первая строка
/// закрепляется, ширина колонок подбирается по содержимому.
pub fn csv_to_xlsx(csv_path: &Path, xlsx_path: &Path) -> Result<(), String> {
    use rust_xlsxwriter::{Format, Workbook};

    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b';')
        // Заголовок — такая же строка данных: переносим его в книгу как есть.
        .has_headers(false)
        .flexible(true)
        .from_path(csv_path)
        .map_err(|e| format!("не удалось открыть {}: {e}", csv_path.display()))?;

    let mut workbook = Workbook::new();
    let header_fmt = Format::new().set_bold();
    let sheet = workbook.add_worksheet();
    sheet
        .set_freeze_panes(1, 0)
        .map_err(|e| format!("закрепление строки заголовка: {e}"))?;

    let mut widths: Vec<f64> = Vec::new();

    for (row, record) in reader.records().enumerate() {
        let record = record.map_err(|e| format!("ошибка чтения {}: {e}", csv_path.display()))?;
        for (col, raw) in record.iter().enumerate() {
            // BOM в первом поле первой строки убираем — иначе заголовок
            // выглядит как "\u{feff}source_root".
            let field = if row == 0 && col == 0 {
                raw.trim_start_matches('\u{feff}')
            } else {
                raw
            };
            let r = row as u32;
            let c = col as u16;
            let res = if row == 0 {
                sheet.write_string_with_format(r, c, field, &header_fmt)
            } else if let Ok(n) = field.parse::<f64>() {
                sheet.write_number(r, c, n)
            } else {
                sheet.write_string(r, c, field)
            };
            res.map_err(|e| format!("запись ячейки ({r},{c}): {e}"))?;

            if widths.len() <= col {
                widths.push(0.0);
            }
            let len = field.chars().count() as f64 + 2.0;
            if len > widths[col] {
                widths[col] = len.min(MAX_COL_WIDTH);
            }
        }
    }

    for (col, w) in widths.iter().enumerate() {
        sheet
            .set_column_width(col as u16, *w)
            .map_err(|e| format!("ширина колонки {col}: {e}"))?;
    }

    workbook
        .save(xlsx_path)
        .map_err(|e| format!("не удалось сохранить {}: {e}", xlsx_path.display()))?;
    Ok(())
}
