# Changelog

## 1.3.0

- Fix the totals blanking to zero when the beatmap-set list opens, and the tick that holds a set
  back doing nothing at all. The screen owns the plan while it draws, and the handlers that ran
  during the frame were reaching for a plan that was not there.
- Index each beatmap set's filenames once instead of folding the whole list on every lookup.
  A custom sample index re-ran that scan for every timing point and every hit object naming one,
  which on a set with a thousand files was hundreds of millions of comparisons. The scan log now
  reports the files opened and the bytes read.
- Move the button that starts a clean to a bar of its own above the status line, so a long
  category list cannot scroll it out of reach.
- Give the categories a larger name, and line their beatmap-set buttons up down the table.
- Centre the empty screen on the one thing there is to do.
- Split the snapshots screen: snapshots on the left, the database and its compaction on the
  right, which now says up front that compacting keeps a copy of `client.realm`.
- Stop labels rendering as selected text when a drag passes over them.

## 1.2.1

- Log every operation with its outcome, duration, and throughput: library discovery, scans with
  their phase timings, cleans, restores, snapshot deletions, and compaction.
- Log failures with the whole error chain. Every failure the window reported reached the screen
  and nothing else, so an attached log could not explain one.
- Write the command line's log to the same file as the window, at `info`, while stderr keeps
  showing warnings only.
- Add per-blob detail under `RUST_LOG=debug`.

## 1.2.0

- Give the executable and the window an icon.
- Set the interface in Inter, and put Phosphor glyphs on the screens, categories, and actions.
- Add an About window carrying the version, the font licences, the project link, and a button
  that opens the log folder.
- Write one dated log file per day and keep the last seven, instead of truncating a single file
  on every run. The files move into `%LOCALAPPDATA%\osu-lazer-cleaner\logs`.
- Ask Windows where `AppData` lives instead of reading the environment, so a redirected profile
  still finds osu!lazer's library and this tool's own data.
- Reopen on the screen the window was closed on.
- Declare `longPathAware` and per-monitor DPI awareness in the Windows manifest.

## 1.1.0

- Match background, storyboard, video, and hitsound references without regard to filename case.
  A background also used by a storyboard stays in the background category.
- Retain snapshot links until a restore commits. Hard-link restores need no second copy of the
  blob contents. Interrupted version 1.0 restores can verify and reuse files already moved.
- Refuse corrupt restore destinations and missing shared blobs before committing restored references.
- Refuse snapshot deletion when another retained snapshot depends on its files.
- Recheck file reference counts under the database write lock before removing library links.
  Serialize cleaner operations for each library.
- Flush recovery files and directory metadata before committing database changes. Use a
  write-through rename for snapshot publication on Windows.
- Return JSON for every command that accepts `--json`, including compact, restore, and delete.
  Keep the `scan --dump` status message off stdout.
- Rebuild Realm when its vendored implementation or build inputs change.

## 1.0.0

- Initial release with desktop and command-line interfaces, content categories, reversible
  snapshots, and database compaction.
