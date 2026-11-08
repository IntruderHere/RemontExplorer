//! Разметка строк журнала: цвет и признак ошибки.
//!
//! Вынесено отдельно от интерфейса, потому что здесь сравниваются служебные
//! пометки ядра («Ошибка …», «Готово»), а не тексты для пользователя: они
//! одинаковы для обоих языков и не должны переводиться.

/// Строка сообщает об ошибке.
pub fn is_error(line: &str) -> bool {
    let l = line.to_lowercase();
    l.contains("ошибка ")        // «Ошибка удаления …»
        || l.contains("ошибки")
        || l.contains("не удалось")
        || l.contains("error:")
        || l.contains("failed")
        || l.contains("could not")
        || l.contains("недостаточно места")
}

/// Строка сообщает об успешном завершении шага.
pub fn is_success(line: &str) -> bool {
    let l = line.to_lowercase();
    l.contains("готово") || l.contains("done") || l.contains("[done]")
}

/// Строка про архив.
pub fn is_archive(line: &str) -> bool {
    let l = line.to_lowercase();
    l.contains("архив") || l.contains("archive") || l.contains("zip")
}

/// Цвет строки журнала в консоли.
pub fn line_color(line: &str, default: egui::Color32) -> egui::Color32 {
    if is_error(line) {
        egui::Color32::from_rgb(220, 53, 69)
    } else if is_success(line) {
        egui::Color32::from_rgb(40, 167, 69)
    } else if is_archive(line) {
        egui::Color32::from_rgb(255, 159, 10)
    } else if line.contains("[START]") {
        egui::Color32::from_rgb(33, 100, 220)
    } else if line.contains("[DONE]") {
        egui::Color32::from_rgb(40, 167, 69)
    } else {
        default
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_errors_in_both_languages() {
        assert!(is_error("Ошибка удаления D:\\a: доступ запрещён"));
        assert!(is_error("Не удалось создать архив: нет места"));
        assert!(is_error("error: permission denied"));
        assert!(is_error("Copy failed for D:\\a"));
        assert!(is_error("Недостаточно места на C:"));
    }

    #[test]
    fn zero_error_counters_are_not_errors() {
        // «Ошибок: 0» — это итоговая строка, а не ошибка.
        assert!(!is_error("Ошибок: 0"));
        assert!(!is_error("Готово: 10 из 10"));
        assert!(!is_error("[DONE] Сканирование"));
    }

    #[test]
    fn success_and_archive_markers() {
        assert!(is_success("Готово: 5 файлов"));
        assert!(is_success("done"));
        assert!(is_archive("Архив создан: out.zip"));
        assert!(is_archive("zip_root: D:\\"));
    }

    #[test]
    fn colors_follow_markers() {
        let def = egui::Color32::WHITE;
        assert_eq!(line_color("Ошибка удаления X", def), egui::Color32::from_rgb(220, 53, 69));
        assert_eq!(line_color("Готово: 3", def), egui::Color32::from_rgb(40, 167, 69));
        assert_eq!(line_color("Архив создан: a.zip", def), egui::Color32::from_rgb(255, 159, 10));
        assert_eq!(line_color("обычная строка", def), def);
    }
}
