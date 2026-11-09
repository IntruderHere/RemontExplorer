//! Диагностика: почему модальное окно растягивается по вертикали.
//! egui можно прогонять без окна — измеряем фактический rect окна.

use egui::{Align2, RichText, Vec2};

/// Безопасное центрирование по горизонтали: желаемая высота 0, чтобы
/// авторазмерное окно не растягивалось на всю доступную высоту.
fn centered_row(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let w = ui.available_width();
    ui.allocate_ui_with_layout(
        egui::vec2(w, 0.0),
        egui::Layout::left_to_right(egui::Align::Center).with_main_align(egui::Align::Center),
        add,
    );
}

fn probe(name: &str, variant: u8) {
    let ctx = egui::Context::default();
    let mut size = Vec2::ZERO;
    for _ in 0..4 {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 800.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| {
            if let Some(ir) = egui::Window::new(RichText::new(name))
                .title_bar(false)
                .resizable(false)
                .collapsible(false)
                .anchor(Align2::CENTER_CENTER, (0.0, 0.0))
                .min_size(egui::vec2(420.0, 0.0))
                .show(ctx, |ui| {
                    ui.add_space(12.0);
                    if variant == 4 {
                        ui.set_min_width(400.0);
                    }
                    match variant {
                        // текущий код: заголовок через horizontal_centered
                        1 => {
                            ui.horizontal_centered(|ui| {
                                ui.label(RichText::new("Рекурсивное удаление").strong().size(16.0));
                            });
                            ui.add_space(8.0);
                            ui.add(egui::Label::new("Удалять пустые родительские папки?").wrap());
                            ui.add_space(14.0);
                            ui.horizontal_centered(|ui| {
                                let _ = ui.button("Да");
                                let _ = ui.button("Нет");
                            });
                        }
                        // вариант без horizontal_centered
                        2 => {
                            ui.label(RichText::new("Рекурсивное удаление").strong().size(16.0));
                            ui.add_space(8.0);
                            ui.add(egui::Label::new("Удалять пустые родительские папки?").wrap());
                            ui.add_space(14.0);
                            ui.horizontal(|ui| {
                                let _ = ui.button("Да");
                                let _ = ui.button("Нет");
                            });
                        }
                        // вариант: центрирование через явный desired height = 0
                        3 | 4 => {
                            centered_row(ui, |ui| {
                                ui.label(RichText::new("Рекурсивное удаление").strong().size(16.0));
                            });
                            ui.add_space(8.0);
                            ui.add(egui::Label::new("Удалять пустые родительские папки?").wrap());
                            ui.add_space(14.0);
                            centered_row(ui, |ui| {
                                let _ = ui.button("Да");
                                let _ = ui.button("Нет");
                            });
                        }
                        // вариант: только кнопки в horizontal_centered
                        _ => {
                            ui.label(RichText::new("Рекурсивное удаление").strong().size(16.0));
                            ui.horizontal_centered(|ui| {
                                let _ = ui.button("Да");
                                let _ = ui.button("Нет");
                            });
                        }
                    }
                    ui.add_space(12.0);
                })
            {
                size = ir.response.rect.size();
            }
        });
    }
    println!("{name}: {:.0} x {:.0}", size.x, size.y);
}

fn main() {
    probe("hv_centered", 1);
    probe("plain", 2);
    probe("buttons_only", 0);
    probe("centered_row", 3);
    probe("centered_row_minwidth", 4);
}
