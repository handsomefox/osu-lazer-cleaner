//! The background thread that does every slow or destructive operation.
//!
//! The interface thread never touches the library. It sends a [`Command`] and redraws when an
//! [`Event`] arrives, which keeps a scan of several hundred thousand files from freezing the
//! window.

use cleaner_core::{Library, Options, Plan, Snapshot, snapshot};
use crossbeam_channel::{Receiver, Sender, unbounded};
use eframe::egui;
use std::collections::HashSet;
use std::path::PathBuf;

/// Work the interface asks for.
#[derive(Debug)]
pub(crate) enum Command {
    /// Find a library, either at `path` or in the default locations.
    OpenLibrary {
        /// Explicit directory, or `None` to search.
        path: Option<PathBuf>,
    },
    /// Scan the open library.
    Scan {
        /// Categories that start selected.
        selected: HashSet<cleaner_core::Category>,
    },
    /// Remove the selected categories.
    Clean {
        /// The plan to act on.
        plan: Box<Plan>,
    },
    /// Re-read the snapshot list.
    ListSnapshots,
    /// Move a snapshot's files back.
    RestoreSnapshot {
        /// Which snapshot.
        id: String,
    },
    /// Delete a snapshot permanently.
    DeleteSnapshot {
        /// Which snapshot.
        id: String,
    },
    /// Rewrite the database without its free space.
    Compact,
    /// Delete one of the copies of the database a compaction kept.
    RemoveDatabaseBackup {
        /// File name of the copy, as `BackupSummary` gives it.
        name: String,
    },
}

impl Command {
    /// A short name for the log. The command itself is too large to print.
    fn name(&self) -> &'static str {
        match self {
            Self::OpenLibrary { .. } => "open library",
            Self::Scan { .. } => "scan",
            Self::Clean { .. } => "clean",
            Self::ListSnapshots => "list snapshots",
            Self::RestoreSnapshot { .. } => "restore snapshot",
            Self::DeleteSnapshot { .. } => "delete snapshot",
            Self::Compact => "compact",
            Self::RemoveDatabaseBackup { .. } => "remove database backup",
        }
    }
}

/// What the worker reports back.
#[derive(Debug)]
pub(crate) enum Event {
    /// A library was opened.
    LibraryOpened {
        /// Its root directory.
        root: PathBuf,
        /// How big `client.realm` is, so the window can show it without a scan.
        database_bytes: u64,
    },
    /// Progress during a scan.
    Progress {
        /// Message to show.
        message: String,
    },
    /// A scan finished.
    Scanned {
        /// What it found.
        plan: Box<Plan>,
    },
    /// A clean finished.
    Cleaned {
        /// Files detached.
        files: usize,
        /// Bytes moved into the snapshot.
        bytes: u64,
    },
    /// A compaction finished, whether or not it changed anything.
    Compacted {
        /// What it did.
        result: cleaner_core::Compaction,
    },
    /// The snapshot list was refreshed.
    Snapshots {
        /// Snapshots, newest first.
        entries: Vec<SnapshotSummary>,
        /// The copies of the database compactions kept, newest first.
        backups: Vec<BackupSummary>,
    },
    /// A snapshot operation finished.
    ///
    /// Restores and deletions report nothing of their own beyond a refreshed snapshot list, so
    /// without this the status line kept the last progress message it was sent and read as
    /// though the operation had stalled just short of the end.
    Finished {
        /// What to put in the status line.
        message: &'static str,
        /// Whether the library now holds files the last scan did not see.
        library_changed: bool,
    },
    /// Something went wrong.
    Failed {
        /// Message to show.
        message: String,
    },
}

/// A copy of `client.realm` a compaction kept.
#[derive(Debug, Clone)]
pub(crate) struct BackupSummary {
    /// File name, used to address it.
    pub(crate) name: String,
    /// Where it is, for the window to show.
    pub(crate) path: PathBuf,
    /// How many bytes deleting it would reclaim.
    pub(crate) bytes: u64,
    /// When it was taken.
    pub(crate) taken: String,
}

/// A snapshot, reduced to what the interface displays.
#[derive(Debug, Clone)]
pub(crate) struct SnapshotSummary {
    /// Directory name, used to address it.
    pub(crate) id: String,
    /// When it was taken.
    pub(crate) created: String,
    /// How many files it holds.
    pub(crate) files: usize,
    /// How many bytes deleting it would reclaim.
    pub(crate) bytes: u64,
}

impl From<&Snapshot> for SnapshotSummary {
    fn from(snapshot: &Snapshot) -> Self {
        Self {
            id: snapshot.id(),
            created: format_taken(snapshot.manifest.created),
            files: snapshot.manifest.blobs.len(),
            bytes: snapshot.manifest.bytes(),
        }
    }
}

/// Formats when a snapshot was taken, in the reader's own time zone.
///
/// The manifest stores UTC, which displayed raw reads as `2026-09-07T21:29:16.7635384Z`.
/// Nobody needs sub-second precision to recognise which clean they are looking at.
fn format_taken(created: jiff::Timestamp) -> String {
    created
        .to_zoned(jiff::tz::TimeZone::system())
        .strftime("%Y-%m-%d %H:%M")
        .to_string()
}

/// Handle to the worker thread.
pub(crate) struct Worker {
    commands: Sender<Command>,
    events: Receiver<Event>,
}

impl Worker {
    /// Starts the worker, which repaints `ctx` whenever it reports something.
    #[must_use]
    pub(crate) fn spawn(ctx: egui::Context) -> Self {
        let (command_tx, command_rx) = unbounded();
        let (event_tx, event_rx) = unbounded();

        std::thread::Builder::new()
            .name("cleaner-worker".to_owned())
            .spawn(move || run(&command_rx, &event_tx, &ctx))
            .expect("failed to start the worker thread");

        Self {
            commands: command_tx,
            events: event_rx,
        }
    }

    /// Queues a command. Dropping the worker makes this a no-op.
    pub(crate) fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    /// Takes any events that have arrived.
    pub(crate) fn drain(&self) -> Vec<Event> {
        self.events.try_iter().collect()
    }
}

/// Worker loop. Owns the library so the interface thread cannot touch it.
fn run(commands: &Receiver<Command>, events: &Sender<Event>, ctx: &egui::Context) {
    let mut library: Option<Library> = None;

    while let Ok(command) = commands.recv() {
        handle(&mut library, command, events);
        ctx.request_repaint();
    }
}

/// Runs one command, reporting the outcome.
fn handle(library: &mut Option<Library>, command: Command, events: &Sender<Event>) {
    let report = |event| {
        let _ = events.send(event);
    };

    // The name only: a Clean carries the whole plan, which is megabytes of Debug output.
    tracing::info!(command = command.name(), "handling a command");

    match command {
        Command::OpenLibrary { path } => open(library, path.as_deref(), events),

        Command::Scan { selected } => match library.as_ref() {
            Some(library) => report(scan(library, &selected, events)),
            None => report(failed_message("no library is open".to_owned())),
        },

        Command::Clean { plan } => {
            if let Some(library) = library.as_ref() {
                report(clean(library, &plan, events));
                // A clean that fails after its commit still leaves a snapshot, and the window
                // has to offer it rather than showing only the error.
                report(snapshot_event(library));
            }
        }

        Command::ListSnapshots => {
            if let Some(library) = library.as_ref() {
                report(snapshot_event(library));
            }
        }

        Command::RestoreSnapshot { id } => {
            let reporter = events.clone();
            let done = Event::Finished {
                message: "Restore finished",
                library_changed: true,
            };
            act_on_snapshot(
                library.as_ref(),
                &id,
                events,
                done,
                move |library, snapshot| {
                    cleaner_core::restore(library, snapshot, |progress| {
                        let _ = reporter.send(Event::Progress {
                            message: describe_clean(progress),
                        });
                    })
                    .map(|_| ())
                },
            );
        }

        Command::DeleteSnapshot { id } => {
            let done = Event::Finished {
                message: "Snapshot deleted",
                library_changed: false,
            };
            act_on_snapshot(library.as_ref(), &id, events, done, snapshot::delete);
        }

        Command::Compact => {
            let Some(library) = library.as_ref() else {
                return;
            };

            report(match cleaner_core::compact(library) {
                Ok(result) => Event::Compacted { result },
                Err(error) => failed(&error),
            });
            report(snapshot_event(library));
        }

        Command::RemoveDatabaseBackup { name } => {
            let Some(library) = library.as_ref() else {
                return;
            };

            match cleaner_core::remove_database_backup(library, &name) {
                Ok(()) => {
                    report(Event::Finished {
                        message: "Copy deleted",
                        library_changed: false,
                    });
                    report(snapshot_event(library));
                }
                Err(error) => report(failed(&error)),
            }
        }
    }
}

/// Finds a library and keeps it for every command that follows.
fn open(library: &mut Option<Library>, path: Option<&std::path::Path>, events: &Sender<Event>) {
    let opened = match path {
        Some(path) => Library::open(path),
        None => Library::discover(),
    };

    match opened {
        Ok(found) => {
            let _ = events.send(Event::LibraryOpened {
                root: found.root().to_path_buf(),
                database_bytes: database_bytes(&found),
            });
            *library = Some(found);
        }
        Err(error) => {
            let _ = events.send(failed(&error));
        }
    }
}

/// Scans the library, reporting progress as it goes.
fn scan(
    library: &Library,
    selected: &HashSet<cleaner_core::Category>,
    events: &Sender<Event>,
) -> Event {
    let result = cleaner_core::build_plan(library, selected, |progress| {
        let _ = events.send(Event::Progress {
            message: describe(&progress),
        });
    });

    match result {
        Ok(plan) => Event::Scanned {
            plan: Box::new(plan),
        },
        Err(error) => failed(&error),
    }
}

/// Runs the clean the interface confirmed.
fn clean(library: &Library, plan: &Plan, events: &Sender<Event>) -> Event {
    let options = Options { dry_run: false };
    let result = cleaner_core::run(library, plan, &options, |progress| {
        let _ = events.send(Event::Progress {
            message: describe_clean(progress),
        });
    });

    match result {
        Ok(outcome) => Event::Cleaned {
            files: outcome.detached,
            bytes: outcome.bytes,
        },
        Err(error) => failed(&error),
    }
}

/// Measures `client.realm`, or reports zero if it cannot be measured.
///
/// A size that cannot be read is worth showing as absent rather than failing to open a library
/// over it: everything else here works without it.
fn database_bytes(library: &Library) -> u64 {
    std::fs::metadata(library.database()).map_or(0, |meta| meta.len())
}

/// Applies an operation to the named snapshot, then refreshes the list.
fn act_on_snapshot(
    library: Option<&Library>,
    id: &str,
    events: &Sender<Event>,
    done: Event,
    operation: impl FnOnce(&Library, &Snapshot) -> Result<(), cleaner_core::SnapshotError>,
) {
    let Some(library) = library else {
        return;
    };

    let entries = match snapshot::list(library) {
        Ok(entries) => entries,
        Err(error) => {
            let _ = events.send(failed(&error));
            return;
        }
    };

    let Some(target) = entries.iter().find(|s| s.id() == id) else {
        let _ = events.send(failed_message(format!("no snapshot named '{id}'")));
        return;
    };

    if let Err(error) = operation(library, target) {
        let _ = events.send(failed(&error));
        return;
    }

    let _ = events.send(done);
    let _ = events.send(snapshot_event(library));
}

fn snapshot_event(library: &Library) -> Event {
    match snapshot::list(library) {
        Ok(entries) => Event::Snapshots {
            entries: entries.iter().map(SnapshotSummary::from).collect(),
            backups: cleaner_core::database_backups(library)
                .into_iter()
                .map(|copy| BackupSummary {
                    name: copy.name,
                    path: copy.path,
                    bytes: copy.bytes,
                    taken: format_taken(copy.taken),
                })
                .collect(),
        },
        Err(error) => failed(&error),
    }
}

/// Turns clean progress into a status line.
fn describe_clean(progress: cleaner_core::CleanProgress) -> String {
    match progress {
        cleaner_core::CleanProgress::UpdatingDatabase => "Updating the database".to_owned(),
        cleaner_core::CleanProgress::Preserving { done, total } => {
            format!("Saving files into the snapshot: {done} of {total}")
        }
        cleaner_core::CleanProgress::Removing { done, total } => {
            format!("Removing files from the library: {done} of {total}")
        }
        cleaner_core::CleanProgress::Reattaching { done, total } => {
            format!("Reattaching files to beatmap sets: {done} of {total}")
        }
        cleaner_core::CleanProgress::Restoring { done, total } => {
            format!("Moving files back: {done} of {total}")
        }
    }
}

/// Turns scan progress into a status line.
fn describe(progress: &cleaner_core::Progress) -> String {
    match progress {
        cleaner_core::Progress::MeasuringFiles { done } => {
            format!("Measuring files: {done}")
        }
        cleaner_core::Progress::ReadingSets { done, total } => {
            format!("Reading beatmap sets: {done} of {total}")
        }
        cleaner_core::Progress::Done => "Scan complete".to_owned(),
    }
}

/// Reports a failure to the window, and the whole error chain to the log.
///
/// `Display` on the outermost error is what the window shows; the chain behind it is what makes
/// a log worth attaching to an issue.
fn failed(error: &dyn std::error::Error) -> Event {
    tracing::error!("{}", cleaner_core::error::chain(error));
    Event::Failed {
        message: error.to_string(),
    }
}

/// A failure with no error value behind it.
fn failed_message(message: String) -> Event {
    tracing::error!("{message}");
    Event::Failed { message }
}
