# Repository guidelines

An osu!lazer library cleaner. It removes unwanted beatmap content and keeps every removal
reversible until the user deletes the snapshot.

## Where code goes

The workspace is layered so that everything portable stays testable on Linux.

- `cleaner-realm` binds realm-core's C API and nothing else. realm-core is a submodule under
  `crates/cleaner-realm/vendor/`, pinned to the tag matching the Realm .NET version osu!lazer
  uses. `build.rs` compiles it and generates the bindings.
- `cleaner-core` holds every rule: storage discovery, `.osu` and `.osb` parsing, category
  assignment, scanning, and snapshots. It depends on no platform and no interface.
- `cleaner-app` is the egui interface, and the only crate allowed to depend on egui.
- `cleaner-cli` is the command-line interface.

A category is data in `cleaner_core::Category`, not a function that removes files. Adding one
means adding a variant and teaching `scan::categorise` to recognise it, not adding a new
removal path.

## Do not weaken these checks

Each rule comes from osu!lazer's own source. Read the source before changing one.

- **Reference counting spans every owner.** Beatmap sets, replays, and skins all own file
  usages, and schema 52 adds cached online assets. A blob may only move once no usage anywhere
  points at it, which is what `RealmFileStore.Cleanup` expresses as `Usages.@count = 0`.
  Counting only beatmap sets deletes files other things still need.
- **Difficulty files and audio tracks are never candidates.** A difficulty's hash identifies it
  online. Removing or rewriting one breaks score submission.
- **Never migrate the user's database.** Scans open it read-only. Cleans use
  `ADDITIVE_DISCOVERED`, which adopts the schema on disk. Migration callbacks only fire for
  `AUTOMATIC` and `MANUAL`, and neither is used here.
- **Scanning copies the database first.** Opening a Realm database creates `.lock` and
  `.management/` beside it even read-only, so scanning in place would write to the library.
- **A clean moves bytes into a snapshot, and never deletes them.** Deleting a snapshot is the
  only destructive operation, and it confirms first.
- **Paths are re-checked immediately before they are touched**, independently of the check made
  while scanning. A plan can be minutes old by the time it runs.

## Tests

Tests are inline `#[cfg(test)] mod tests` blocks using `tempfile`. There is no `tests/`
directory. Cover the rejection paths, not just the successes: a snapshot outside the library
must be refused, an already-missing file must not be an error, and a shared blob must survive
a clean that gives up one of its references.

Tests that need a real library read `ref/client.realm` and skip when it is absent, so CI never
exercises them. A committable synthetic database would fix that and is worth building once the
schema this depends on has settled.
