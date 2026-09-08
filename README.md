# osu!lazer Cleaner

[![CI](https://github.com/handsomefox/osu-lazer-cleaner/actions/workflows/ci.yml/badge.svg)](https://github.com/handsomefox/osu-lazer-cleaner/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Removes videos, storyboards, hitsounds, skin elements, and backgrounds from an osu!lazer
library, and gives the space back. Every clean is reversible until you say otherwise.

This is an osu!lazer equivalent of [TcNo osu! Cleaner](https://github.com/TCNOco/TcNo-osu-Cleaner),
which only works on osu!stable. lazer stores beatmaps differently, and the difference matters:
there are no per-beatmap folders. A Realm database records which files each beatmap owns, and
the files themselves live in one flat store named by content hash. Two beatmaps that share a
hitsound share one file on disk. Deleting by filename, the way a stable-era cleaner does, would
take that file away from every beatmap using it.

## What it does

Scan a library and you get a table: how many files each category holds and how much space
removing it would return. Open **Browse sets** on any row to see which beatmaps make up that
number and untick the ones you want left alone.

| Category | What it removes |
|---|---|
| Videos | Video backdrops. Beatmaps play normally without them. |
| Storyboards | `.osb` scripts and the art only they use. Backgrounds are kept. |
| Backgrounds | Background images. Beatmaps play against a blank background. |
| Hitsounds | Custom hit samples. Beatmaps fall back to the default skin. |
| Skin elements | Per-beatmap skin overrides. Beatmaps fall back to your active skin. |
| Junk files | `thumbs.db`, `desktop.ini`, `.DS_Store`. |
| Unreferenced files | Files nothing points at any more. |

Junk and unreferenced files are selected by default, because neither can change how a beatmap
plays. The rest are opt-in.

A file two beatmap sets share only leaves when the clean gives up both references. Hold one of
the sets back in the browse list and the file stays where it is, and the totals say so.

## Safety model

Difficulty files and audio tracks are never removed. A difficulty's hash is what identifies it
to the osu! servers, so deleting or editing one breaks score submission on that beatmap.

A file is only removed once nothing else refers to it. osu!lazer applies the same rule in
`RealmFileStore.Cleanup`, expressed as the query `Usages.@count = 0`. Beatmap sets are not the
only owners: replays, skins, and cached online assets hold references too, so a hitsound shared between a beatmap and
a skin stays where it is when only the beatmap gives it up.

Removing anything other than a difficulty leaves `BeatmapSetInfo.Hash` untouched. osu!lazer
behaves the same way in `BeatmapManager.DeleteVideos`, which detaches a video without
recomputing the set hash.

Scanning never writes to your library. It copies the database to a temporary directory first,
because opening a Realm database creates lock files beside it even when opening read-only.

The tool refuses to run against a database whose schema is newer than it understands, and it
never migrates one. Scans open the database read-only, and cleans use a mode that adopts the
schema on disk rather than reconciling it against a declared one.

### Nothing is deleted in one step

A clean puts every file it is about to orphan into the snapshot before it touches the database,
using hard links, which need no extra space for the contents. It writes the recovery manifest,
commits the database, and only then removes the library's own links. Stop it at any point and
either nothing happened or the snapshot holds the bytes.

A file is never removed from the library until the snapshot demonstrably holds a copy of the
same size. A clean that fails before it commits deletes its half-built snapshot, because those
links would otherwise pin files the library still owns.

On a filesystem with no hard links the snapshot gets a copy instead, which needs the space
twice until you delete the snapshot.

Before detaching files, the tool checks their identities and current reference counts under
the database write lock. If a set or file changed since the scan, the clean stops and asks you
to scan again. After committing, it reacquires the write lock and checks reference counts
again before removing library links. A file that acquired a new owner stays in the library.

Recovery files are flushed before the database commits. Unix builds also flush the snapshot
directories and their parents. Windows builds flush files through writable handles and publish
the snapshot with a write-through rename. A per-library lock prevents two cleaner processes
from changing snapshots at the same time.

Your free space does not change yet. That is deliberate. Start osu!lazer, check that your
beatmaps still play, and then delete the snapshot to reclaim the space. If something is wrong,
restore the snapshot instead and the files go back.

Restore snapshots newest first. Restore also recreates file records that osu!lazer removed
after the clean. If a beatmap set is missing or a filename now holds different content, restore
stops and keeps the snapshot. The snapshot retains its links until the restored database
references commit, so an interrupted restore remains recoverable.

On a filesystem with hard links, restoring needs no second copy of the blob contents. Without
hard links, restore temporarily needs extra space for copies. An existing destination must
match the snapshot before the recovery copy can be removed. Interrupted restores from version
1.0 can resume by verifying already-moved files against their SHA-256 hashes.

Deleting a snapshot is the only operation that destroys anything, and it asks first.
Deletion is refused if another retained snapshot needs files that only this snapshot holds.
Restore the newer snapshot first, or delete the dependent older snapshot first.

## Install

Download the latest release from the
[releases page](https://github.com/handsomefox/osu-lazer-cleaner/releases) and run
`osu-lazer-cleaner.exe`. It finds your library automatically, including when `storage.ini`
points somewhere other than the default location.

Close osu!lazer before cleaning, restoring, or compacting the database. The window watches for
the game and says so while it is open.

Keyboard: `Ctrl+Tab` switches screens, `Ctrl+1` and `Ctrl+2` go straight to one, `F5` scans
again, and `Esc` closes whatever is open.

The **Snapshots** screen shows the database size and a **Compact database** button. Compaction
reclaims the space left by removed database rows. It reports when another open handle prevents
the rewrite. A copy of `client.realm` is taken first and kept until you delete it, so a rewrite
osu!lazer will not open costs nothing but the time to rename the copy back.

## Command line

One executable holds both interfaces. Run it with no arguments and the window opens; give it a
subcommand and it runs headless, which is useful for scripting and for checking a scan without
a desktop session.

```
osu-lazer-cleaner scan
osu-lazer-cleaner clean videos storyboards --confirm
osu-lazer-cleaner snapshot list
osu-lazer-cleaner snapshot delete 20260907-215337 --confirm
```

Every command takes `--library <path>` to point at a specific directory and `--json` for
machine-readable output. Successful JSON commands write one JSON document to stdout, including
when `scan --dump` also saves a report. Progress messages go to stderr. `clean`, `snapshot
restore`, and `snapshot delete` change nothing until you add `--confirm`.

## Development

```
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The workspace is layered so that everything portable stays testable on Linux:

- `cleaner-realm` binds realm-core's C API. osu!lazer's database format has no Rust SDK, so
  realm-core is vendored as a submodule and built from source. Clone with
  `git clone --recurse-submodules`.
- `cleaner-core` holds storage discovery, `.osu` and `.osb` parsing, scanning, and snapshots.
  It has no platform or interface dependencies.
- `cleaner-app` holds both interfaces and builds the one executable. It is the only crate that
  depends on egui.

Building realm-core needs `cmake` and a C++17 compiler. A cold build takes about two minutes.

Cross-compile a Windows executable from Linux with `scripts/package-windows.sh`, which needs
`cargo-xwin`. The same tool lints against the Windows target, which is worth running before
pushing because some differences appear only there:

```
cargo xwin clippy --workspace --all-targets --target x86_64-pc-windows-msvc -- -D warnings
```

Testing a Windows executable needs the database on an NTFS volume, because Windows file
locking does not work over the WSL filesystem.

Synthetic Realm tests run in CI and cover cached asset references, transaction rollback, and
restoration after osu!lazer removes a file record. Additional tests use a real library at
`ref/client.realm` and skip when it is absent. Tests always copy that database before opening it.

## License

MIT. See [LICENSE](LICENSE).
