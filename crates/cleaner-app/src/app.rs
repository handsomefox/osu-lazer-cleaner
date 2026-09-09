//! Application state and screen layout.

use crate::icons;
use crate::theme;
use crate::worker::{BackupSummary, Command, Event, SnapshotSummary, Worker};
use cleaner_core::{Category, Plan, SetEntry, Totals, human_bytes};
use eframe::egui;
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

/// How often to look for a running osu!lazer.
///
/// Enumerating processes is cheap, but not so cheap that it belongs in every frame.
const LAZER_CHECK_INTERVAL: Duration = Duration::from_secs(2);

/// How many beatmap sets the browse list shows before it asks the user to narrow the search.
const BROWSE_LIMIT: usize = 500;

/// Storage key holding the screen that was open when the window closed.
const SCREEN_KEY: &str = "screen";

/// Which screen is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    /// Categories and scan results.
    Clean,
    /// Snapshots from previous cleans.
    Snapshots,
}

impl Screen {
    /// The name this screen is stored under. Stored rather than derived, so renaming a variant
    /// cannot silently invalidate what a previous version wrote.
    fn key(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Snapshots => "snapshots",
        }
    }

    /// The screen a stored key names, if it still names one.
    fn from_key(key: &str) -> Option<Self> {
        match key {
            "clean" => Some(Self::Clean),
            "snapshots" => Some(Self::Snapshots),
            _ => None,
        }
    }
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
    /// Reading the snapshot list.
    ListingSnapshots,
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
        !matches!(self, Self::Scanning | Self::ListingSnapshots)
    }

    /// How to name it mid-sentence.
    fn describe(self) -> &'static str {
        match self {
            Self::Scanning => "scanning the library",
            Self::ListingSnapshots => "reading snapshots",
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
    /// What the current selection would free, recomputed only when it changes.
    ///
    /// A large library holds a quarter of a million candidates, and walking them is too much to
    /// do sixty times a second.
    selected_totals: Totals,
    /// What each category would free on its own, in the order the rows appear.
    category_totals: Vec<(Category, Totals)>,
    /// Category whose beatmap sets are open for browsing.
    browsing: Option<Category>,
    /// Search text for the browse list.
    search: String,
    /// Beatmap sets in the open category, largest first.
    sets: Vec<SetEntry>,
    /// Whether osu!lazer was running at the last check.
    ///
    /// Written by the watcher thread, read every frame.
    lazer_running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    snapshots: Vec<SnapshotSummary>,
    /// The copy of the database the last compaction kept.
    backup: Option<BackupSummary>,
    /// Size of `client.realm`, so the compact control can show what it is working on.
    database_bytes: u64,
    /// What the last compaction did, if one has run.
    compaction: Option<cleaner_core::Compaction>,
    modal: Option<Question>,
    /// Snapshot the open question refers to, for restore and delete.
    pending_snapshot: Option<String>,
    last_result: Option<String>,
    /// Whether the About modal is open.
    about: bool,
}

impl App {
    /// Builds the application and asks the worker to find a library.
    pub(crate) fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = &cc.egui_ctx;
        theme::apply(ctx);
        // The About window draws the icon from a PNG, which needs a loader.
        egui_extras::install_image_loaders(ctx);

        let screen = cc
            .storage
            .and_then(|storage| storage.get_string(SCREEN_KEY))
            .and_then(|key| Screen::from_key(&key))
            .unwrap_or(Screen::Clean);

        let worker = Worker::spawn(ctx.clone());
        worker.send(Command::OpenLibrary { path: None });

        Self {
            worker,
            screen,
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
            selected_totals: Totals::default(),
            category_totals: Vec::new(),
            browsing: None,
            search: String::new(),
            sets: Vec::new(),
            lazer_running: watch_for_lazer(ctx.clone()),
            backup: None,
            database_bytes: 0,
            compaction: None,
            modal: None,
            pending_snapshot: None,
            last_result: None,
            about: false,
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
                    self.plan = None;
                    self.compaction = None;
                    self.activity = Some(Activity::ListingSnapshots);
                    self.error = None;
                    self.worker.send(Command::ListSnapshots);
                }
                Event::Progress { message } => self.status = message,
                Event::Scanned { plan } => {
                    // Untick anything the scan found nothing for, so the selection describes
                    // what is actually there.
                    for group in &plan.groups {
                        if group.is_empty() {
                            self.selected.remove(&group.category);
                        }
                    }

                    self.status = format!("Scanned {} beatmap sets", plan.sets_scanned);
                    self.plan = Some(*plan);
                    self.activity = None;
                    self.browsing = None;
                    self.search.clear();
                    self.apply_selection();
                }
                Event::Cleaned { files, bytes } => {
                    self.browsing = None;
                    self.last_result = Some(format!(
                        "Moved {files} files into a snapshot. Deleting it reclaims {}.",
                        human_bytes(bytes)
                    ));
                    "Clean finished".clone_into(&mut self.status);
                    self.plan = None;
                    self.activity = Some(Activity::ListingSnapshots);
                    self.worker.send(Command::ListSnapshots);
                }
                Event::Compacted { result } => {
                    self.database_bytes = result.after;
                    "Compact finished".clone_into(&mut self.status);
                    self.compaction = Some(result);
                    self.activity = None;
                }
                Event::Snapshots { entries, backup } => {
                    self.snapshots = entries;
                    self.backup = backup;
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
        let Some(plan) = self.plan.take() else {
            return;
        };

        self.activity = Some(Activity::Cleaning);
        self.worker.send(Command::Clean {
            plan: Box::new(plan),
        });
    }

    /// Keeps the plan's selection in step with the checkboxes, then recounts.
    fn apply_selection(&mut self) {
        let Some(plan) = self.plan.as_mut() else {
            self.selected_totals = Totals::default();
            self.category_totals = Vec::new();
            self.sets = Vec::new();
            return;
        };

        for category in Category::ALL {
            plan.select(*category, self.selected.contains(category));
        }

        self.selected_totals = plan.selected_totals();
        self.category_totals = plan.totals_by_category();
        self.sets = self.browsing.map(|c| plan.sets_in(c)).unwrap_or_default();
    }

    /// Opens or closes the beatmap-set list under one category.
    fn browse(&mut self, category: Option<Category>) {
        self.browsing = if self.browsing == category {
            None
        } else {
            category
        };
        self.search.clear();
        self.apply_selection();
    }

    /// Takes one beatmap set out of the clean, or puts it back.
    fn exclude(&mut self, set: cleaner_core::SetId, excluded: bool) {
        if let Some(plan) = self.plan.as_mut() {
            plan.exclude(set, excluded);
        }
        self.apply_selection();
    }

    /// Applies the keyboard shortcuts, whatever has focus.
    ///
    /// `consume_key` takes the press, so a shortcut never also lands in a text box.
    fn shortcuts(&mut self, ctx: &egui::Context) {
        let (next, previous, clean, snapshots, rescan, escape) = ctx.input_mut(|input| {
            (
                input.consume_key(egui::Modifiers::CTRL, egui::Key::Tab),
                input.consume_key(
                    egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
                    egui::Key::Tab,
                ),
                input.consume_key(egui::Modifiers::CTRL, egui::Key::Num1),
                input.consume_key(egui::Modifiers::CTRL, egui::Key::Num2),
                input.consume_key(egui::Modifiers::NONE, egui::Key::F5),
                input.consume_key(egui::Modifiers::NONE, egui::Key::Escape),
            )
        });

        if escape {
            // Innermost first: a question, then the browse list.
            if self.modal.take().is_none() && self.browsing.is_some() {
                self.browse(None);
            }
        }

        // Everything else waits for the worker, so that a shortcut cannot start a second
        // operation on top of one already running.
        if !self.idle() {
            return;
        }

        if rescan && self.library.is_some() && self.screen == Screen::Clean {
            self.scan();
        }

        if clean {
            self.screen = Screen::Clean;
        } else if snapshots {
            self.screen = Screen::Snapshots;
        } else if next || previous {
            // There are two screens, so cycling forwards and backwards land in the same place.
            self.screen = match self.screen {
                Screen::Clean => Screen::Snapshots,
                Screen::Snapshots => Screen::Clean,
            };
        }
    }

    /// Whether osu!lazer was running when the watcher last looked.
    fn lazer_running(&self) -> bool {
        self.lazer_running
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The banner shown while osu!lazer has the library open.
    fn lazer_warning(&self, ui: &mut egui::Ui) {
        if !self.lazer_running() {
            return;
        }

        egui::Frame::new()
            .fill(theme::SURFACE)
            .stroke(egui::Stroke::new(1.0, theme::BAD))
            .corner_radius(egui::CornerRadius::same(4))
            .inner_margin(egui::Margin::symmetric(12, 9))
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(icons::labelled(icons::WARNING, "osu!lazer is running"))
                        .color(theme::BAD)
                        .strong(),
                );
                ui.label(
                    egui::RichText::new(
                        "Close it before cleaning, restoring, or compacting. The game sweeps \
                         unreferenced files on startup and holds the database open, so leaving \
                         it running makes those three fail or take longer than they should.",
                    )
                    .small()
                    .color(theme::MUTED),
                );
            });
        ui.add_space(14.0);
    }
}

impl eframe::App for App {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.absorb_events();
        let ctx = root.ctx().clone();
        self.guard_close(&ctx);
        self.shortcuts(&ctx);

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
                    ui.label(
                        egui::RichText::new(icons::labelled(icons::ERROR, &error))
                            .color(theme::BAD),
                    );
                    ui.add_space(10.0);
                }

                self.lazer_warning(ui);

                egui::ScrollArea::vertical().show(ui, |ui| match self.screen {
                    Screen::Clean => self.clean_screen(ui),
                    Screen::Snapshots => self.snapshots_screen(ui),
                });
            });

        self.show_modal(&ctx);
        crate::about::show(&ctx, &mut self.about);
    }

    /// Remembers the open screen, so the window comes back where it was left.
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        storage.set_string(SCREEN_KEY, self.screen.key().to_owned());
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
                ui.selectable_value(
                    &mut self.screen,
                    Screen::Clean,
                    icons::labelled(icons::CLEAN, "Clean"),
                )
                .on_hover_text("Ctrl+1, or Ctrl+Tab to switch");
                ui.selectable_value(
                    &mut self.screen,
                    Screen::Snapshots,
                    icons::labelled(icons::SNAPSHOTS, "Snapshots"),
                )
                .on_hover_text("Ctrl+2, or Ctrl+Tab to switch");
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button(icons::ABOUT)
                    .on_hover_text("About osu!lazer Cleaner")
                    .clicked()
                {
                    self.about = true;
                }
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
            ui.label(
                egui::RichText::new(icons::labelled(icons::SUCCESS, &result)).color(theme::GOOD),
            );
            ui.label(
                egui::RichText::new(
                    "Start osu!lazer and check your beatmaps before deleting the snapshot.",
                )
                .small()
                .color(theme::MUTED),
            );
            ui.add_space(14.0);
        }

        let Some(plan) = self.plan.take() else {
            ui.label(theme::figure("Nothing scanned yet"));
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("Scanning reads your library. It changes nothing.")
                    .color(theme::MUTED),
            );
            ui.add_space(16.0);
            ui.add_enabled_ui(self.idle() && self.library.is_some(), |ui| {
                if ui
                    .button(icons::labelled(icons::SCAN, "Scan library"))
                    .clicked()
                {
                    self.scan();
                }
            });
            return;
        };

        let removing = self.selected_totals.bytes;
        let previous_selection = self.selected.clone();

        ui.horizontal(|ui| {
            ui.label(theme::figure(human_bytes(plan.bytes_total)));
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(format!("across {} beatmap sets", plan.sets_scanned))
                    .color(theme::MUTED),
            );

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_enabled_ui(self.idle(), |ui| {
                    if ui
                        .button(icons::labelled(icons::RESCAN, "Scan again"))
                        .on_hover_text("F5")
                        .clicked()
                    {
                        self.scan();
                    }
                });
            });
        });

        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(scan_timings(&plan))
                .small()
                .color(theme::MUTED),
        );

        ui.add_space(12.0);
        composition_bar(ui, plan.bytes_total, removing);
        ui.add_space(18.0);

        self.category_table(ui, &plan);

        ui.add_space(16.0);
        ui.separator();
        ui.add_space(10.0);

        ui.horizontal(|ui| {
            let references = self.selected_totals.references;
            let enabled = self.idle() && references > 0;
            ui.add_enabled_ui(enabled, |ui| {
                let label = if references == 0 {
                    "Nothing selected".to_owned()
                } else if removing == 0 {
                    // Every selected file is shared with something that keeps it, so the clean
                    // detaches references and frees nothing.
                    "Detach selected files".to_owned()
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
        self.plan = Some(plan);
        if self.selected != previous_selection {
            self.apply_selection();
        }
    }

    /// One row per category: tick, name, what it means, how much space, how many files.
    fn category_table(&mut self, ui: &mut egui::Ui, plan: &Plan) {
        column_headings(ui);
        for (index, group) in plan.groups.iter().enumerate() {
            let totals = self
                .category_totals
                .iter()
                .find(|(category, _)| *category == group.category)
                .map_or_else(Totals::default, |(_, totals)| *totals);

            self.category_row(ui, group, totals, index % 2 == 1);

            if self.browsing == Some(group.category) {
                self.browse_list(ui, group.category);
            }
        }

        if plan.excluded.is_empty() {
            return;
        }

        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(format!(
                "{} beatmap sets are held back from every category.",
                theme::grouped(plan.excluded.len())
            ))
            .small()
            .color(theme::REMOVE),
        );
    }

    /// Draws one category, banded so the eye can carry a name across to its numbers.
    fn category_row(
        &mut self,
        ui: &mut egui::Ui,
        group: &cleaner_core::Group,
        totals: Totals,
        striped: bool,
    ) {
        // Reserved now, painted once the row's height is known, so the band sits behind the
        // text rather than over it.
        let band = ui.painter().add(egui::Shape::Noop);

        let found = !group.is_empty();
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

            column(ui, SIZE_WIDTH, &human_bytes(totals.bytes), tint);
            column(ui, COUNT_WIDTH, &theme::grouped(totals.files), tint);
        });

        if striped {
            stripe(ui, band, row.response.rect);
        }
    }

    /// The tick, the category name, the sentence explaining it, and the way into its sets.
    fn category_label(
        &mut self,
        ui: &mut egui::Ui,
        group: &cleaner_core::Group,
        found: bool,
        on: bool,
    ) {
        let category = group.category;

        ui.horizontal(|ui| {
            ui.add_enabled_ui(found && self.idle(), |ui| {
                let mut ticked = on;
                let label = egui::RichText::new(icons::labelled(
                    icons::category(category),
                    category.label(),
                ))
                .color(if on && found {
                    theme::REMOVE
                } else {
                    theme::TEXT
                });

                if ui.checkbox(&mut ticked, label).changed() {
                    if ticked {
                        self.selected.insert(category);
                    } else {
                        self.selected.remove(&category);
                    }
                    self.apply_selection();
                }
            });

            // Unreferenced files belong to no beatmap set, so there is nothing to list.
            if !found || category == Category::Unreferenced {
                return;
            }

            ui.add_enabled_ui(self.idle(), |ui| {
                let open = self.browsing == Some(category);
                let label = if open { "Hide sets" } else { "Browse sets" };
                if ui
                    .small_button(label)
                    .on_hover_text("Pick which beatmap sets this category cleans")
                    .clicked()
                {
                    self.browse(Some(category));
                }
            });
        });

        ui.label(
            egui::RichText::new(category.description())
                .small()
                .color(theme::MUTED),
        );
    }

    /// Every beatmap set in one category, with a tick that holds one back.
    ///
    /// A category can hold well over a hundred thousand references across tens of thousands of
    /// sets, so the list is searchable and shows the largest [`BROWSE_LIMIT`] matches. Sorting
    /// by size puts the sets worth deciding about at the top, where a limit cannot hide them.
    fn browse_list(&mut self, ui: &mut egui::Ui, category: Category) {
        egui::Frame::new()
            .fill(theme::SURFACE)
            .corner_radius(egui::CornerRadius::same(4))
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
                let matches: Vec<usize> = matching(&self.sets, &self.search);

                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Find").small().color(theme::MUTED));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.search)
                            .hint_text("artist or title")
                            .desired_width(260.0),
                    );

                    ui.label(
                        egui::RichText::new(format!(
                            "{} of {} beatmap sets",
                            theme::grouped(matches.len()),
                            theme::grouped(self.sets.len())
                        ))
                        .small()
                        .color(theme::MUTED),
                    );

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_enabled_ui(self.idle() && !matches.is_empty(), |ui| {
                            if ui.small_button("Keep all shown").clicked() {
                                self.set_all(&matches, true);
                            }
                            if ui.small_button("Clean all shown").clicked() {
                                self.set_all(&matches, false);
                            }
                        });
                    });
                });

                ui.add_space(8.0);

                if self.sets.is_empty() {
                    ui.label(
                        egui::RichText::new("No beatmap set contributes to this category.")
                            .small()
                            .color(theme::MUTED),
                    );
                    return;
                }

                if matches.is_empty() {
                    ui.label(
                        egui::RichText::new("Nothing matches that search.")
                            .small()
                            .color(theme::MUTED),
                    );
                    return;
                }

                let shown = matches.len().min(BROWSE_LIMIT);
                let row_height = ui.spacing().interact_size.y;

                egui::ScrollArea::vertical()
                    .id_salt(("browse", category))
                    .max_height(320.0)
                    .auto_shrink([false, true])
                    .show_rows(ui, row_height, shown, |ui, range| {
                        for position in range {
                            self.browse_row(ui, matches[position], position % 2 == 1);
                        }
                    });

                if matches.len() > shown {
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "Showing the {} largest. Search to reach the rest.",
                            theme::grouped(shown)
                        ))
                        .small()
                        .color(theme::MUTED),
                    );
                }
            });

        ui.add_space(10.0);
    }

    /// One beatmap set: whether it is being cleaned, its name, and what it holds.
    fn browse_row(&mut self, ui: &mut egui::Ui, index: usize, striped: bool) {
        let Some(entry) = self.sets.get(index).cloned() else {
            return;
        };

        let band = ui.painter().add(egui::Shape::Noop);
        let tint = if entry.excluded {
            theme::MUTED
        } else {
            theme::REMOVE
        };

        let row = ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = COLUMN_GAP;
            // Every row has to be exactly one line high: the list is drawn by index, from a
            // single row height, so a title that wrapped would displace everything below it.
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                column(ui, COUNT_WIDTH, &theme::grouped(entry.files), tint);
                column(ui, SIZE_WIDTH, &human_bytes(entry.bytes), tint);

                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add_enabled_ui(self.idle(), |ui| {
                        let mut cleaning = !entry.excluded;
                        let label = egui::RichText::new(&entry.title).color(if entry.excluded {
                            theme::MUTED
                        } else {
                            theme::TEXT
                        });

                        if ui
                            .checkbox(&mut cleaning, label)
                            .on_hover_text(format!(
                                "{}\n\nUntick to leave this beatmap set alone",
                                entry.title
                            ))
                            .changed()
                        {
                            self.exclude(entry.set, !cleaning);
                        }
                    });
                });
            });
        });

        if striped {
            stripe(ui, band, row.response.rect);
        }
    }

    /// Includes or excludes every set the search currently shows.
    fn set_all(&mut self, matches: &[usize], keep: bool) {
        let sets: Vec<_> = matches
            .iter()
            .filter_map(|index| self.sets.get(*index))
            .map(|entry| entry.set)
            .collect();

        if let Some(plan) = self.plan.as_mut() {
            for set in sets {
                plan.exclude(set, keep);
            }
        }
        self.apply_selection();
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
                    if ui
                        .button(icons::labelled(icons::DELETE, "Delete"))
                        .clicked()
                    {
                        self.pending_snapshot = Some(entry.id.clone());
                        self.modal = Some(Question::Delete);
                    }
                });

                ui.add_enabled_ui(newest && self.idle(), |ui| {
                    let button = ui.button(icons::labelled(icons::RESTORE, "Put files back"));
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
                if ui
                    .button(icons::labelled(icons::COMPACT, "Compact database"))
                    .clicked()
                {
                    self.compaction = None;
                    self.error = None;
                    self.activity = Some(Activity::Compacting);
                    self.worker.send(Command::Compact);
                }
            });

            if let Some(result) = self.compaction.clone() {
                let (text, colour) = describe_compaction(&result);
                ui.label(egui::RichText::new(text).small().color(colour));
            }
        });

        let Some(backup) = self.backup.clone() else {
            return;
        };

        ui.add_space(14.0);
        ui.label(
            egui::RichText::new(format!(
                "A copy of client.realm from before the last compaction is kept at {}, holding \
                 {}. Put it back by renaming it over client.realm with osu!lazer closed.",
                backup.path.display(),
                human_bytes(backup.bytes)
            ))
            .small()
            .color(theme::MUTED),
        );
        ui.add_space(8.0);

        ui.add_enabled_ui(self.idle(), |ui| {
            if ui
                .button(icons::labelled(icons::DELETE, "Delete the copy"))
                .on_hover_text("Do this once osu!lazer has opened your library without complaint")
                .clicked()
            {
                self.error = None;
                self.activity = Some(Activity::ListingSnapshots);
                self.worker.send(Command::RemoveDatabaseBackup);
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
                let totals = self.selected_totals;
                let held_back = self.plan.as_ref().map_or(0, |plan| plan.excluded.len());
                let mut body = format!(
                    "{} files, {}.",
                    theme::grouped(totals.files),
                    human_bytes(totals.bytes)
                );

                if held_back > 0 {
                    use std::fmt::Write as _;
                    let _ = write!(
                        body,
                        " {} beatmap sets you held back are left alone.",
                        theme::grouped(held_back)
                    );
                }

                body.push_str(
                    "\n\nYour free space will not change yet. Start osu!lazer, check your \
                     beatmaps, then delete the snapshot to reclaim the space, or put the files \
                     back if something is wrong.",
                );

                if self.lazer_running() {
                    body.push_str("\n\nosu!lazer is running. Close it first.");
                }

                (
                    "Move these files to a snapshot?",
                    body,
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

/// Starts the thread that watches for osu!lazer, and returns what it writes to.
///
/// Enumerating processes takes tens of milliseconds on Windows, which is a visible hitch if it
/// happens on the interface thread. It cannot go on the worker either: a clean holds that for
/// minutes, and the answer would freeze exactly while it matters.
fn watch_for_lazer(ctx: egui::Context) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = std::sync::Arc::clone(&running);

    let started = std::thread::Builder::new()
        .name("lazer-watch".to_owned())
        .spawn(move || {
            loop {
                let found = cleaner_core::lazer_is_running();
                if writer.swap(found, std::sync::atomic::Ordering::Relaxed) != found {
                    ctx.request_repaint();
                }
                std::thread::sleep(LAZER_CHECK_INTERVAL);
            }
        });

    if let Err(error) = started {
        // Without the watcher the window simply never shows the banner, which is what it did
        // before there was one.
        tracing::warn!(%error, "could not start the osu!lazer watcher");
    }

    running
}

/// How long a scan took, and where the time went.
fn scan_timings(plan: &Plan) -> String {
    let timings = plan.timings;
    format!(
        "Scanned in {}: {} measuring files, {} reading the database, {} reading beatmaps. \
         Opened difficulty files for {} of {} beatmap sets.",
        seconds(timings.total_ms()),
        seconds(timings.measure_ms),
        seconds(timings.database_ms),
        seconds(timings.classify_ms),
        theme::grouped(timings.sets_parsed),
        theme::grouped(plan.sets_scanned)
    )
}

/// Formats milliseconds as seconds with one decimal.
fn seconds(ms: u64) -> String {
    #[expect(
        clippy::cast_precision_loss,
        reason = "display-only approximation of a duration"
    )]
    let value = ms as f64 / 1000.0;
    format!("{value:.1}s")
}

/// Positions of the beatmap sets whose title contains `search`, ignoring case.
fn matching(sets: &[SetEntry], search: &str) -> Vec<usize> {
    let needle = search.trim().to_lowercase();
    if needle.is_empty() {
        return (0..sets.len()).collect();
    }

    sets.iter()
        .enumerate()
        .filter(|(_, entry)| entry.title.to_lowercase().contains(&needle))
        .map(|(index, _)| index)
        .collect()
}

fn describe_compaction(result: &cleaner_core::Compaction) -> (String, egui::Color32) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(title: &str) -> SetEntry {
        SetEntry {
            set: [0; 16],
            title: title.to_owned(),
            files: 1,
            bytes: 1,
            excluded: false,
        }
    }

    #[test]
    fn searching_ignores_case_and_surrounding_space() {
        let sets = [entry("Sotarks - Nhelv"), entry("Camellia - Ghost")];

        assert_eq!(matching(&sets, "  SOTARKS "), vec![0]);
        assert_eq!(matching(&sets, "e"), vec![0, 1]);
        assert_eq!(matching(&sets, ""), vec![0, 1]);
        assert!(matching(&sets, "nothing here").is_empty());
    }
}
