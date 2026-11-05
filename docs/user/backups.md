# Backups

[Documentation](../README.md)

Preferences → Backups creates and lists backups of rbxport's library.
rekordbox's library is never backed up or restored over here. Music files are
excluded, so keep a separate backup of your audio.

## Contents and format

A new backup is a ZIP file named `rbexport-YYYYMMDD-HHMM.zip` using local
24-hour time. It contains:

- The library database, including playlists, tags, ratings, and history.
- The complete `PIONEER/USBANLZ` analysis folder, including cues, grids, waveforms, phrases, and vocals.
- `PIONEER/Artwork` images and thumbnails.
- Sync Manager selections and the Automix playlist, when present.
- `import.db`, the record of what was imported from rekordbox, when present.
  It is restored with the database so a later import from rekordbox compares
  against the library as restored.
- `summary.json`, recording track/playlist/cue counts and component sizes.

ZIP creation uses maximum Deflate compression and parallel work based on CPU
capacity. A same-minute backup is never overwritten. Backups stay until deleted.

## Storage and estimates

The saved-backup list shows date, time, and size. **Default backup folder →
Change folder…** changes where future backups are written and which folder
is listed. Existing archives remain in their original folders. The setting
persists across restarts.

The Rekordbox Data bar saves its last successful estimate. It refreshes a
week-old estimate on opening Backups or while that pane stays open.
**Refresh** requests an immediate estimate.

## Restore

Restore through the separate RBXport Restore app. Quit RBXport first. The restore app lists backups and uses `summary.json` when
available to show their contents.

Restore the whole backup or selected components: database, analysis, artwork,
or the files beside the database (Sync Manager/Automix selections and
`import.db`). Each backup also holds `restore-library.sh` and
`restore-library.ps1`, which restore it without the app once RBXport is quit. If interrupted, the next RBXport or
RBXport Restore startup rolls back the operation, or completes it when new
files were already in place, before opening the library.

## Developer entry points

Backup integration lives in `src-tauri/src/backups.rs` and the adjacent
backup helper modules, with shared archive logic in `crates/rbl-backup/`.
Validate backup/restore changes against disposable data, following [Testing](../development/testing.md).
