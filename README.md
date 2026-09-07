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
removing it would return.

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

## Safety model

Difficulty files and audio tracks are never removed. A difficulty's hash is what identifies it
to the osu! servers, so deleting or editing one breaks score submission on that beatmap.

A file is only removed once nothing else refers to it. osu!lazer applies the same rule in
`RealmFileStore.Cleanup`, expressed as the query `Usages.@count = 0`. Beatmap sets are not the
only owners: replays and skins hold references too, so a hitsound shared between a beatmap and
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

A clean detaches files from the database and moves them into a snapshot. Moving is a rename,
so it is instant and needs no extra disk space, which matters when a clean displaces tens of
gigabytes.

Your free space does not change yet. That is deliberate. Start osu!lazer, check that your
beatmaps still play, and then delete the snapshot to reclaim the space. If something is wrong,
restore the snapshot instead and the files go back.

Deleting a snapshot is the only operation that destroys anything, and it asks first.

## Install

Download the latest release from the
[releases page](https://github.com/handsomefox/osu-lazer-cleaner/releases) and run
`osu-lazer-cleaner.exe`. It finds your library automatically, including when `storage.ini`
points somewhere other than the default location.

Close osu!lazer before cleaning.

## Command line

The same operations are available from `osu-lazer-cleaner-cli`, which is useful for scripting
and for checking a scan without a desktop session.

```
osu-lazer-cleaner-cli scan
osu-lazer-cleaner-cli clean videos storyboards --confirm
osu-lazer-cleaner-cli snapshot list
osu-lazer-cleaner-cli snapshot delete 20260907-215337 --confirm
```

Every command takes `--library <path>` to point at a specific directory and `--json` for
machine-readable output. `clean` and `snapshot delete` report what they would do and change
nothing until you add `--confirm`.

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
- `cleaner-app` is the desktop interface. It is the only crate that depends on egui.
- `cleaner-cli` is the command-line interface.

Building realm-core needs `cmake` and a C++17 compiler. A cold build takes about two minutes.

Cross-compile a Windows executable from Linux with `scripts/package-windows.sh`, which needs
`cargo-xwin`. Testing that executable needs the database on an NTFS volume: Windows file
locking does not work over the WSL filesystem.

Some tests need a real osu!lazer library at `ref/client.realm`. They are skipped when it is
absent, which is why CI does not exercise them.

## License

MIT. See [LICENSE](LICENSE).
