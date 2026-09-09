# Changelog

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
