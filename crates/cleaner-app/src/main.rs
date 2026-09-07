//! Desktop interface for osu-lazer-cleaner.

// The window subsystem keeps a console from appearing behind the application on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
mod app;
mod diagnostics;
mod theme;
mod worker;

use std::process::ExitCode;

fn main() -> ExitCode {
    diagnostics::install();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1024.0, 680.0])
            .with_min_inner_size([760.0, 520.0])
            .with_title("osu!lazer Cleaner"),
        ..eframe::NativeOptions::default()
    };

    let started = eframe::run_native(
        "osu-lazer-cleaner",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(&cc.egui_ctx)))),
    );

    match started {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{error}");
            ExitCode::FAILURE
        }
    }
}
