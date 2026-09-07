//! Counts file extensions across every beatmap set in a library.
//!
//! Useful for sanity-checking a scan: if a category reports zero files, this says whether the
//! library genuinely holds none or the classifier is missing them.
//!
//! ```text
//! cargo run -p cleaner-realm --example extensions -- ref/client.realm
//! ```

#![expect(
    clippy::print_stdout,
    reason = "a diagnostic example whose entire purpose is printing to a terminal"
)]

use std::collections::HashMap;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(path) = std::env::args_os().nth(1) else {
        return Err("usage: extensions <path to client.realm>".into());
    };

    let realm = cleaner_realm::Realm::open_read_only(std::path::Path::new(&path))?;
    let mut counts: HashMap<String, usize> = HashMap::new();

    for set in realm.read_library()?.beatmap_sets {
        for file in set.files {
            let extension = file.filename.rsplit_once('.').map_or_else(
                || "(none)".to_owned(),
                |(_, ext)| format!(".{}", ext.to_ascii_lowercase()),
            );
            *counts.entry(extension).or_default() += 1;
        }
    }

    let mut sorted: Vec<_> = counts.into_iter().collect();
    sorted.sort_by_key(|(_, count)| std::cmp::Reverse(*count));

    for (extension, count) in sorted.iter().take(30) {
        println!("{count:>9}  {extension}");
    }

    Ok(())
}
