//! Prints the schema of a Realm database.
//!
//! This exists to prove the realm-core static libraries actually link into an executable,
//! which a library crate alone never demonstrates. It is also the quickest way to inspect an
//! unfamiliar osu!lazer library.
//!
//! ```text
//! cargo run -p cleaner-realm --example schema_dump -- ref/client.realm
//! ```

#![expect(
    clippy::print_stdout,
    reason = "a diagnostic example whose entire purpose is printing to a terminal"
)]

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(path) = std::env::args_os().nth(1) else {
        return Err("usage: schema_dump <path to client.realm>".into());
    };

    let realm = cleaner_realm::Realm::open_read_only(std::path::Path::new(&path))?;
    let mut classes = realm.classes()?;
    classes.sort_by_key(|c| std::cmp::Reverse(c.rows));

    println!("schema version: {}", realm.schema_version());
    for class in &classes {
        let kind = if class.embedded { "embedded" } else { "table" };
        println!("{:>10}  {:<24} {kind}", class.rows, class.name);
    }

    Ok(())
}
