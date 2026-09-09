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

    #[cfg(windows)]
    open_in_explorer(&directory);

    #[cfg(not(windows))]
    {
        let command = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        if let Err(error) = std::process::Command::new(command).arg(&directory).spawn() {
            tracing::warn!("failed to open the log folder: {error}");
        }
    }
}

/// Asks the shell to open a folder.
///
/// `Command::new("explorer")` fails here with `ERROR_NOT_SUPPORTED`, which the sibling apps
/// never see because they are linked for the window subsystem. This one is linked for the
/// console subsystem and releases its console before the window opens, so it does not get to
/// spawn Explorer the ordinary way. `ShellExecuteW` is the documented way to ask the shell to
/// open something, and starts no process of ours.
#[cfg(windows)]
fn open_in_explorer(directory: &std::path::Path) {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let mut path: Vec<u16> = directory.as_os_str().encode_wide().collect();
    if path.contains(&0) {
        tracing::warn!("the log directory path contains NUL");
        return;
    }
    path.push(0);
    let verb: Vec<u16> = "open\0".encode_utf16().collect();

    // SAFETY: both strings are NUL-terminated and live through this synchronous call, and every
    // other argument is the documented null. winit initialises COM on the thread that runs the
    // window, which is this one, and ShellExecuteW needs it for shell extensions.
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            path.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };

    // The return value is an instance handle above 32, and an error code at or below it.
    let code = result as usize;
    if code <= 32 {
        tracing::warn!(code, "failed to open the log folder");
    }
}
