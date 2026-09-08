//! Timing probes, so a claim about speed has a number behind it.
//!
//! These are `#[ignore]`d: they build a plan the size of a large library, which is too slow for
//! an ordinary test run and measures nothing on a small one. Run them with
//! `cargo test --release -p cleaner-core -- --ignored --nocapture`, and only in release: a
//! debug build measures the debug build.

#![cfg(test)]
#![expect(
    clippy::print_stdout,
    reason = "a timing probe reports its measurement on stdout"
)]

use crate::catalog::Category;
use crate::plan::{Candidate, Group, HASH_SEED, Plan};
use std::collections::BTreeMap;
use std::fmt::Write as _;

#[test]
#[ignore = "timing probe; run with --release --ignored --nocapture"]
fn selection_cost_on_a_large_library() {
    let categories = [
        (Category::Storyboards, 154_002),
        (Category::Backgrounds, 39_821),
        (Category::Hitsounds, 46_070),
        (Category::SkinElements, 9_052),
        (Category::Videos, 0),
        (Category::Junk, 0),
        (Category::Unreferenced, 104),
    ];

    let mut groups = Vec::new();
    let mut set_titles = BTreeMap::new();
    let mut serial = 0_u64;

    for (category, count) in categories {
        let mut candidates = Vec::with_capacity(count);
        for index in 0..count {
            serial += 1;
            let mut set = [0_u8; 16];
            set[..8].copy_from_slice(&((index as u64 % 34_562) + 1).to_le_bytes());
            set_titles
                .entry(set)
                .or_insert_with(|| format!("Artist {index} - Title"));
            candidates.push(Candidate {
                set_index: index,
                set_id: set,
                file_index: index,
                filename: format!("file{index}.png"),
                hash: {
                    // Spread like a real digest rather than sharing a long zero prefix.
                    let mut digest = String::with_capacity(64);
                    let mut word = serial.wrapping_mul(HASH_SEED);
                    for _ in 0..8 {
                        word = word.wrapping_mul(HASH_SEED).rotate_left(17);
                        let _ = write!(
                            digest,
                            "{:08x}",
                            u32::try_from(word & 0xffff_ffff).unwrap_or(0)
                        );
                    }
                    digest
                },
                bytes: 1024,
                category,
                usage_count: 1,
            });
        }
        groups.push(Group {
            category,
            candidates,
            selected: true,
        });
    }

    let plan = Plan {
        groups,
        set_titles,
        ..Plan::default()
    };
    let total: usize = plan.groups.iter().map(Group::len).sum();

    for label in ["warm", "measured"] {
        let started = std::time::Instant::now();
        let selected = plan.selected_totals();
        let by_category = plan.totals_by_category();
        let sets = plan.sets_in(Category::Storyboards);
        let elapsed = started.elapsed();
        println!(
            "{label}: {total} candidates -> {:?} (files {}, categories {}, sets {})",
            elapsed,
            selected.files,
            by_category.len(),
            sets.len()
        );
    }
}
