//! osu-lazer-cleaner: one executable holding both interfaces.
//!
//! Run it with no arguments and it opens the window. Give it a subcommand and it runs
//! headless, which is what makes the tool scriptable and lets a scan be checked against a real
//! library without a graphical session.

use eframe::egui;
mod about;
mod app;
mod cli;
mod console;
mod diagnostics;
mod icons;
mod theme;
mod worker;

use std::process::ExitCode;

fn main() -> ExitCode {
    // `args_os().len() > 1` rather than clap's own detection: clap cannot tell "no arguments"
    // from "arguments it rejects", and a typo must print an error rather than open a window.
    if std::env::args_os().len() > 1 {
        return cli::main();
    }

    console::release();
    diagnostics::install(diagnostics::Interface::Window);
    gui()
}

/// Opens the window.
fn gui() -> ExitCode {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1024.0, 680.0])
            .with_min_inner_size([760.0, 520.0])
            .with_title("osu!lazer Cleaner")
            .with_icon(app_icon()),
        ..eframe::NativeOptions::default()
    };

    let started = eframe::run_native(
        "osu-lazer-cleaner",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    );

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
