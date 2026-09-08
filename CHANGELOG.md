# Changelog

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
