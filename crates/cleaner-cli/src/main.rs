//! Command-line interface for osu-lazer-cleaner.
//!
//! Everything the desktop application does is available here, which makes the tool scriptable
//! and lets a scan be checked against a real library without a graphical session.

#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a command-line tool reports results on stdout and progress on stderr"
)]

use clap::{Parser, Subcommand};
use cleaner_core::{Category, Library, Options, Plan, human_bytes, snapshot};
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::ExitCode;

/// Removes unwanted beatmap content from an osu!lazer library.
#[derive(Debug, Parser)]
#[command(name = "osu-lazer-cleaner-cli", version, about)]
struct Cli {
    /// Path to the osu!lazer data directory. Found automatically when omitted.
    #[arg(long, global = true)]
    library: Option<PathBuf>,

    /// Print results as JSON.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Report what could be removed, without changing anything.
    Scan {
        /// Print per-category detail, timings, and example filenames.
        #[arg(long, short)]
        verbose: bool,

        /// Also write the full diagnostic report to this file, as JSON.
        #[arg(long, value_name = "FILE")]
        dump: Option<PathBuf>,
    },

    /// List the cleaning categories.
    Categories,

    /// Remove the given categories, keeping a snapshot so the change can be undone.
    Clean {
        /// Categories to remove, as reported by `categories`.
        #[arg(required = true, value_parser = parse_category)]
        categories: Vec<Category>,

        /// Perform the removal. Without this, the command only reports what it would do.
        #[arg(long)]
        confirm: bool,
    },

    /// Shrink the database file by rewriting it without its free space.
    Compact,

    /// Work with snapshots taken by previous cleans.
    #[command(subcommand)]
    Snapshot(SnapshotCommand),
}

#[derive(Debug, Subcommand)]
enum SnapshotCommand {
    /// List snapshots.
    List,

    /// Move a snapshot's files back into the library.
    Restore {
        /// Snapshot identifier, as reported by `snapshot list`.
        id: String,
    },

    /// Delete a snapshot permanently, reclaiming its space.
    Delete {
        /// Snapshot identifier, as reported by `snapshot list`.
        id: String,

        /// Confirm the deletion. Without this, the command only reports what it would do.
        #[arg(long)]
        confirm: bool,
    },
}

/// Parses a category slug for clap.
fn parse_category(value: &str) -> Result<Category, String> {
    Category::from_slug(value).ok_or_else(|| {
        let known: Vec<_> = Category::ALL.iter().map(|c| c.slug()).collect();
        format!(
            "unknown category '{value}'; expected one of: {}",
            known.join(", ")
        )
    })
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{error}");
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Dispatches the requested command.
fn run(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    match &cli.command {
        Command::Categories => {
            list_categories(cli.json);
            Ok(())
        }
        Command::Scan { verbose, dump } => {
            let library = open_library(cli.library.as_deref())?;
            let plan = scan(&library, &HashSet::new())?;
            let report =
                cleaner_core::report::summarise(&plan, &library.root().display().to_string());

            if let Some(path) = dump {
                let text = serde_json::to_string_pretty(&report)?;
                std::fs::write(path, text)?;
                println!("wrote the diagnostic report to {}", path.display());
            }

            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else if *verbose {
                print!("{}", cleaner_core::report::render(&report));
            } else {
                report_scan(&library, &plan, false);
            }

            Ok(())
        }
        Command::Clean {
            categories,
            confirm,
        } => clean(cli, categories, *confirm),
        Command::Compact => compact(cli),
        Command::Snapshot(command) => snapshots(cli, command),
    }
}

/// Opens the library at `path`, or finds one.
fn open_library(path: Option<&std::path::Path>) -> Result<Library, cleaner_core::StorageError> {
    match path {
        Some(path) => Library::open(path),
        None => Library::discover(),
    }
}

/// Runs a scan, reporting progress on stderr so stdout stays machine-readable.
fn scan(library: &Library, selected: &HashSet<Category>) -> Result<Plan, cleaner_core::ScanError> {
    cleaner_core::build_plan(library, selected, |progress| match progress {
        cleaner_core::Progress::MeasuringFiles { done } => {
            eprint!("\rmeasuring files: {done}");
        }
        cleaner_core::Progress::ReadingSets { done, total } => {
            eprint!("\rreading beatmap sets: {done}/{total}");
        }
        cleaner_core::Progress::Done => eprintln!("\rscan complete                    "),
    })
}

/// Prints the categories and what each one removes.
fn list_categories(json: bool) {
    if json {
        let entries: Vec<_> = Category::ALL
            .iter()
            .map(|c| {
                serde_json::json!({
                    "slug": c.slug(),
                    "label": c.label(),
                    "description": c.description(),
                    "default_selected": c.default_selected(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&entries).unwrap_or_default()
        );
        return;
    }

    for category in Category::ALL {
        let mark = if category.default_selected() {
            "*"
        } else {
            " "
        };
        println!("{mark} {:<16} {}", category.slug(), category.description());
    }
    println!("\n* selected by default");
}

/// Prints what a scan found.
fn report_scan(library: &Library, plan: &Plan, json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(plan).unwrap_or_default());
        return;
    }

    println!("library: {}", library.root().display());
    println!(
        "{} beatmap sets, {} files, {}\n",
        plan.sets_scanned,
        plan.blobs_total,
        human_bytes(plan.bytes_total)
    );

    println!("{:<16} {:>9} {:>12}", "category", "files", "reclaimable");
    for group in &plan.groups {
        println!(
            "{:<16} {:>9} {:>12}",
            group.category.slug(),
            group.files,
            human_bytes(group.bytes)
        );
    }

    println!(
        "\nscan took {:.1}s: {:.1}s measuring files, {:.1}s reading the database, \
         {:.1}s reading beatmaps",
        f64::from(
            u32::try_from(
                plan.timings.measure_ms + plan.timings.database_ms + plan.timings.classify_ms
            )
            .unwrap_or(u32::MAX)
        ) / 1000.0,
        f64::from(u32::try_from(plan.timings.measure_ms).unwrap_or(u32::MAX)) / 1000.0,
        f64::from(u32::try_from(plan.timings.database_ms).unwrap_or(u32::MAX)) / 1000.0,
        f64::from(u32::try_from(plan.timings.classify_ms).unwrap_or(u32::MAX)) / 1000.0,
    );
    println!("\nRemove a category with: osu-lazer-cleaner-cli clean <category> --confirm");
}

/// Removes the selected categories.
fn clean(
    cli: &Cli,
    categories: &[Category],
    confirm: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let library = open_library(cli.library.as_deref())?;
    let selected: HashSet<Category> = categories.iter().copied().collect();
    let plan = scan(&library, &selected)?;

    let options = Options { dry_run: !confirm };
    let outcome = cleaner_core::run(&library, &plan, &options, |progress| match progress {
        cleaner_core::CleanProgress::UpdatingDatabase => eprint!("\rupdating the database"),
        cleaner_core::CleanProgress::Moving { done, total } => {
            eprint!("\rmoving files into the snapshot: {done}/{total}");
        }
    })?;
    eprintln!("\r                                                   ");

    if cli.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "dry_run": outcome.dry_run,
                "files": plan.selected_files(),
                "references": plan.selected_references(),
                "bytes": outcome.bytes,
                "detached": outcome.detached,
                "stashed": outcome.stashed,
                "snapshot": outcome.snapshot,
            }))
            .unwrap_or_default()
        );
        return Ok(());
    }

    if outcome.dry_run {
        println!(
            "would remove {} files ({} references) and reclaim {}",
            plan.selected_files(),
            plan.selected_references(),
            human_bytes(outcome.bytes)
        );
        println!("re-run with --confirm to do it");
        return Ok(());
    }

    println!(
        "removed {} files and moved {} into a snapshot",
        outcome.detached,
        human_bytes(outcome.bytes)
    );

    if let Some(path) = &outcome.snapshot {
        let id = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        println!("\nNothing is deleted yet. Start osu!lazer and check your beatmaps.");
        println!("To reclaim the space: osu-lazer-cleaner-cli snapshot delete {id} --confirm");
        println!("To undo instead:      osu-lazer-cleaner-cli snapshot restore {id}");
    }

    Ok(())
}

/// Rewrites the database without its free space.
fn compact(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let library = open_library(cli.library.as_deref())?;
    let (before, after) = cleaner_core::compact(&library)?;

    if after < before {
        println!(
            "database went from {} to {}, freeing {}",
            human_bytes(before),
            human_bytes(after),
            human_bytes(before - after)
        );
    } else {
        println!(
            "database is already compact at {}; nothing to reclaim",
            human_bytes(before)
        );
    }

    Ok(())
}

/// Handles the snapshot subcommands.
fn snapshots(cli: &Cli, command: &SnapshotCommand) -> Result<(), Box<dyn std::error::Error>> {
    let library = open_library(cli.library.as_deref())?;
    let available = snapshot::list(&library)?;

    match command {
        SnapshotCommand::List => {
            if cli.json {
                let entries: Vec<_> = available
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "id": s.id(),
                            "created": s.manifest.created.to_string(),
                            "files": s.manifest.blobs.len(),
                            "bytes": s.manifest.bytes(),
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&entries).unwrap_or_default()
                );
                return Ok(());
            }

            if available.is_empty() {
                println!("no snapshots");
                return Ok(());
            }

            println!("Newest first. Restore in this order.\n");
            println!("{:<24} {:>8} {:>12}  created", "id", "files", "size");
            for entry in &available {
                println!(
                    "{:<24} {:>8} {:>12}  {}",
                    entry.id(),
                    entry.manifest.blobs.len(),
                    human_bytes(entry.manifest.bytes()),
                    entry
                        .manifest
                        .created
                        .to_zoned(jiff::tz::TimeZone::system())
                        .strftime("%Y-%m-%d %H:%M")
                );
            }
            Ok(())
        }

        SnapshotCommand::Restore { id } => {
            let entry = find(&available, id)?;

            // Snapshots are listed newest first, and each was taken against the library as the
            // one before it left it. Restoring an older one on its own would leave the database
            // pointing at files a newer snapshot still holds.
            if available.first().map(cleaner_core::Snapshot::id).as_deref() != Some(id) {
                return Err(format!(
                    "restore snapshots newest first; start with {}",
                    available
                        .first()
                        .map(cleaner_core::Snapshot::id)
                        .unwrap_or_default()
                )
                .into());
            }
            let restored = cleaner_core::restore(&library, entry, |progress| match progress {
                cleaner_core::CleanProgress::UpdatingDatabase => {
                    eprint!("\rupdating the database");
                }
                cleaner_core::CleanProgress::Moving { done, total } => {
                    eprint!("\rmoving files back: {done}/{total}");
                }
            })?;
            eprintln!("\r                                             ");
            println!("restored {restored} files into the library");
            println!("snapshot {id} is gone; its files are back where they were");
            Ok(())
        }

        SnapshotCommand::Delete { id, confirm } => {
            let entry = find(&available, id)?;

            if !confirm {
                println!(
                    "would delete snapshot {} and reclaim {}",
                    entry.id(),
                    human_bytes(entry.manifest.bytes())
                );
                println!("re-run with --confirm to do it; this cannot be undone");
                return Ok(());
            }

            let reclaimed = entry.manifest.bytes();
            snapshot::delete(&library, entry)?;
            println!(
                "deleted snapshot {id} and reclaimed {}",
                human_bytes(reclaimed)
            );
            Ok(())
        }
    }
}

/// Finds a snapshot by identifier.
fn find<'a>(
    snapshots: &'a [cleaner_core::Snapshot],
    id: &str,
) -> Result<&'a cleaner_core::Snapshot, String> {
    snapshots
        .iter()
        .find(|s| s.id() == id)
        .ok_or_else(|| format!("no snapshot named '{id}'"))
}
