//! Updates to osu!lazer Cleaner itself: the header chip that points out a new version, and the
//! window around the update dialog.
//!
//! `app-update` does the checking, downloading, and swapping, and `app-update-ui` draws the
//! dialog's content. This module keeps the chip, the modal, and the [`Look`] that gives the
//! dialog this app's palette, type, and icons.

use crate::icons;
use crate::theme;
use app_update::{Settings, Updater};
use app_update_ui::Icon;
use eframe::egui::{self, RichText};

/// Where this app's releases are published. `bin` matches `BIN` in `release.yml`.
pub(crate) const APP_INFO: app_update::AppInfo = app_update::AppInfo {
    name: "osu!lazer Cleaner",
    owner: "handsomefox",
    repo: "osu-lazer-cleaner",
    bin: "osu-lazer-cleaner",
    version: env!("CARGO_PKG_VERSION"),
};

/// Storage key holding the update settings, as JSON.
pub(crate) const SETTINGS_KEY: &str = "app_updates";

/// This app's theme, as the shared dialog sees it.
struct Look;

impl app_update_ui::Look for Look {
    fn muted(&self, _visuals: &egui::Visuals) -> egui::Color32 {
        theme::MUTED
    }

    fn success(&self, _visuals: &egui::Visuals) -> egui::Color32 {
        theme::GOOD
    }

    fn warning(&self, _visuals: &egui::Visuals) -> egui::Color32 {
        theme::BAD
    }

    fn heading(&self, text: RichText) -> RichText {
        // The figure size, as `theme::figure` sets it.
        text.text_style(egui::TextStyle::Name("Figure".into()))
    }

    fn section(&self, text: RichText) -> RichText {
        text.family(theme::bold())
    }

    fn bold(&self, text: RichText) -> RichText {
        text.family(theme::bold())
    }

    fn glyph(&self, icon: Icon) -> Option<&'static str> {
        match icon {
            Icon::CheckNow | Icon::Restart => Some(icons::RESCAN),
            Icon::Download => Some(icons::DOWNLOAD),
            Icon::Success => Some(icons::SUCCESS),
            Icon::Warning => Some(icons::ERROR),
            Icon::Link => Some(icons::LINK),
            Icon::Skip | Icon::Later => None,
        }
    }

    /// Pink, because the button moves the update on.
    fn primary_button(&self, icon: Icon, text: &str) -> egui::Button<'static> {
        egui::Button::new(self.icon_text(icon, text, Some(theme::BASE))).fill(theme::REMOVE)
    }
}

/// A chip in the header while a new version is out. Returns true when it was clicked.
pub(crate) fn chip(ui: &mut egui::Ui, updater: &Updater, settings: &Settings) -> bool {
    let Some(text) = app_update_ui::chip_text(updater, settings) else {
        return false;
    };
    let button = egui::Button::new(
        RichText::new(icons::labelled(icons::UPDATE, &text))
            .color(theme::REMOVE)
            .small(),
    )
    .fill(theme::REMOVE.gamma_multiply(0.16))
    .stroke(egui::Stroke::NONE)
    .corner_radius(egui::CornerRadius::same(10));
    ui.add(button)
        .on_hover_text(app_update_ui::chip_hover_text(updater))
        .clicked()
}

/// Draws the update window while `open` is set, and clears it when the user closes it.
///
/// `idle` is false while the app scans, cleans, restores, deletes, or compacts. Restarting then
/// would cut that work off, so the restart waits for it.
pub(crate) fn show(
    ctx: &egui::Context,
    open: &mut bool,
    updater: &mut Updater,
    settings: &mut Settings,
    idle: bool,
) {
    if !*open {
        return;
    }

    let mut chosen = None;
    let mut close = false;
    let response = egui::Modal::new(egui::Id::unique("app-update")).show(ctx, |ui| {
        ui.set_width(520.0);
        ui.label(
            RichText::new(icons::labelled(icons::UPDATE, app_update_ui::TITLE))
                .family(theme::bold())
                .heading(),
        );
        ui.add_space(8.0);

        chosen = app_update_ui::body(ui, &Look, updater, settings);
        let busy = (!idle).then_some("Wait for the current work to finish");
        chosen = app_update_ui::actions(ui, &Look, updater, busy).or(chosen);

        // The shared buttons appear only while an update is out. A modal has no title bar, so
        // it needs a way out the rest of the time.
        if updater.update().is_none() {
            ui.add_space(12.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close = ui.button("Close").clicked();
            });
        }
    });

    if let Some(action) = chosen
        && app_update_ui::apply(action, ctx, updater, settings)
    {
        close = true;
    }
    if close || response.should_close() {
        *open = false;
    }
}
