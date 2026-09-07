//! A diagnostic summary of a scan.
//!
//! A [`Plan`] holds one entry per file reference, which on a large library means hundreds of
//! thousands of them. That is too much to read and too much to write to a file. This condenses
//! it to the numbers worth looking at, with a handful of examples per category so the
//! classifier's decisions can be checked by eye.

use crate::catalog::Category;
use crate::format::human_bytes;
use crate::plan::{Plan, Timings};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

/// How many example filenames to keep per category.
const EXAMPLES: usize = 12;

/// A scan, condensed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    /// The library this describes.
    pub library: String,
    /// Version of the tool that produced it.
    pub app_version: String,
    /// Totals for the whole library.
    pub library_totals: LibraryTotals,
    /// How long each phase took.
    pub timings: Timings,
    /// One entry per category.
    pub categories: Vec<CategoryReport>,
    /// How often files are shared between beatmap sets.
    pub sharing: Sharing,
    /// File extensions across everything the scan classified.
    pub extensions: BTreeMap<String, usize>,
}

/// Totals describing the library as a whole.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct LibraryTotals {
    /// Beatmap sets examined.
    pub sets: usize,
    /// Distinct files in the blob store.
    pub files: usize,
    /// Bytes the blob store holds.
    pub bytes: u64,
}

/// What one category found.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryReport {
    /// Name used on the command line.
    pub category: String,
    /// Distinct files that would leave the library.
    pub files: usize,
    /// References that would be detached, counting one per owning beatmap set.
    pub references: usize,
    /// Bytes that would be freed.
    pub bytes: u64,
    /// References whose file stays because something else still points at it.
    pub kept_shared: usize,
    /// Example filenames, for checking the classifier by eye.
    pub examples: Vec<String>,
}

/// How much deduplication the library has.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Sharing {
    /// References across every category.
    pub references: usize,
    /// Distinct files behind those references.
    pub files: usize,
    /// References pointing at a file that another beatmap set also uses.
    pub shared_references: usize,
}

/// Condenses a plan into a report.
#[must_use]
pub fn summarise(plan: &Plan, library: &str) -> Report {
    let mut extensions: BTreeMap<String, usize> = BTreeMap::new();
    let mut all_hashes: HashSet<&str> = HashSet::new();
    let mut references = 0;
    let mut shared_references = 0;

    let categories = plan
        .groups
        .iter()
        .map(|group| {
            let mut seen: HashSet<&str> = HashSet::new();
            let mut examples = Vec::new();
            let mut kept_shared = 0;

            for candidate in &group.candidates {
                references += 1;
                all_hashes.insert(candidate.hash.as_str());

                if candidate.frees_blob {
                    seen.insert(candidate.hash.as_str());
                } else {
                    kept_shared += 1;
                    shared_references += 1;
                }

                *extensions
                    .entry(extension_of(&candidate.filename))
                    .or_default() += 1;

                if examples.len() < EXAMPLES {
                    examples.push(candidate.filename.clone());
                }
            }

            CategoryReport {
                category: group.category.slug().to_owned(),
                files: group.files,
                references: group.candidates.len(),
                bytes: group.bytes,
                kept_shared,
                examples,
            }
        })
        .collect();

    Report {
        library: library.to_owned(),
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
        library_totals: LibraryTotals {
            sets: plan.sets_scanned,
            files: plan.blobs_total,
            bytes: plan.bytes_total,
        },
        timings: plan.timings,
        categories,
        sharing: Sharing {
            references,
            files: all_hashes.len(),
            shared_references,
        },
        extensions,
    }
}

/// Renders a report as lines for a terminal.
#[must_use]
pub fn render(report: &Report) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    let _ = writeln!(out, "library: {}", report.library);
    let _ = writeln!(
        out,
        "{} beatmap sets, {} files, {}",
        report.library_totals.sets,
        report.library_totals.files,
        human_bytes(report.library_totals.bytes)
    );

    let total = report.timings.measure_ms + report.timings.database_ms + report.timings.classify_ms;
    let _ = writeln!(
        out,
        "\nscan took {}: {} measuring files, {} reading the database, {} reading beatmaps",
        seconds(total),
        seconds(report.timings.measure_ms),
        seconds(report.timings.database_ms),
        seconds(report.timings.classify_ms)
    );

    let _ = writeln!(
        out,
        "\n{:<16} {:>9} {:>12} {:>12} {:>10}",
        "category", "files", "reclaimable", "references", "kept"
    );
    for category in &report.categories {
        let _ = writeln!(
            out,
            "{:<16} {:>9} {:>12} {:>12} {:>10}",
            category.category,
            category.files,
            human_bytes(category.bytes),
            category.references,
            category.kept_shared
        );
    }

    let _ = writeln!(
        out,
        "\n{} references across {} distinct files; {} point at a file another set also uses",
        report.sharing.references, report.sharing.files, report.sharing.shared_references
    );

    let _ = writeln!(out, "\nextensions among classified files:");
    let mut by_count: Vec<_> = report.extensions.iter().collect();
    by_count.sort_by_key(|(_, count)| std::cmp::Reverse(**count));
    for (extension, count) in by_count.iter().take(15) {
        let _ = writeln!(out, "  {count:>9}  {extension}");
    }

    for category in &report.categories {
        if category.examples.is_empty() {
            continue;
        }
        let _ = writeln!(out, "\nexamples, {}:", category.category);
        for example in &category.examples {
            let _ = writeln!(out, "  {example}");
        }
    }

    out
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

/// Lowercase extension of a filename, or a placeholder when it has none.
fn extension_of(filename: &str) -> String {
    let name = filename.rsplit('/').next().unwrap_or(filename);
    name.rsplit_once('.').map_or_else(
        || "(none)".to_owned(),
        |(_, extension)| format!(".{}", extension.to_ascii_lowercase()),
    )
}

/// Counts how many distinct files each category would remove, keyed by slug.
///
/// Exposed for callers that want the totals without the rest of the report.
#[must_use]
pub fn files_by_category(plan: &Plan) -> HashMap<Category, usize> {
    plan.groups.iter().map(|g| (g.category, g.files)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Candidate, Group};

    fn candidate(hash: &str, name: &str, frees: bool) -> Candidate {
        Candidate {
            set_index: 0,
            file_index: 0,
            filename: name.to_owned(),
            hash: hash.to_owned(),
            bytes: 10,
            category: Category::Videos,
            frees_blob: frees,
        }
    }

    #[test]
    fn counts_shared_references_separately_from_files() {
        let plan = Plan {
            groups: vec![Group {
                category: Category::Videos,
                candidates: vec![
                    candidate("aa", "a.mp4", true),
                    candidate("bb", "b.mp4", false),
                ],
                files: 1,
                bytes: 10,
                selected: true,
            }],
            ..Plan::default()
        };

        let report = summarise(&plan, "/tmp/library");

        assert_eq!(report.sharing.references, 2);
        assert_eq!(report.sharing.shared_references, 1);
        assert_eq!(report.categories[0].files, 1);
        assert_eq!(report.categories[0].kept_shared, 1);
        assert_eq!(report.extensions.get(".mp4"), Some(&2));
    }

    #[test]
    fn extensions_are_lowercased_and_ignore_directories() {
        assert_eq!(extension_of("sb/Foo.PNG"), ".png");
        assert_eq!(extension_of("noextension"), "(none)");
    }

    #[test]
    fn rendering_mentions_every_category() {
        let plan = Plan {
            groups: Category::ALL
                .iter()
                .map(|&category| Group {
                    category,
                    candidates: Vec::new(),
                    files: 0,
                    bytes: 0,
                    selected: false,
                })
                .collect(),
            ..Plan::default()
        };

        let text = render(&summarise(&plan, "/tmp/library"));
        for category in Category::ALL {
            assert!(
                text.contains(category.slug()),
                "missing {}",
                category.slug()
            );
        }
    }
}
