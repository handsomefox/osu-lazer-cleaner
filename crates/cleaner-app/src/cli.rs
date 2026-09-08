//! Command-line interface for osu-lazer-cleaner.
//!
//! Everything the window does is available here. The same executable runs both: arguments pick
//! this interface, and no arguments open the window.

#![expect(
    clippy::print_stderr,
    reason = "a command-line tool reports progress on stderr; results go to the writer `run` takes"
)]

use clap::{Parser, Subcommand};
use cleaner_core::{Category, Library, Options, Plan, human_bytes, snapshot};
use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

/// Removes unwanted beatmap content from an osu!lazer library.
#[derive(Debug, Parser)]
#[command(name = "osu-lazer-cleaner", version, about, after_help = NO_ARGUMENTS)]
pub(crate) struct Cli {
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

        /// Confirm the restore. Without this, the command only reports what it would do.
        #[arg(long)]
        confirm: bool,
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

/// Shown at the end of `--help`, where someone looking for the window will find it.
const NO_ARGUMENTS: &str = "Run osu-lazer-cleaner with no arguments to open the window.";

/// Runs the command-line interface and returns the process exit code.
pub(crate) fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    match run(&cli, &mut std::io::stdout().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if cli.json {
                eprintln!("{}", serde_json::json!({"error": error.to_string()}));
            } else {
                eprintln!("error: {error}");
            }
            ExitCode::FAILURE
        }
    }
}

/// Dispatches the requested command.
fn run(cli: &Cli, out: &mut impl Write) -> Result<(), Box<dyn std::error::Error>> {
    match &cli.command {
        Command::Categories => {
            list_categories(cli.json, out)?;
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
                eprintln!("wrote the diagnostic report to {}", path.display());
            }

            if cli.json {
                writeln!(out, "{}", serde_json::to_string_pretty(&report)?)?;
            } else if *verbose {
                write!(out, "{}", cleaner_core::report::render(&report))?;
            } else {
                report_scan(&library, &plan, out)?;
            }

            Ok(())
        }
        Command::Clean {
            categories,
            confirm,
        } => clean(cli, categories, *confirm, out),
        Command::Compact => compact(cli, out),
        Command::Snapshot(command) => snapshots(cli, command, out),
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
fn list_categories(json: bool, out: &mut impl Write) -> std::io::Result<()> {
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
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&entries).unwrap_or_default()
        )?;
        return Ok(());
    }

    for category in Category::ALL {
        let mark = if category.default_selected() {
            "*"
        } else {
            " "
        };
        writeln!(
            out,
            "{mark} {:<16} {}",
            category.slug(),
            category.description()
        )?;
    }
    writeln!(out, "\n* selected by default")
}

/// Prints what a scan found.
///
/// `--json` goes through the report rather than here, because that is the shape worth
/// promising to a script.
fn report_scan(library: &Library, plan: &Plan, out: &mut impl Write) -> std::io::Result<()> {
    writeln!(out, "library: {}", library.root().display())?;
    writeln!(
        out,
        "{} beatmap sets, {} files, {}\n",
        plan.sets_scanned,
        plan.blobs_total,
        human_bytes(plan.bytes_total)
    )?;

    writeln!(
        out,
        "{:<16} {:>9} {:>12}",
        "category", "files", "reclaimable"
    )?;
    for (category, totals) in plan.totals_by_category() {
        writeln!(
            out,
            "{:<16} {:>9} {:>12}",
            category.slug(),
            totals.files,
            human_bytes(totals.bytes)
        )?;
    }

    writeln!(
        out,
        "\nscan took {}: {} measuring files, {} reading the database, {} reading beatmaps",
        seconds(plan.timings.total_ms()),
        seconds(plan.timings.measure_ms),
        seconds(plan.timings.database_ms),
        seconds(plan.timings.classify_ms),
    )?;
    writeln!(
        out,
        "\nRemove a category with: osu-lazer-cleaner clean <category> --confirm"
    )
}

/// Reports what a clean or a restore is doing, on stderr so stdout stays machine-readable.
fn report_clean(progress: cleaner_core::CleanProgress) {
    match progress {
        cleaner_core::CleanProgress::UpdatingDatabase => eprint!("\rupdating the database"),
        cleaner_core::CleanProgress::Preserving { done, total } => {
            eprint!("\rsaving files into the snapshot: {done}/{total}");
        }
        cleaner_core::CleanProgress::Removing { done, total } => {
            eprint!("\rremoving files from the library: {done}/{total}");
        }
        cleaner_core::CleanProgress::Restoring { done, total } => {
            eprint!("\rmoving files back: {done}/{total}");
        }
        cleaner_core::CleanProgress::Reattaching { done, total } => {
            eprint!("\rreattaching files to beatmap sets: {done}/{total}");
        }
    }
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

/// Removes the selected categories.
fn clean(
    cli: &Cli,
    categories: &[Category],
    confirm: bool,
    out: &mut impl Write,
) -> Result<(), Box<dyn std::error::Error>> {
    let library = open_library(cli.library.as_deref())?;
    let selected: HashSet<Category> = categories.iter().copied().collect();
    let plan = scan(&library, &selected)?;

    let options = Options { dry_run: !confirm };
    let outcome = cleaner_core::run(&library, &plan, &options, report_clean)?;
    eprintln!("\r                                                   ");

    if cli.json {
        writeln!(
            out,
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
        )?;
        return Ok(());
    }

    if outcome.dry_run {
        writeln!(
            out,
            "would remove {} files ({} references) and reclaim {}",
            plan.selected_files(),
            plan.selected_references(),
            human_bytes(outcome.bytes)
        )?;
        writeln!(out, "re-run with --confirm to do it")?;
        return Ok(());
    }

    writeln!(
        out,
        "removed {} files and moved {} into a snapshot",
        outcome.detached,
        human_bytes(outcome.bytes)
    )?;

    if let Some(path) = &outcome.snapshot {
        let id = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        writeln!(
            out,
            "\nNothing is deleted yet. Start osu!lazer and check your beatmaps."
        )?;
        writeln!(
            out,
            "To reclaim the space: osu-lazer-cleaner snapshot delete {id} --confirm"
        )?;
        writeln!(
            out,
            "To undo instead:      osu-lazer-cleaner snapshot restore {id} --confirm"
        )?;
    }

    Ok(())
}

/// Rewrites the database without its free space.
fn compact(cli: &Cli, out: &mut impl Write) -> Result<(), Box<dyn std::error::Error>> {
    let library = open_library(cli.library.as_deref())?;
    let result = cleaner_core::compact(&library)?;

    if cli.json {
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "rewritten": result.rewritten,
                "before": result.before,
                "after": result.after,
                "freed": result.freed(),
                "backup": result.backup,
            }))?
        )?;
        return Ok(());
    }

    if !result.rewritten {
        // realm-core declines rather than fails here, so saying nothing would report a
        // compaction that never ran.
        writeln!(
            out,
            "database left alone at {}; something else has it open, so close osu!lazer and try \
             again",
            human_bytes(result.before)
        )?;
        return Ok(());
    }

    if result.freed() == 0 {
        writeln!(
            out,
            "database is already compact at {}; nothing to reclaim",
            human_bytes(result.before)
        )?;
    } else {
        writeln!(
            out,
            "database went from {} to {}, freeing {}",
            human_bytes(result.before),
            human_bytes(result.after),
            human_bytes(result.freed())
        )?;
    }

    Ok(())
}

/// Handles the snapshot subcommands.
fn snapshots(
    cli: &Cli,
    command: &SnapshotCommand,
    out: &mut impl Write,
) -> Result<(), Box<dyn std::error::Error>> {
    let library = open_library(cli.library.as_deref())?;
    let available = snapshot::list(&library)?;

    match command {
        SnapshotCommand::List => {
            list_snapshots(&available, cli.json, out)?;
            Ok(())
        }
        SnapshotCommand::Restore { id, confirm } => {
            restore_snapshot(&library, &available, id, *confirm, cli.json, out)
        }
        SnapshotCommand::Delete { id, confirm } => {
            delete_snapshot(&library, &available, id, *confirm, cli.json, out)
        }
    }
}

/// Prints the snapshots, newest first.
fn list_snapshots(
    available: &[cleaner_core::Snapshot],
    json: bool,
    out: &mut impl Write,
) -> std::io::Result<()> {
    if json {
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
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&entries).unwrap_or_default()
        )?;
        return Ok(());
    }

    if available.is_empty() {
        writeln!(out, "no snapshots")?;
        return Ok(());
    }

    writeln!(out, "Newest first. Restore in this order.\n")?;
    writeln!(out, "{:<24} {:>8} {:>12}  created", "id", "files", "size")?;
    for entry in available {
        writeln!(
            out,
            "{:<24} {:>8} {:>12}  {}",
            entry.id(),
            entry.manifest.blobs.len(),
            human_bytes(entry.manifest.bytes()),
            entry
                .manifest
                .created
                .to_zoned(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d %H:%M")
        )?;
    }
    Ok(())
}

/// Moves one snapshot's files back into the library.
fn restore_snapshot(
    library: &Library,
    available: &[cleaner_core::Snapshot],
    id: &str,
    confirm: bool,
    json: bool,
    out: &mut impl Write,
) -> Result<(), Box<dyn std::error::Error>> {
    let entry = find(available, id)?;

    // Snapshots are listed newest first, and each was taken against the library as the one
    // before it left it. Restoring an older one on its own would leave the database pointing
    // at files a newer snapshot still holds.
    let newest = available.first().map(cleaner_core::Snapshot::id);
    if newest.as_deref() != Some(id) {
        return Err(format!(
            "restore snapshots newest first; start with {}",
            newest.unwrap_or_default()
        )
        .into());
    }

    if !confirm {
        if json {
            writeln!(
                out,
                "{}",
                serde_json::json!({
                    "dry_run": true, "snapshot": id, "files": entry.manifest.blobs.len(),
                    "bytes": entry.manifest.bytes(), "restored": 0,
                })
            )?;
            return Ok(());
        }
        writeln!(
            out,
            "would put {} files ({}) back and remove snapshot {id}",
            entry.manifest.blobs.len(),
            human_bytes(entry.manifest.bytes())
        )?;
        writeln!(out, "re-run with --confirm to do it")?;
        return Ok(());
    }

    let restored = cleaner_core::restore(library, entry, report_clean)?;
    eprintln!("\r                                             ");
    if json {
        writeln!(
            out,
            "{}",
            serde_json::json!({
                "dry_run": false, "snapshot": id, "files": entry.manifest.blobs.len(),
                "bytes": entry.manifest.bytes(), "restored": restored,
            })
        )?;
    } else {
        writeln!(out, "restored {restored} files into the library")?;
        writeln!(
            out,
            "snapshot {id} is gone; its files are back where they were"
        )?;
    }
    Ok(())
}

/// Deletes one snapshot, which is the only operation that destroys anything.
fn delete_snapshot(
    library: &Library,
    available: &[cleaner_core::Snapshot],
    id: &str,
    confirm: bool,
    json: bool,
    out: &mut impl Write,
) -> Result<(), Box<dyn std::error::Error>> {
    let entry = find(available, id)?;

    if !confirm {
        if json {
            writeln!(
                out,
                "{}",
                serde_json::json!({
                    "dry_run": true, "snapshot": id, "bytes": entry.manifest.bytes(), "deleted": false,
                })
            )?;
            return Ok(());
        }
        writeln!(
            out,
            "would delete snapshot {} and reclaim {}",
            entry.id(),
            human_bytes(entry.manifest.bytes())
        )?;
        writeln!(out, "re-run with --confirm to do it; this cannot be undone")?;
        return Ok(());
    }

    let reclaimed = entry.manifest.bytes();
    snapshot::delete(library, entry)?;
    if json {
        writeln!(
            out,
            "{}",
            serde_json::json!({
                "dry_run": false, "snapshot": id, "bytes": reclaimed, "deleted": true,
            })
        )?;
    } else {
        writeln!(
            out,
            "deleted snapshot {id} and reclaimed {}",
            human_bytes(reclaimed)
        )?;
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn json_command(library: &Library, arguments: &[&str]) -> serde_json::Value {
        let mut args = vec![
            "osu-lazer-cleaner",
            "--json",
            "--library",
            library.root().to_str().unwrap(),
        ];
        args.extend_from_slice(arguments);
        let cli = Cli::try_parse_from(args).unwrap();
        let mut out = Vec::new();
        run(&cli, &mut out).unwrap();
        serde_json::from_slice(&out).unwrap_or_else(|error| {
            panic!(
                "{arguments:?} emitted invalid JSON: {error}: {}",
                String::from_utf8_lossy(&out)
            )
        })
    }

    fn empty_snapshot(library: &Library) -> String {
        let dir = snapshot::begin(library).unwrap();
        let manifest = cleaner_core::Manifest {
            format_version: snapshot::FORMAT_VERSION,
            created: jiff::Timestamp::now(),
            app_version: "test".to_owned(),
            schema_version: 52,
            detached: Vec::new(),
            blobs: Vec::new(),
        };
        snapshot::finalise(&dir, &manifest)
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn every_json_command_emits_one_json_document() {
        let directory = tempfile::tempdir().unwrap();
        drop(cleaner_realm::fixture::synthetic_realm(
            &directory.path().join("client.realm"),
            &[],
        ));
        let library = Library::open(directory.path()).unwrap();

        assert!(json_command(&library, &["categories"]).is_array());
        assert!(json_command(&library, &["scan"])["categories"].is_array());
        assert_eq!(json_command(&library, &["clean", "junk"])["dry_run"], true);
        assert_eq!(
            json_command(&library, &["clean", "junk", "--confirm"])["dry_run"],
            false
        );
        assert!(json_command(&library, &["compact"])["rewritten"].is_boolean());
        assert!(json_command(&library, &["snapshot", "list"]).is_array());

        let id = empty_snapshot(&library);
        assert_eq!(
            json_command(&library, &["snapshot", "restore", &id])["dry_run"],
            true
        );
        assert_eq!(
            json_command(&library, &["snapshot", "restore", &id, "--confirm"])["dry_run"],
            false
        );
        let id = empty_snapshot(&library);
        assert_eq!(
            json_command(&library, &["snapshot", "delete", &id])["deleted"],
            false
        );
        assert_eq!(
            json_command(&library, &["snapshot", "delete", &id, "--confirm"])["deleted"],
            true
        );

        let report = directory.path().join("report.json");
        let output = json_command(&library, &["scan", "--dump", report.to_str().unwrap()]);
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(report).unwrap()).unwrap();
        assert_eq!(output, saved);
    }
}
