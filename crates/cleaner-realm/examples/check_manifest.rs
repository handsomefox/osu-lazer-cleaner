//! Reports how many files in a snapshot manifest still have database references.
//!
//! This helps check a restore by comparing the manifest with current file usages.
//!
//! ```text
//! cargo run -p cleaner-realm --example check_manifest -- client.realm manifest.json
//! ```

#![expect(
    clippy::print_stdout,
    reason = "a diagnostic example whose entire purpose is printing to a terminal"
)]

use std::collections::HashSet;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let (Some(realm_path), Some(manifest_path)) = (args.next(), args.next()) else {
        return Err("usage: check_manifest <client.realm> <manifest.json>".into());
    };

    let text = std::fs::read_to_string(&manifest_path)?;
    let manifest: serde_json::Value = serde_json::from_str(&text)?;

    let wanted: HashSet<String> = manifest["detached"]
        .as_array()
        .ok_or("manifest has no detached list")?
        .iter()
        .filter_map(|entry| entry["hash"].as_str().map(str::to_owned))
        .collect();

    let scratch = tempfile::tempdir()?;
    let copy = scratch.path().join("client.realm");
    std::fs::copy(&realm_path, &copy)?;
    let realm = cleaner_realm::Realm::open_read_only(&copy)?;
    let present: HashSet<String> = realm.read_library()?.usage_counts.into_keys().collect();

    // A hash still referenced by something has a row for certain. For the rest, the row may or
    // may not survive, so report both numbers rather than one misleading total.
    let still_referenced = wanted.iter().filter(|h| present.contains(*h)).count();

    println!("manifest names {} distinct files", wanted.len());
    println!("  {still_referenced} are still referenced by something in the database");
    println!(
        "  {} have no current references",
        wanted.len() - still_referenced
    );

    Ok(())
}
