//! Updates to osu!lazer Cleaner itself: the header chip that points out a new version, and the
//! window with its release notes, the install, and how often to check.
//!
//! `app-update` does the checking, downloading, and swapping. This module only draws it.

use crate::icons;
use crate::theme;
use app_update::{CheckPeriod, NoteBlock, Settings, Update, Updater};
use eframe::egui::{self, RichText};

/// Where this app's releases are published. `bin` matches `BIN` in `release.yml`.
pub(crate) const APP_INFO: app_update::AppInfo = app_update::AppInfo {
    owner: "handsomefox",
    repo: "osu-lazer-cleaner",
    bin: "osu-lazer-cleaner",
    version: env!("CARGO_PKG_VERSION"),
};

/// Storage key holding the update settings, as JSON.
pub(crate) const SETTINGS_KEY: &str = "app_updates";

/// What the user chose in the window this frame.
enum Action {
    Download,
    Restart,
    Skip,
    Later,
}

/// A chip in the header while a new version is out. Returns true when it was clicked.
pub(crate) fn chip(ui: &mut egui::Ui, updater: &Updater, settings: &Settings) -> bool {
    let Some(release) = updater.notification(settings) else {
        return false;
    };
    let text = if updater.staged().is_some() {
        format!("Restart for {}", release.version)
    } else {
        format!("Update {} available", release.version)
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
        .on_hover_text("A new version of osu!lazer Cleaner is out. Click to see what changed.")
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

    let mut check = false;
    let mut action = None;
    let mut close = false;
    let height = (ctx.content_rect().height() - 260.0).max(160.0);
    let response = egui::Modal::new(egui::Id::new("app-update")).show(ctx, |ui| {
        ui.set_width(520.0);
        ui.label(
            RichText::new(icons::labelled(icons::UPDATE, "App updates"))
                .family(theme::bold())
                .heading(),
        );
        ui.add_space(8.0);

        egui::ScrollArea::vertical()
            .max_height(height)
            .auto_shrink([false, true])
            .show(ui, |ui| status(ui, updater, settings));

        if let Some(error) = updater.error() {
            ui.add_space(8.0);
            ui.label(RichText::new(icons::labelled(icons::ERROR, error)).color(theme::BAD));
        }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);
        check = schedule(ui, updater, settings);
        ui.add_space(12.0);

        action = actions(ui, updater, idle, &mut close);
    });

    if check {
        updater.check_now();
    }
    match action {
        Some(Action::Download) => updater.download(),
        Some(Action::Restart) => {
            if updater.install_and_restart().is_ok() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        Some(Action::Skip) => {
            updater.skip(settings);
            close = true;
        }
        Some(Action::Later) => {
            updater.dismiss();
            close = true;
        }
        None => {}
    }
    if close || response.should_close() {
        *open = false;
    }
}

/// Where the running version stands against GitHub.
fn status(ui: &mut egui::Ui, updater: &Updater, settings: &Settings) {
    let current = updater.current().map_or_else(
        || APP_INFO.version.to_owned(),
        |version| version.to_string(),
    );
    if let Some(update) = updater.update() {
        available(ui, updater, settings, update, &current);
    } else if updater.is_checking() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Checking GitHub for a new version");
        });
    } else if updater.is_up_to_date() {
        ui.label(
            RichText::new(icons::labelled(
                icons::SUCCESS,
                &format!("Version {current} is the latest version."),
            ))
            .color(theme::GOOD),
        );
    } else {
        ui.label(
            RichText::new(format!(
                "You have version {current}. It has not checked for a new version yet."
            ))
            .color(theme::MUTED),
        );
    }
}

/// The newest version, where the install stands, and the notes of every release since the
/// running one.
fn available(
    ui: &mut egui::Ui,
    updater: &Updater,
    settings: &Settings,
    update: &Update,
    current: &str,
) {
    let latest = update.latest();
    ui.label(theme::figure(format!(
        "Version {} is available",
        latest.version
    )));
    let released = if latest.date.is_empty() {
        String::new()
    } else {
        format!(" It was released on {}.", latest.date)
    };
    ui.label(RichText::new(format!("You have version {current}.{released}")).color(theme::MUTED));

    if updater.staged() == Some(latest.version) {
        ui.add_space(6.0);
        ui.label(
            RichText::new(icons::labelled(
                icons::SUCCESS,
                "Downloaded and verified. Restart osu!lazer Cleaner to finish.",
            ))
            .color(theme::GOOD),
        );
    } else if settings.skipped.as_deref() == Some(latest.version.to_string().as_str()) {
        ui.add_space(6.0);
        ui.label(
            RichText::new("You skipped this version, so the header does not point it out.")
                .color(theme::MUTED),
        );
    }
    if !updater.can_install() {
        ui.add_space(6.0);
        ui.label(
            RichText::new(
                "This release has no build that osu!lazer Cleaner can install by itself. \
                 Download it from the release page instead.",
            )
            .color(theme::MUTED),
        );
        ui.hyperlink_to(
            icons::labelled(icons::LINK, "Release page"),
            &latest.page_url,
        );
    }

    ui.add_space(12.0);
    ui.label(RichText::new("What's new").family(theme::bold()));
    ui.add_space(4.0);
    for release in update.releases() {
        ui.horizontal(|ui| {
            ui.label(RichText::new(release.version.to_string()).family(theme::bold()));
            if !release.date.is_empty() {
                ui.label(RichText::new(&release.date).small().color(theme::MUTED));
            }
        });
        notes(ui, &release.notes);
        ui.add_space(8.0);
    }
    ui.hyperlink_to(
        icons::labelled(icons::LINK, "All releases"),
        APP_INFO.releases_page(),
    );
}

/// One release's notes, laid out from the Markdown subset `CHANGELOG.md` uses.
fn notes(ui: &mut egui::Ui, markdown: &str) {
    let blocks = app_update::parse_notes(markdown);
    if blocks.is_empty() {
        ui.label(RichText::new("No notes for this release.").color(theme::MUTED));
    }
    for block in blocks {
        match block {
            NoteBlock::Heading(text) => {
                ui.label(RichText::new(text).family(theme::bold()));
            }
            NoteBlock::Bullet(text) => {
                ui.horizontal_top(|ui| {
                    ui.label(RichText::new("•").color(theme::MUTED));
                    ui.add(egui::Label::new(text).wrap());
                });
            }
            NoteBlock::Paragraph(text) => {
                ui.add(egui::Label::new(text).wrap());
            }
        }
    }
}

/// How often to check, and a way to check now. Returns true when the user asked for a check.
fn schedule(ui: &mut egui::Ui, updater: &Updater, settings: &mut Settings) -> bool {
    let mut check = false;
    ui.horizontal(|ui| {
        ui.label("Check for new versions");
        egui::ComboBox::from_id_salt("app-update-period")
            .selected_text(settings.period.label())
            .show_ui(ui, |ui| {
                for option in CheckPeriod::ALL {
                    ui.selectable_value(&mut settings.period, option, option.label());
                }
            });
        check = ui
            .add_enabled(
                !updater.is_busy(),
                egui::Button::new(icons::labelled(icons::RESCAN, "Check now")),
            )
            .clicked();
    });
    let last = settings.last_check.map_or_else(
        || "Never checked".to_owned(),
        |then| {
            format!(
                "Last checked {}",
                app_update::describe_age(then, app_update::now())
            )
        },
    );
    ui.label(
        RichText::new(format!(
            "{last}. A check asks GitHub for this app's releases and sends nothing else. \
             The command line never checks."
        ))
        .small()
        .color(theme::MUTED),
    );
    check
}

/// The download progress, or the buttons that decide what happens to the update.
fn actions(ui: &mut egui::Ui, updater: &Updater, idle: bool, close: &mut bool) -> Option<Action> {
    if let Some((received, total)) = updater.progress() {
        let percent = total
            .filter(|total| *total > 0)
            .map_or(0, |total| received.saturating_mul(100) / total)
            .min(100);
        let fraction = f32::from(u8::try_from(percent).unwrap_or(100)) / 100.0;
        let version = updater
            .update()
            .map(|update| update.latest().version.to_string())
            .unwrap_or_default();
        ui.add(egui::ProgressBar::new(fraction).text(format!("Downloading {version}… {percent}%")));
        return None;
    }

    let mut action = None;
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if updater.update().is_none() {
            if ui.button("Close").clicked() {
                *close = true;
            }
            return;
        }

        if updater.staged().is_some() {
            let button = egui::Button::new(
                RichText::new(icons::labelled(icons::RESCAN, "Restart to update"))
                    .color(theme::BASE),
            )
            .fill(theme::REMOVE);
            if ui
                .add_enabled(idle, button)
                .on_hover_text("Close osu!lazer Cleaner and open the new version")
                .on_disabled_hover_text("Wait for the current work to finish")
                .clicked()
            {
                action = Some(Action::Restart);
            }
        } else if updater.can_install() {
            let button = egui::Button::new(
                RichText::new(icons::labelled(icons::DOWNLOAD, "Download update"))
                    .color(theme::BASE),
            )
            .fill(theme::REMOVE);
            if ui
                .add_enabled(!updater.is_busy(), button)
                .on_hover_text(
                    "Download the new version and check it against its published SHA-256",
                )
                .clicked()
            {
                action = Some(Action::Download);
            }
        }
        if ui
            .button("Skip this version")
            .on_hover_text("Stop pointing out this version. A newer one shows up again.")
            .clicked()
        {
            action = Some(Action::Skip);
        }
        if ui
            .button("Not now")
            .on_hover_text("Hide the reminder until osu!lazer Cleaner starts again")
            .clicked()
        {
            action = Some(Action::Later);
        }
    });
    action
}
