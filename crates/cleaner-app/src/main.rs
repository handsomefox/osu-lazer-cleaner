//! osu-lazer-cleaner: the window.

#![cfg_attr(windows, windows_subsystem = "windows")]

use eframe::egui;
mod about;
mod app;
mod diagnostics;
mod icons;
mod theme;
mod update;
mod worker;

use std::process::ExitCode;

/// Opens the window. Arguments are ignored: there is nothing to configure from a shell.
fn main() -> ExitCode {
    diagnostics::install();
    gui()
}

/// Names the application to eframe and to the desktop.
///
/// eframe keeps the window's saved state in a directory named after it, so changing it loses
/// the screen the window reopens on. Wayland compositors match it against a `.desktop` file.
const APP_ID: &str = "osu-lazer-cleaner";

/// Opens the window.
fn gui() -> ExitCode {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id(APP_ID)
            .with_inner_size([1024.0, 680.0])
            .with_min_inner_size([760.0, 520.0])
            .with_title("osu!lazer Cleaner")
            .with_icon(app_icon()),
        ..eframe::NativeOptions::default()
    };

    let started = eframe::run_native(
        APP_ID,
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    );

    // eframe saves the app's state before `run_native` returns, so the updated copy starts
    // from everything this one knew.
    if let Err(error) = app_update::restart_if_requested() {
        tracing::error!("failed to start the updated app: {error}");
    }

    match started {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{error}");
            ExitCode::FAILURE
        }
    }
}

/// The window and taskbar icon. The executable carries the same artwork as a Windows resource,
/// which is what Explorer shows before the window opens.
fn app_icon() -> egui::IconData {
    let bytes = include_bytes!("../assets/icon.png");
    match image::load_from_memory(bytes) {
        Ok(decoded) => {
            let rgba = decoded.into_rgba8();
            let (width, height) = rgba.dimensions();
            egui::IconData {
                rgba: rgba.into_raw(),
                width,
                height,
            }
        }
        Err(error) => {
            tracing::warn!("failed to decode the window icon: {error}");
            egui::IconData::default()
        }
    }
}
