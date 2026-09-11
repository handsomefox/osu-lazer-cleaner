# Repository guidelines

An osu!lazer library cleaner. It removes unwanted beatmap content and keeps every removal
reversible until the user deletes the snapshot.

## Where code goes

The workspace is layered so that everything portable stays testable on Linux.

- `cleaner-realm` binds realm-core's C API and nothing else. realm-core is a submodule under
  `crates/cleaner-realm/vendor/`, pinned to the tag matching the Realm .NET version osu!lazer
  uses. `build.rs` compiles it and generates the bindings.
- `cleaner-core` holds every rule: storage discovery, `.osu` and `.osb` parsing, category
  assignment, scanning, and snapshots. It depends on no interface. The three places it touches a
  platform are `storage`, `running`, and `folders`, each behind `cfg` with a portable fallback,
  so the rest stays testable anywhere.
- `cleaner-app` holds both interfaces and builds the one executable. It is the only crate
  allowed to depend on egui. `main` opens the window when there are no arguments and hands over
  to `cli` when there are. `assets/` carries the icon and the vendored fonts, and `build.rs`
  compiles `app.rc` so the executable carries the icon, the version, and the manifest.

The vendored Inter files have their Private Use Area cmap entries stripped. Upstream Inter maps
stylistic-set alternates into U+E000..U+F8FF, where the Phosphor icon glyphs live, and Inter
sits earlier in the family, so re-vendoring Inter from upstream turns every icon into a Latin
alternate. `theme::install_fonts` carries the same warning.

A category is data in `cleaner_core::Category`, not a function that removes files. Adding one
means adding a variant and teaching `scan::categorise` to recognise it, not adding a new
removal path.

Totals are computed, never stored. `Plan::category_totals` and `Plan::selected_totals` both go
through `plan::freed_blobs`, which is the one place that knows a blob leaves only when every
usage pointing at it is being detached. A field holding a precomputed count would go stale the
moment the user holds a beatmap set back.

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
- **A clean gives up the library's link to a file only once the snapshot holds the same file.**
  `preserve_blob` hard-links it in first and `release_blob` re-checks the size before it
  unlinks. On one filesystem those two names are one inode, so no bytes move and none are lost.
  Deleting a snapshot is the only operation that destroys anything, and it confirms first.
- **Recovery data is published before the database commits.** A clean that fails before its
  commit removes its own half-built snapshot, because those links would pin blobs the library
  still owns. Never reorder those steps.
- **Paths are re-checked immediately before they are touched**, independently of the check made
  while scanning. A plan can be minutes old by the time it runs.

## Check both targets before pushing

Some differences only appear on Windows, and CI is a slow way to find them. bindgen maps C
enums to `u32` on Linux and `i32` on Windows, so a cast that is required on one platform is a
lint error on the other. `CMake` generators differ too: Ninja writes a flat output tree, while
the Visual Studio generator nests artifacts per build configuration.

```
cargo xwin clippy --workspace --all-targets --target x86_64-pc-windows-msvc -- -D warnings
```

This compiles for Windows; it does not run there. Differences in what the two operating systems
allow at runtime reach CI untouched, so think about them while writing rather than after. One
that already cost a release: Windows refuses `FlushFileBuffers` on a read-only handle, so
`File::open` followed by `sync_all` passes every test here and fails every test on Windows. Open
for writing when the point is to flush.

The Linux release builds in an `ubuntu:22.04` container, which holds its glibc floor at 2.35. A
newer image raises the floor, and `scripts/check-linux-floor.sh` fails the build when it does.

## Tests

Tests are inline `#[cfg(test)] mod tests` blocks using `tempfile`. There is no `tests/`
directory. Cover the rejection paths, not just the successes: a snapshot outside the library
must be refused, an already-missing file must not be an error, and a shared blob must survive
a clean that gives up one of its references.

Everything that must run everywhere uses `cleaner_realm::fixture`, which builds a scratch
database through the C API under the `test-support` feature. `cleaner-core` turns that feature
on for its own tests, and `end_to_end.rs` uses it to run whole cleans, restores, and deletes
against a real database. Nothing there needs a personal library.

Tests that want a library osu!lazer itself wrote read `ref/client-slim.realm`, falling back to
`ref/client.realm`, and skip when neither is there. `ref/` is gitignored, so CI never runs them.
Make the slim copy once with:

```
cargo run -p cleaner-realm --features test-support --example slim -- \
    ref/client.realm ref/client-slim.realm 400
```
