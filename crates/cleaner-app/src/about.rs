//! The About window: icon, version, credits, project link, and the way to the log folder.

use crate::theme;
use eframe::egui::{self, RichText};

/// Where the source lives, which is also where issues go.
const REPOSITORY: &str = "https://github.com/handsomefox/osu-lazer-cleaner";

/// Draws the About window while `open` is set, and clears it when the user closes it.
pub(crate) fn show(ctx: &egui::Context, open: &mut bool) {
    if !*open {
        return;
    }

    let mut close = false;
    let response = egui::Modal::new(egui::Id::new("about")).show(ctx, |ui| {
        ui.set_max_width(440.0);

        ui.horizontal(|ui| {
            ui.add(
                egui::Image::new(egui::include_image!("../assets/icon.png"))
                    .fit_to_exact_size(egui::vec2(64.0, 64.0)),
            );
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("osu!lazer Cleaner")
                        .family(theme::bold())
                        .heading(),
                );
                ui.label(
                    RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION")))
                        .color(theme::MUTED),
                );
                ui.label(
                    RichText::new("Inter, SIL Open Font License 1.1. Phosphor Icons, MIT.")
                        .small()
                        .color(theme::MUTED),
                );
                ui.hyperlink_to("Project page", REPOSITORY);
            });
        });

        ui.add_space(6.0);
        ui.separator();
        ui.add_space(6.0);

        ui.horizontal(|ui| {
            if ui
                .button("Open log folder")
                .on_hover_text("The newest log file is the one to attach to an issue")
                .clicked()
            {
                open_log_folder();
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Close").clicked() {
                    close = true;
                }
            });
        });
    });

    if close || response.should_close() {
        *open = false;
    }
}

/// Opens the log directory in the system file manager, so the user can pick up the file rather
/// than be told a path to type.
fn open_log_folder() {
    let Some(directory) = cleaner_core::folders::logs_dir() else {
        tracing::warn!("no log directory to open");
        return;
    };
    if let Err(error) = std::fs::create_dir_all(&directory) {
        tracing::warn!("failed to create the log directory: {error}");
        return;
    }

    let command = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if let Err(error) = std::process::Command::new(command).arg(&directory).spawn() {
        tracing::warn!("failed to open the log folder: {error}");
    }
}
