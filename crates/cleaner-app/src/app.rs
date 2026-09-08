//! Application state and screen layout.

use crate::theme;
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

/// What the worker is doing, when it is doing something.
///
/// Scanning only reads, so interrupting it costs nothing. The other three move files and edit
/// the database, and stopping one part-way would leave the library needing a repair. That
/// difference decides whether closing the window asks first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Activity {
    /// Reading the library. Safe to abandon.
    Scanning,
    /// Moving files into a snapshot.
    Cleaning,
    /// Moving files back out of a snapshot.
    Restoring,
    /// Deleting a snapshot permanently.
    Deleting,
    /// Rewriting the database without its free space.
    Compacting,
}

impl Activity {
    /// Whether stopping now could leave the library half-changed.
    fn is_destructive(self) -> bool {
        !matches!(self, Self::Scanning)
    }

    /// How to name it mid-sentence.
    fn describe(self) -> &'static str {
        match self {
            Self::Scanning => "scanning the library",
            Self::Cleaning => "moving files into a snapshot",
            Self::Restoring => "moving files back into the library",
            Self::Deleting => "deleting a snapshot",
            Self::Compacting => "rewriting the database",
        }
    }
}

/// A question the user has to answer before anything happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Question {
    /// About to remove files.
    Clean,
    /// About to delete a snapshot, which cannot be undone.
    Delete,
    /// About to move a snapshot's files back into the library.
    Restore,
    /// Closing while work is in flight.
    Quit(Activity),
}

/// Gap between a table's columns.
const COLUMN_GAP: f32 = 20.0;

/// Width of the byte-count column, set by the widest string it holds (`534.08 MB`).
const SIZE_WIDTH: f32 = 86.0;

/// Width of the file-count column, set by the widest grouped count (`1,234,567`).
const COUNT_WIDTH: f32 = 86.0;

/// What both numeric columns and their gaps take, so the text column can have the rest.
const NUMBER_COLUMNS: f32 = SIZE_WIDTH + COUNT_WIDTH + COLUMN_GAP * 2.0;

/// The whole application.
pub(crate) struct App {
    worker: Worker,
    screen: Screen,
    library: Option<PathBuf>,
    status: String,
    error: Option<String>,
    activity: Option<Activity>,
    plan: Option<Plan>,
    selected: HashSet<Category>,
    snapshots: Vec<SnapshotSummary>,
    /// Size of `client.realm`, so the compact control can show what it is working on.
    database_bytes: u64,
    /// What the last compaction did, if one has run.
    compaction: Option<cleaner_core::Compaction>,
    modal: Option<Question>,
    /// Snapshot the open question refers to, for restore and delete.
    pending_snapshot: Option<String>,
    last_result: Option<String>,
}

impl App {
    /// Builds the application and asks the worker to find a library.
    pub(crate) fn new(ctx: &egui::Context) -> Self {
        theme::apply(ctx);

        let worker = Worker::spawn(ctx.clone());
        worker.send(Command::OpenLibrary { path: None });

        Self {
            worker,
            screen: Screen::Clean,
            library: None,
            status: "Looking for an osu!lazer library".to_owned(),
            error: None,
            activity: Some(Activity::Scanning),
            plan: None,
            selected: Category::ALL
                .iter()
                .copied()
                .filter(|c| c.default_selected())
                .collect(),
            snapshots: Vec::new(),
            database_bytes: 0,
            compaction: None,
            modal: None,
            pending_snapshot: None,
            last_result: None,
        }
    }

    /// Whether the interface should accept input.
    ///
    /// Everything locks while the worker is busy. A second command sent mid-clean would queue
    /// behind it and then act on a plan describing a library that no longer exists.
    fn idle(&self) -> bool {
        self.activity.is_none()
    }

    /// Applies everything the worker reported since the last frame.
    fn absorb_events(&mut self) {
        for event in self.worker.drain() {
            match event {
                Event::LibraryOpened {
                    root,
                    database_bytes,
                } => {
                    "Ready to scan".clone_into(&mut self.status);
                    self.library = Some(root);
                    self.database_bytes = database_bytes;
                    self.activity = None;
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

                    self.status = format!("Scanned {} beatmap sets", plan.sets_scanned);
                    self.plan = Some(*plan);
                    self.activity = None;
                    self.apply_selection();
                }
                Event::Cleaned { files, bytes } => {
                    self.last_result = Some(format!(
                        "Moved {files} files into a snapshot. Deleting it reclaims {}.",
                        human_bytes(bytes)
                    ));
                    "Clean finished".clone_into(&mut self.status);
                    self.plan = None;
                    self.activity = None;
                    self.worker.send(Command::ListSnapshots);
                }
                Event::Compacted { result } => {
                    self.database_bytes = result.after;
                    "Compact finished".clone_into(&mut self.status);
                    self.compaction = Some(result);
                    self.activity = None;
                }
                Event::Snapshots { entries } => {
                    self.snapshots = entries;
                    self.activity = None;
                }
                Event::Failed { message } => {
                    self.error = Some(message);
                    self.activity = None;
                }
            }
        }
    }

    /// Starts a scan.
    fn scan(&mut self) {
        self.activity = Some(Activity::Scanning);
        self.error = None;
        self.last_result = None;
        "Scanning".clone_into(&mut self.status);
        self.worker.send(Command::Scan {
            selected: self.selected.clone(),
        });
    }

    /// Sends the clean the user confirmed.
    fn confirm_clean(&mut self) {
        let Some(plan) = self.plan.clone() else {
            return;
        };

        self.activity = Some(Activity::Cleaning);
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
        self.guard_close(&ctx);

        egui::Panel::top("header")
            .frame(
                egui::Frame::new()
                    .fill(theme::SURFACE)
                    .inner_margin(egui::Margin::symmetric(18, 12)),
            )
            .show(root, |ui| self.header(ui));

        egui::Panel::bottom("status")
            .frame(
                egui::Frame::new()
                    .fill(theme::SURFACE)
                    .inner_margin(egui::Margin::symmetric(18, 9)),
            )
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    if self.activity.is_some() {
                        ui.spinner();
                    }
                    ui.label(egui::RichText::new(&self.status).color(theme::MUTED));
                });
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BASE)
                    .inner_margin(egui::Margin::symmetric(18, 16)),
            )
            .show(root, |ui| {
                if let Some(error) = self.error.clone() {
                    ui.label(egui::RichText::new(error).color(theme::BAD));
                    ui.add_space(10.0);
                }

                egui::ScrollArea::vertical().show(ui, |ui| match self.screen {
                    Screen::Clean => self.clean_screen(ui),
                    Screen::Snapshots => self.snapshots_screen(ui),
                });
            });

        self.show_modal(&ctx);
    }
}

impl App {
    /// Stops the window closing part-way through work that changes the library.
    fn guard_close(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) {
            return;
        }

        let Some(activity) = self.activity.filter(|a| a.is_destructive()) else {
            return;
        };

        // Already asked, and the answer has not arrived yet.
        if matches!(self.modal, Some(Question::Quit(_))) {
            return;
        }

        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        self.modal = Some(Question::Quit(activity));
    }

    /// Title, screen switch, and the library path.
    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("osu!")
                    .heading()
                    .color(theme::REMOVE)
                    .strong(),
            );
            ui.add_space(-6.0);
            ui.label(egui::RichText::new("lazer Cleaner").heading());

            ui.add_space(18.0);
            ui.add_enabled_ui(self.idle(), |ui| {
                ui.selectable_value(&mut self.screen, Screen::Clean, "Clean");
                ui.selectable_value(&mut self.screen, Screen::Snapshots, "Snapshots");
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(root) = &self.library {
                    ui.label(
                        egui::RichText::new(root.display().to_string())
                            .small()
                            .color(theme::MUTED),
                    );
                }
            });
        });
    }

    /// The library, what it is made of, and what to take out of it.
    fn clean_screen(&mut self, ui: &mut egui::Ui) {
        if let Some(result) = self.last_result.clone() {
            ui.label(egui::RichText::new(result).color(theme::GOOD));
            ui.label(
                egui::RichText::new(
                    "Start osu!lazer and check your beatmaps before deleting the snapshot.",
                )
                .small()
                .color(theme::MUTED),
            );
            ui.add_space(14.0);
        }

        let Some(plan) = self.plan.clone() else {
            ui.label(theme::figure("Nothing scanned yet"));
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("Scanning reads your library. It changes nothing.")
                    .color(theme::MUTED),
            );
            ui.add_space(16.0);
            ui.add_enabled_ui(self.idle() && self.library.is_some(), |ui| {
                if ui.button("Scan library").clicked() {
                    self.scan();
                }
            });
            return;
        };

        let removing = plan.selected_bytes();

        ui.horizontal(|ui| {
            ui.label(theme::figure(human_bytes(plan.bytes_total)));
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(format!("across {} beatmap sets", plan.sets_scanned))
                    .color(theme::MUTED),
            );

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_enabled_ui(self.idle(), |ui| {
                    if ui.button("Scan again").clicked() {
                        self.scan();
                    }
                });
            });
        });

        ui.add_space(12.0);
        composition_bar(ui, plan.bytes_total, removing);
        ui.add_space(18.0);

        self.category_table(ui, &plan);

        ui.add_space(16.0);
        ui.separator();
        ui.add_space(10.0);

        ui.horizontal(|ui| {
            let enabled = self.idle() && removing > 0;
            ui.add_enabled_ui(enabled, |ui| {
                let label = if removing == 0 {
                    "Nothing selected".to_owned()
                } else {
                    format!("Move {} to a snapshot", human_bytes(removing))
                };

                let fill = if enabled { theme::REMOVE } else { theme::LINE };
                let button =
                    egui::Button::new(egui::RichText::new(label).color(theme::BASE)).fill(fill);

                if ui.add(button).clicked() {
                    self.modal = Some(Question::Clean);
                }
            });

            ui.label(
                egui::RichText::new("Files move into a snapshot. Nothing is deleted yet.")
                    .small()
                    .color(theme::MUTED),
            );
        });
    }

    /// One row per category: tick, name, what it means, how much space, how many files.
    fn category_table(&mut self, ui: &mut egui::Ui, plan: &Plan) {
        column_headings(ui);
        for (index, group) in plan.groups.iter().enumerate() {
            self.category_row(ui, group, index % 2 == 1);
        }
    }

    /// Draws one category, banded so the eye can carry a name across to its numbers.
    fn category_row(&mut self, ui: &mut egui::Ui, group: &cleaner_core::Group, striped: bool) {
        // Reserved now, painted once the row's height is known, so the band sits behind the
        // text rather than over it.
        let band = ui.painter().add(egui::Shape::Noop);

        let found = group.files > 0;
        let on = self.selected.contains(&group.category);
        let tint = if !found {
            theme::LINE
        } else if on {
            theme::REMOVE
        } else {
            theme::TEXT
        };

        let text_width = (ui.available_width() - NUMBER_COLUMNS).max(180.0);
        let row = ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = COLUMN_GAP;

            ui.allocate_ui_with_layout(
                egui::vec2(text_width, 0.0),
                egui::Layout::top_down(egui::Align::LEFT),
                |ui| {
                    ui.set_width(text_width);
                    self.category_label(ui, group, found, on);
                },
            );

            column(ui, SIZE_WIDTH, &human_bytes(group.bytes), tint);
            column(ui, COUNT_WIDTH, &theme::grouped(group.files), tint);
        });

        if striped {
            stripe(ui, band, row.response.rect);
        }
    }

    /// The tick, the category name, and the sentence explaining what removing it costs.
    fn category_label(
        &mut self,
        ui: &mut egui::Ui,
        group: &cleaner_core::Group,
        found: bool,
        on: bool,
    ) {
        ui.add_enabled_ui(found && self.idle(), |ui| {
            let mut ticked = on;
            let label = egui::RichText::new(group.category.label()).color(if on && found {
                theme::REMOVE
            } else {
                theme::TEXT
            });

            if ui.checkbox(&mut ticked, label).changed() {
                if ticked {
                    self.selected.insert(group.category);
                } else {
                    self.selected.remove(&group.category);
                }
                self.apply_selection();
            }
        });

        ui.label(
            egui::RichText::new(group.category.description())
                .small()
                .color(theme::MUTED),
        );
    }

    /// Snapshots, newest first, with restore and delete.
    fn snapshots_screen(&mut self, ui: &mut egui::Ui) {
        ui.label(theme::figure("Snapshots"));
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new("Each snapshot holds the files one clean removed.")
                .color(theme::MUTED),
        );
        ui.add_space(16.0);

        if self.snapshots.is_empty() {
            ui.label("No snapshots yet. Cleaning creates one.");
        } else {
            if self.snapshots.len() > 1 {
                ui.label(
                    egui::RichText::new(
                        "Restore newest first. Each snapshot was taken against the library as \
                         the one above it left it.",
                    )
                    .small()
                    .color(theme::MUTED),
                );
                ui.add_space(12.0);
            }

            for (position, entry) in self.snapshots.clone().iter().enumerate() {
                // Only the newest can be restored on its own; the rest wait their turn.
                self.snapshot_row(ui, entry, position == 0, position % 2 == 1);
            }
        }

        ui.add_space(22.0);
        ui.separator();
        ui.add_space(14.0);
        self.database_section(ui);
    }

    /// One snapshot: when it was taken, what it holds, and the two things to do with it.
    fn snapshot_row(
        &mut self,
        ui: &mut egui::Ui,
        entry: &SnapshotSummary,
        newest: bool,
        striped: bool,
    ) {
        let band = ui.painter().add(egui::Shape::Noop);

        let row = ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = COLUMN_GAP;

            // Right to left, so the buttons keep their place while the date column takes up
            // whatever the window leaves.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_enabled_ui(self.idle(), |ui| {
                    if ui.button("Delete").clicked() {
                        self.pending_snapshot = Some(entry.id.clone());
                        self.modal = Some(Question::Delete);
                    }
                });

                ui.add_enabled_ui(newest && self.idle(), |ui| {
                    let button = ui.button("Put files back");
                    if !newest {
                        button.on_disabled_hover_text("Restore the snapshot above this one first.");
                    } else if button.clicked() {
                        self.pending_snapshot = Some(entry.id.clone());
                        self.modal = Some(Question::Restore);
                    }
                });

                column(ui, COUNT_WIDTH, &theme::grouped(entry.files), theme::TEXT);
                column(ui, SIZE_WIDTH, &human_bytes(entry.bytes), theme::TEXT);

                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.label(&entry.created);
                });
            });
        });

        if striped {
            stripe(ui, band, row.response.rect);
        }
    }

    /// The database file, its size, and the one operation that shrinks it.
    ///
    /// A clean can detach hundreds of thousands of rows without `client.realm` losing a byte,
    /// because Realm keeps the freed space on an internal list to reuse. Showing the size next
    /// to the button is the only way that stops looking like the tool did nothing.
    fn database_section(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Database").heading());
            ui.add_space(6.0);
            if self.database_bytes > 0 {
                ui.label(theme::number(human_bytes(self.database_bytes)).color(theme::MUTED));
            }
        });

        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(
                "client.realm keeps the space its removed rows used until it is rewritten. \
                 Close osu!lazer first.",
            )
            .small()
            .color(theme::MUTED),
        );
        ui.add_space(12.0);

        ui.horizontal(|ui| {
            ui.add_enabled_ui(self.idle() && self.library.is_some(), |ui| {
                if ui.button("Compact database").clicked() {
                    self.compaction = None;
                    self.error = None;
                    self.activity = Some(Activity::Compacting);
                    self.worker.send(Command::Compact);
                }
            });

            if let Some(result) = self.compaction {
                let (text, colour) = describe_compaction(result);
                ui.label(egui::RichText::new(text).small().color(colour));
            }
        });
    }

    /// What an open question says and which button carries it out.
    ///
    /// Returns the title, the body, the label on the acting button, and whether that button
    /// should read as dangerous.
    fn wording(&self, modal: Question) -> (&'static str, String, &'static str, bool) {
        match modal {
            Question::Clean => {
                let plan = self.plan.as_ref();
                let bytes = plan.map_or(0, Plan::selected_bytes);
                let files = plan.map_or(0, Plan::selected_files);
                (
                    "Move these files to a snapshot?",
                    format!(
                        "{files} files, {}.\n\nYour free space will not change yet. Start \
                         osu!lazer, check your beatmaps, then delete the snapshot to reclaim the \
                         space, or put the files back if something is wrong.",
                        human_bytes(bytes)
                    ),
                    "Move to snapshot",
                    false,
                )
            }
            Question::Restore => {
                let entry = self
                    .pending_snapshot
                    .as_ref()
                    .and_then(|id| self.snapshots.iter().find(|s| &s.id == id));
                let files = entry.map_or(0, |s| s.files);
                let size = entry.map_or_else(String::new, |s| human_bytes(s.bytes));
                (
                    "Put these files back?",
                    format!(
                        "{files} files, {size}, return to your library, and the snapshot is \
                         removed.\n\nYour free space will not change: the files were never \
                         deleted, only moved aside."
                    ),
                    "Put files back",
                    false,
                )
            }
            Question::Delete => {
                let size = self
                    .pending_snapshot
                    .as_ref()
                    .and_then(|id| self.snapshots.iter().find(|s| &s.id == id))
                    .map_or_else(String::new, |s| human_bytes(s.bytes));
                (
                    "Delete this snapshot?",
                    format!(
                        "This reclaims {size} and cannot be undone. The files it holds are gone \
                         for good."
                    ),
                    "Delete permanently",
                    true,
                )
            }
            Question::Quit(activity) => (
                "Work is still running",
                format!(
                    "osu!lazer Cleaner is {}. Closing now could leave your library half-changed \
                     and needing a repair.",
                    activity.describe()
                ),
                "Close anyway",
                true,
            ),
        }
    }

    /// Draws whichever question is open.
    fn show_modal(&mut self, ctx: &egui::Context) {
        let Some(modal) = self.modal else {
            return;
        };

        let (title, body, action, danger) = self.wording(modal);

        egui::Modal::new(egui::Id::new("confirm")).show(ctx, |ui| {
            ui.set_width(430.0);
            ui.label(egui::RichText::new(title).heading());
            ui.add_space(10.0);
            ui.label(body);
            ui.add_space(16.0);

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let fill = if danger { theme::BAD } else { theme::REMOVE };
                let button =
                    egui::Button::new(egui::RichText::new(action).color(theme::BASE)).fill(fill);

                if ui.add(button).clicked() {
                    match modal {
                        Question::Clean => self.confirm_clean(),
                        Question::Restore => {
                            if let Some(id) = self.pending_snapshot.take() {
                                self.activity = Some(Activity::Restoring);
                                self.worker.send(Command::RestoreSnapshot { id });
                            }
                        }
                        Question::Delete => {
                            if let Some(id) = self.pending_snapshot.take() {
                                self.activity = Some(Activity::Deleting);
                                self.worker.send(Command::DeleteSnapshot { id });
                            }
                        }
                        Question::Quit(_) => {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    }
                    self.modal = None;
                }

                let cancel = if matches!(modal, Question::Quit(_)) {
                    "Keep working"
                } else {
                    "Cancel"
                };
                if ui.button(cancel).clicked() {
                    self.pending_snapshot = None;
                    self.modal = None;
                }
            });
        });
    }
}

/// Draws one number in a fixed-width column, right-aligned against the column's edge.
///
/// The width has to come from a reserved rectangle rather than from a right-aligned layout: a
/// layout inside a container that has not settled its own width aligns against whatever space
/// happens to be free, which puts each row's number in a different place.
fn column(ui: &mut egui::Ui, width: f32, text: &str, colour: egui::Color32) {
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let height = ui.spacing().interact_size.y;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());

    ui.painter().text(
        rect.right_center(),
        egui::Align2::RIGHT_CENTER,
        text,
        font,
        colour,
    );
}

/// Names the two numeric columns, since a bare pair of figures does not say which is which.
fn column_headings(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = COLUMN_GAP;
        ui.add_space((ui.available_width() - NUMBER_COLUMNS).max(0.0));

        for (width, heading) in [(SIZE_WIDTH, "space"), (COUNT_WIDTH, "files")] {
            let height = ui.spacing().interact_size.y;
            let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
            ui.painter().text(
                rect.right_center(),
                egui::Align2::RIGHT_CENTER,
                heading,
                egui::TextStyle::Small.resolve(ui.style()),
                theme::MUTED,
            );
        }
    });
}

/// Paints the alternating band behind a row, across the container's full width.
fn stripe(ui: &egui::Ui, reserved: egui::layers::ShapeIdx, row: egui::Rect) {
    let pad = ui.spacing().item_spacing.y / 2.0;
    let rect = egui::Rect::from_x_y_ranges(ui.max_rect().x_range(), row.y_range().expand(pad));

    ui.painter().set(
        reserved,
        egui::Shape::rect_filled(rect, egui::CornerRadius::same(4), theme::SURFACE),
    );
}

/// One band showing the whole library, with the part a clean would take filled in.
///
/// This is the question the tool exists to answer, so it is the only loud thing on the screen.
/// The pink is the same pink as every ticked category and every total, and it always means the
/// same thing: this is leaving.
fn composition_bar(ui: &mut egui::Ui, total: u64, removing: u64) {
    let height = 26.0;
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::hover(),
    );

    let radius = egui::CornerRadius::same(3);
    ui.painter().rect_filled(rect, radius, theme::KEEP);

    #[expect(
        clippy::cast_precision_loss,
        reason = "display-only proportion of a byte count"
    )]
    let fraction = if total == 0 {
        0.0_f32
    } else {
        removing as f32 / total as f32
    };

    if fraction > 0.0 {
        // A sliver still has to be visible, or a small category looks like nothing at all.
        let width = (rect.width() * fraction).max(3.0);
        let filled = egui::Rect::from_min_size(
            egui::pos2(rect.right() - width, rect.top()),
            egui::vec2(width, height),
        );
        ui.painter().rect_filled(filled, radius, theme::REMOVE);
    }

    ui.add_space(7.0);
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(format!(
                "keeping {}",
                human_bytes(total.saturating_sub(removing))
            ))
            .small()
            .color(theme::MUTED),
        );

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let text = if removing == 0 {
                egui::RichText::new("nothing selected")
                    .small()
                    .color(theme::MUTED)
            } else {
                egui::RichText::new(format!("removing {}", human_bytes(removing)))
                    .small()
                    .color(theme::REMOVE)
            };
            ui.label(text);
        });
    });
}

fn describe_compaction(result: cleaner_core::Compaction) -> (String, egui::Color32) {
    if !result.rewritten {
        return (
            "Database is in use. Close osu!lazer and try again.".to_owned(),
            theme::REMOVE,
        );
    }
    if result.freed() == 0 {
        return ("No space to reclaim.".to_owned(), theme::MUTED);
    }
    (
        format!("Reclaimed {}.", human_bytes(result.freed())),
        theme::GOOD,
    )
}
