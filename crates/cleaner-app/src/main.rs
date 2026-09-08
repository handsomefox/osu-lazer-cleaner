//! osu-lazer-cleaner: one executable holding both interfaces.
//!
//! Run it with no arguments and it opens the window. Give it a subcommand and it runs
//! headless, which is what makes the tool scriptable and lets a scan be checked against a real
//! library without a graphical session.

use eframe::egui;
mod app;
mod cli;
mod console;
mod diagnostics;
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
    diagnostics::install();
    gui()
}

/// Opens the window.
fn gui() -> ExitCode {
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
