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
}

/// What the worker reports back.
#[derive(Debug)]
pub(crate) enum Event {
    /// A library was opened.
    LibraryOpened {
        /// Its root directory.
        root: PathBuf,
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
    /// The snapshot list was refreshed.
    Snapshots {
        /// Snapshots, oldest first.
        entries: Vec<SnapshotSummary>,
    },
    /// Something went wrong.
    Failed {
        /// Message to show.
        message: String,
    },
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

    match command {
        Command::OpenLibrary { path } => {
            let opened = match path {
                Some(path) => Library::open(&path),
                None => Library::discover(),
            };

            match opened {
                Ok(found) => {
                    report(Event::LibraryOpened {
                        root: found.root().to_path_buf(),
                    });
                    *library = Some(found);
                }
                Err(error) => report(Event::Failed {
                    message: error.to_string(),
                }),
            }
        }

        Command::Scan { selected } => {
            let Some(library) = library.as_ref() else {
                report(Event::Failed {
                    message: "no library is open".to_owned(),
                });
                return;
            };

            let result = cleaner_core::build_plan(library, &selected, |progress| {
                let _ = events.send(Event::Progress {
                    message: describe(&progress),
                });
            });

            match result {
                Ok(plan) => report(Event::Scanned {
                    plan: Box::new(plan),
                }),
                Err(error) => report(Event::Failed {
                    message: error.to_string(),
                }),
            }
        }

        Command::Clean { plan } => {
            let Some(library) = library.as_ref() else {
                return;
            };

            let options = Options { dry_run: false };
            let result = cleaner_core::run(library, &plan, &options, |progress| {
                let _ = events.send(Event::Progress {
                    message: describe_clean(progress),
                });
            });

            match result {
                Ok(outcome) => report(Event::Cleaned {
                    files: outcome.detached,
                    bytes: outcome.bytes,
                }),
                Err(error) => report(Event::Failed {
                    message: error.to_string(),
                }),
            }
        }

        Command::ListSnapshots => {
            let Some(library) = library.as_ref() else {
                return;
            };

            match snapshot::list(library) {
                Ok(entries) => report(Event::Snapshots {
                    entries: entries.iter().map(SnapshotSummary::from).collect(),
                }),
                Err(error) => report(Event::Failed {
                    message: error.to_string(),
                }),
            }
        }

        Command::RestoreSnapshot { id } => {
            let reporter = events.clone();
            act_on_snapshot(library.as_ref(), &id, events, move |library, snapshot| {
                cleaner_core::restore(library, snapshot, |progress| {
                    let _ = reporter.send(Event::Progress {
                        message: describe_clean(progress),
                    });
                })
                .map(|_| ())
            });
        }

        Command::DeleteSnapshot { id } => {
            act_on_snapshot(library.as_ref(), &id, events, snapshot::delete);
        }
    }
}

/// Applies an operation to the named snapshot, then refreshes the list.
fn act_on_snapshot(
    library: Option<&Library>,
    id: &str,
    events: &Sender<Event>,
    operation: impl FnOnce(&Library, &Snapshot) -> Result<(), cleaner_core::SnapshotError>,
) {
    let Some(library) = library else {
        return;
    };

    let entries = match snapshot::list(library) {
        Ok(entries) => entries,
        Err(error) => {
            let _ = events.send(Event::Failed {
                message: error.to_string(),
            });
            return;
        }
    };

    let Some(target) = entries.iter().find(|s| s.id() == id) else {
        let _ = events.send(Event::Failed {
            message: format!("no snapshot named '{id}'"),
        });
        return;
    };

    if let Err(error) = operation(library, target) {
        let _ = events.send(Event::Failed {
            message: error.to_string(),
        });
        return;
    }

    if let Ok(entries) = snapshot::list(library) {
        let _ = events.send(Event::Snapshots {
            entries: entries.iter().map(SnapshotSummary::from).collect(),
        });
    }
}

/// Turns clean progress into a status line.
fn describe_clean(progress: cleaner_core::CleanProgress) -> String {
    match progress {
        cleaner_core::CleanProgress::UpdatingDatabase => "Updating the database".to_owned(),
        cleaner_core::CleanProgress::Moving { done, total } => {
            format!("Moving files: {done} of {total}")
        }
        cleaner_core::CleanProgress::Reattaching { done, total } => {
            format!("Reattaching files to beatmap sets: {done} of {total}")
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
