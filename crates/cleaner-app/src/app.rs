//! Application state and screen layout.

use crate::worker::{Command, Event, SnapshotSummary, Worker};
use cleaner_core::{Category, Plan, human_bytes};
use eframe::egui;
use std::collections::HashSet;
use std::path::PathBuf;

/// Which screen is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    /// Categories and scan results.
    Clean,
    /// Snapshots from previous cleans.
    Snapshots,
}

/// A confirmation the user has to answer before anything is removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Modal {
    /// Nothing is selected, so there is nothing to do.
    NothingSelected,
    /// About to remove files.
    ConfirmClean,
    /// About to delete a snapshot, which cannot be undone.
    ConfirmDelete,
}

/// The whole application.
pub(crate) struct App {
    worker: Worker,
    screen: Screen,
    library: Option<PathBuf>,
    status: String,
    error: Option<String>,
    busy: bool,
    plan: Option<Plan>,
    selected: HashSet<Category>,
    snapshots: Vec<SnapshotSummary>,
    modal: Option<Modal>,
    pending_delete: Option<String>,
    last_result: Option<String>,
}

impl App {
    /// Builds the application and asks the worker to find a library.
    #[must_use]
    pub(crate) fn new(ctx: &egui::Context) -> Self {
        let worker = Worker::spawn(ctx.clone());
        worker.send(Command::OpenLibrary { path: None });

        let selected = Category::ALL
            .iter()
            .copied()
            .filter(|c| c.default_selected())
            .collect();

        Self {
            worker,
            screen: Screen::Clean,
            library: None,
            status: "Looking for an osu!lazer library".to_owned(),
            error: None,
            busy: true,
            plan: None,
            selected,
            snapshots: Vec::new(),
            modal: None,
            pending_delete: None,
            last_result: None,
        }
    }

    /// Applies everything the worker reported since the last frame.
    fn absorb_events(&mut self) {
        for event in self.worker.drain() {
            match event {
                Event::LibraryOpened { root } => {
                    "Ready to scan".clone_into(&mut self.status);
                    self.library = Some(root);
                    self.busy = false;
                    self.error = None;
                    self.worker.send(Command::ListSnapshots);
                }
                Event::Progress { message } => self.status = message,
                Event::Scanned { plan } => {
                    // Untick anything the scan found nothing for, so the selection describes
                    // what is actually there.
                    for group in &plan.groups {
                        if group.files == 0 {
                            self.selected.remove(&group.category);
                        }
                    }

                    self.status = format!(
                        "Scanned {} beatmap sets holding {}",
                        plan.sets_scanned,
                        human_bytes(plan.bytes_total)
                    );
                    self.plan = Some(*plan);
                    self.busy = false;
                }
                Event::Cleaned { files, bytes } => {
                    self.last_result = Some(format!(
                        "Removed {files} files. Deleting the snapshot will reclaim {}.",
                        human_bytes(bytes)
                    ));
                    "Clean complete".clone_into(&mut self.status);
                    self.plan = None;
                    self.busy = false;
                    self.worker.send(Command::ListSnapshots);
                }
                Event::Snapshots { entries } => self.snapshots = entries,
                Event::Failed { message } => {
                    self.error = Some(message);
                    self.busy = false;
                }
            }
        }
    }

    /// Starts a scan.
    fn scan(&mut self) {
        self.busy = true;
        self.error = None;
        self.last_result = None;
        "Scanning".clone_into(&mut self.status);
        self.worker.send(Command::Scan {
            selected: self.selected.clone(),
        });
    }

    /// Decides which confirmation the clean button should raise.
    fn request_clean(&mut self) {
        let selected_files = self.plan.as_ref().map_or(0, Plan::selected_files);
        self.modal = Some(if selected_files == 0 {
            Modal::NothingSelected
        } else {
            Modal::ConfirmClean
        });
    }

    /// Sends the clean the user confirmed.
    fn confirm_clean(&mut self) {
        let Some(plan) = self.plan.clone() else {
            return;
        };

        self.busy = true;
        "Removing files".clone_into(&mut self.status);
        self.worker.send(Command::Clean {
            plan: Box::new(plan),
        });
    }

    /// Keeps the plan's selection in step with the checkboxes.
    fn apply_selection(&mut self) {
        let Some(plan) = self.plan.as_mut() else {
            return;
        };

        for category in Category::ALL {
            plan.select(*category, self.selected.contains(category));
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.absorb_events();
        let ctx = root.ctx().clone();

        egui::Panel::top("header").show(root, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("osu!lazer Cleaner");
                ui.separator();
                ui.selectable_value(&mut self.screen, Screen::Clean, "Clean");
                ui.selectable_value(&mut self.screen, Screen::Snapshots, "Snapshots");

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(root) = &self.library {
                        ui.label(root.display().to_string());
                    }
                });
            });
            ui.add_space(6.0);
        });

        egui::Panel::bottom("status").show(root, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if self.busy {
                    ui.spinner();
                }
                ui.label(&self.status);
            });
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(root, |ui| {
            if let Some(error) = self.error.clone() {
                ui.colored_label(egui::Color32::from_rgb(220, 80, 80), error);
                ui.separator();
            }

            match self.screen {
                Screen::Clean => self.clean_screen(ui),
                Screen::Snapshots => self.snapshots_screen(ui),
            }
        });

        self.show_modal(&ctx);
    }
}

impl App {
    /// Scan results, with a checkbox beside each category.
    ///
    /// The selection lives in the results table rather than in a separate panel. A scan always
    /// classifies every category, so a panel of checkboxes above a scan button would imply the
    /// two are connected when they are not.
    fn clean_screen(&mut self, ui: &mut egui::Ui) {
        if let Some(result) = self.last_result.clone() {
            ui.colored_label(egui::Color32::from_rgb(120, 190, 120), result);
            ui.label("Start osu!lazer and check your beatmaps before deleting the snapshot.");
            ui.separator();
        }

        ui.horizontal(|ui| {
            ui.add_enabled_ui(!self.busy && self.library.is_some(), |ui| {
                let label = if self.plan.is_some() {
                    "Scan again"
                } else {
                    "Scan library"
                };
                if ui.button(label).clicked() {
                    self.scan();
                }
            });

            if self.plan.is_none() {
                ui.label("Nothing has been scanned yet.");
            }
        });

        let Some(plan) = self.plan.clone() else {
            return;
        };

        ui.add_space(12.0);
        ui.strong("Tick what to remove");
        ui.add_space(6.0);

        egui::Grid::new("results")
            .num_columns(3)
            .spacing([28.0, 10.0])
            .striped(true)
            .show(ui, |ui| {
                for group in &plan.groups {
                    let found = group.files > 0;

                    ui.vertical(|ui| {
                        ui.add_enabled_ui(found, |ui| {
                            let mut on = self.selected.contains(&group.category);
                            if ui.checkbox(&mut on, group.category.label()).changed() {
                                if on {
                                    self.selected.insert(group.category);
                                } else {
                                    self.selected.remove(&group.category);
                                }
                                self.apply_selection();
                            }
                        });
                        ui.small(group.category.description());
                    });

                    let cell = |ui: &mut egui::Ui, value: String| {
                        let text = egui::RichText::new(value);
                        ui.label(if found { text } else { text.weak() });
                    };

                    cell(ui, group.files.to_string());
                    cell(ui, human_bytes(group.bytes));
                    ui.end_row();
                }
            });

        ui.add_space(14.0);
        ui.separator();
        ui.label(format!("Selected: {}", plan.summary()));
        ui.small("Files move into a snapshot. Nothing is deleted until you delete it.");
        ui.add_space(8.0);

        ui.add_enabled_ui(!self.busy, |ui| {
            if ui.button("Clean").clicked() {
                self.request_clean();
            }
        });
    }

    /// Snapshots, with restore and delete.
    fn snapshots_screen(&mut self, ui: &mut egui::Ui) {
        ui.strong("Snapshots");
        ui.small("Each snapshot holds the files one clean removed. Restoring puts them back.");
        ui.add_space(8.0);

        if self.snapshots.is_empty() {
            ui.label("No snapshots yet.");
            return;
        }

        let entries = self.snapshots.clone();
        egui::Grid::new("snapshots")
            .num_columns(5)
            .spacing([18.0, 8.0])
            .striped(true)
            .show(ui, |ui| {
                ui.strong("Taken");
                ui.strong("Files");
                ui.strong("Size");
                ui.strong("");
                ui.strong("");
                ui.end_row();

                for entry in &entries {
                    ui.label(&entry.created);
                    ui.label(entry.files.to_string());
                    ui.label(human_bytes(entry.bytes));

                    if ui.button("Restore").clicked() {
                        self.worker.send(Command::RestoreSnapshot {
                            id: entry.id.clone(),
                        });
                    }

                    if ui.button("Delete").clicked() {
                        self.pending_delete = Some(entry.id.clone());
                        self.modal = Some(Modal::ConfirmDelete);
                    }

                    ui.end_row();
                }
            });
    }

    /// Draws whichever confirmation is open.
    fn show_modal(&mut self, ctx: &egui::Context) {
        let Some(modal) = self.modal else {
            return;
        };

        let (title, body, action) = match modal {
            Modal::NothingSelected => (
                "Nothing selected",
                "Choose at least one category to remove.".to_owned(),
                None,
            ),
            Modal::ConfirmClean => {
                let summary = self.plan.as_ref().map(Plan::summary).unwrap_or_default();
                (
                    "Remove files?",
                    format!(
                        "{summary} will move into a snapshot.\n\nNothing is deleted yet. Your \
                         disk space is reclaimed when you delete the snapshot, after you have \
                         checked that osu!lazer still works."
                    ),
                    Some("Clean"),
                )
            }
            Modal::ConfirmDelete => {
                let size = self
                    .pending_delete
                    .as_ref()
                    .and_then(|id| self.snapshots.iter().find(|s| &s.id == id))
                    .map_or_else(String::new, |s| human_bytes(s.bytes));
                (
                    "Delete this snapshot?",
                    format!(
                        "This frees {size} and cannot be undone. The files it holds are gone \
                         for good."
                    ),
                    Some("Delete permanently"),
                )
            }
        };

        egui::Modal::new(egui::Id::new("confirm")).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.heading(title);
            ui.add_space(8.0);
            ui.label(body);
            ui.add_space(12.0);

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(label) = action
                    && ui.button(label).clicked()
                {
                    {
                        match modal {
                            Modal::ConfirmClean => self.confirm_clean(),
                            Modal::ConfirmDelete => {
                                if let Some(id) = self.pending_delete.take() {
                                    self.worker.send(Command::DeleteSnapshot { id });
                                }
                            }
                            Modal::NothingSelected => {}
                        }
                        self.modal = None;
                    }
                }

                if ui.button("Cancel").clicked() {
                    self.pending_delete = None;
                    self.modal = None;
                }
            });
        });
    }
}
