//! Lists the junk files a library holds, with their hashes.
//!
//! Used to assemble a small scratch library for testing a clean end to end, without copying a
//! real library's several hundred gigabytes.

#![expect(
    clippy::print_stdout,
    reason = "a diagnostic example whose entire purpose is printing to a terminal"
)]

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(path) = std::env::args_os().nth(1) else {
        return Err("usage: find_junk <path to client.realm>".into());
    };

    let realm = cleaner_realm::Realm::open_read_only(std::path::Path::new(&path))?;

    for set in realm.read_library()?.beatmap_sets {
        for file in set.files {
            if cleaner_core::catalog::is_junk(&file.filename) {
                println!("{} {}", file.hash, file.filename);
            }
        }
    }

    Ok(())
}
