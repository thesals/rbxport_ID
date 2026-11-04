# Own library — specification

Status: Phase 0 (spec). Fork-only, like [Identify](../identify/IDENTIFY_SPEC.md).

## Goal

rbxport becomes a replacement for rekordbox, not another interface to rekordbox's database. It keeps its own library in its own folder. rekordbox is only a source to **import** from, and it is never written.

## Decisions (owner: the user, 2026-11-03)

1. **Keep the proven layout.** rbxport's library uses the same tables as rekordbox's `master.db` (SQLCipher, the same schema and passphrase that `rbl_db::new_library` already writes). The file is rbxport's, in rbxport's folder. Indexing, editing, analysis, USB export, LINK and the existing tests keep working unchanged.
2. **Import can run many times.**
   - A track that isn't in rbxport yet is imported in full.
   - A track that is already in rbxport gets a **delta** import: only what rbxport doesn't have yet.
   - Tag data is **never** taken from rekordbox for a track whose tags were changed in rbxport.
3. **Accepted Identify results** write to rbxport's database **and** to the audio file's tags. rekordbox is untouched.

## Hard constraints

- **H1.** Nothing rbxport does ever writes rekordbox's files. That covers `master.db` and its `-wal`/`-shm` files, `share/`, `options.json`, and the rest of the rekordbox folder.
  - **Enforced in code:** `rbl-db` refuses a read-write open of rekordbox's database. That is the path `options.json` names, or the default rekordbox folder, compared after canonicalising. This isn't just a convention.
- **H2. Import reads a copy, never the original.**
  - Copy `master.db` together with its `-wal`/`-shm`, and hash them before and after the copy. If anything changed while copying, discard the copy and retry, up to 3 times, then fail with "rekordbox changed the library while it was being copied; close rekordbox and try again".
  - Open only the copy. After the import, check that the original's hashes are unchanged and record that in the import log.
- **H3. Import never writes audio files.** The tracks point at the files where they are. Nothing is moved, renamed or retagged.
- **H4. Import never deletes anything from rbxport.** A track, playlist or cue that has been removed from rekordbox stays in rbxport.
- **H5. Tests must not touch rbxport's real library either.**
  - With `RB_LITE_TEST=1`, read-write opens of rbxport's own library folder are refused, the same way they are refused for the installed rekordbox library today.
  - Tests use `rbl_db::fixture::build` in a temporary directory, or copies of `C:\dev\id-testset\`.
- **H6. Identify file writes stay behind the "Write tags to audio files" lock**, default off.
- **H7. No silent tag overwrites.** Writes to the database go through the single edit path: `edit(...)` → `rbl_db::write::Writer` → `refresh_after_edit`.
- **H8. Never touch the USB export code path**, as in the Identify spec. Export keeps reading the library through `LibraryLocation` and doesn't care where the file lives.

## Where things live

| What | Windows | macOS |
|---|---|---|
| Library | `%APPDATA%\rbxport\library\master.db` | `~/Library/Application Support/rbxport/library/master.db` |
| Analysis files and artwork | `…\library\share\PIONEER\…` (the same relative layout as rekordbox, so the `AnalysisDataPath` and `ImagePath` values carry over unchanged) | same |
| Import bookkeeping | `…\library\import.db` (plain SQLite; see [Delta import](#delta-import)) | same |
| Import log | `…\library\import-log\<timestamp>.txt` | same |
| Identify | `%APPDATA%\rbxport\identify\` (unchanged) | unchanged |

- The folder is found through `dirs::data_dir()`, as Identify already does. It can't come from rekordbox's `options.json`.
- `RBXPORT_LIBRARY_DIR` overrides it, replacing `RBXPORT_OPTIONS` for e2e and test machines.
- `LibraryLocation.is_real_install` is split into two flags:
  - `is_user_library`: rbxport's real library, so the test guard H5 applies;
  - `is_rekordbox_install`: always refused for writes (H1), and never opened by the app at all except by the import's read of the copy.

## Starting up

1. If `…\rbxport\library\master.db` exists, open it.
2. If not, show **Welcome**, with two choices:
   - **Create a new library**: an empty library in rbxport's folder, using `rbl_db::new_library`. It no longer writes rekordbox's `options.json`; that would break H1.
   - **Import from rekordbox**: offered only when rekordbox's library is found. It creates the library, then runs the first import.
3. Opening rekordbox's library directly is removed. No startup path reads `options.json` except the importer.

## First import (rbxport's library is empty)

1. Make the verified copy (H2).
2. Copy the copied database's contents into a new rbxport library. This uses SQLCipher `sqlcipher_export` or the backup API, so no file of rekordbox's is ever opened for writing.
3. Copy `share/PIONEER/` (ANLZ files and artwork) into rbxport's `share/`. Files are copied, never moved. Each copy's size is checked against its source.
4. Record each imported track's **baseline** in `import.db` (see below).
5. Track IDs stay the same as rekordbox's. Existing Identify results, which are keyed by rekordbox content ID, stay valid.
6. Show a summary: tracks, playlists, cues, files copied, problems, and confirmation that rekordbox's original is unchanged.

## Delta import

The same entry point as the first import is used when rbxport's library already has tracks.

### Matching a rekordbox track to an rbxport track, in order

1. The rekordbox content ID was mapped by an earlier import: `import.db` `track_map(rb_id, rb_uuid, rbx_id)`.
2. No mapping, but the same audio file path (normalised: case-insensitive on Windows, `\`/`/` unified). The two are linked, and the track counts as existing, with rbxport's data kept.
3. Otherwise it's a new track. It's imported in full under a **fresh rbxport ID**. rekordbox's ID is never reused after the first import, because rbxport may have given that ID to its own track meanwhile.

### Tag fields

These are user-editable track metadata: title, artist, album, album artist, remixer, original artist, composer, lyricist, genre, label, release year/date, track/disc number, comment, rating, colour, ISRC, My Tags.

### Rule for an existing track

| rbxport's tag fields compared with the baseline | What the delta import does |
|---|---|
| All equal (never edited in rbxport) | Take rekordbox's current tag fields, then update the baseline to them. |
| Any differs (edited in rbxport, by hand, by Identify or any other way) | **Take no tag data** for this track. The baseline isn't changed either. |

The decision is made per track, as you specified: "if tracks have had their tags updated in rbxport".

Comparing with the baseline, rather than flagging each edit, means every way of editing counts, including future ones and edits from a CDJ over LINK, with no change to the edit path.

### Everything else on an existing track: fill only, never replace

- **Cues, hot cues, beat grid, waveform and analysis files:** taken only if rbxport has none of that kind for the track.
- **Playlists:**
  - new rekordbox playlists are created;
  - tracks added to a rekordbox playlist are appended to the matching rbxport playlist, unless that playlist was changed in rbxport since the last import, which a baseline of its members shows;
  - nothing is removed (H4).
- **Play history:** new history sessions are added; existing ones are kept as they are.

### Report

The delta report lists:
- new tracks;
- tracks whose tags were updated;
- tracks skipped because they were edited in rbxport, each with the differing fields, shown but not applied;
- what was filled in;
- matches made by file path.

## Identify after the switch

- **Accept** writes the reviewed fields in two steps:
  1. **rbxport's database**, through the single edit path. This is instant and needs no Reload Tag.
  2. **The audio file**, through the existing safe writer: atomic, undoable, deferred when the file is in use, and gated by H6.
- If the database write fails, nothing is written to the file. If the file write fails or is deferred, the database edit stands, and the result shows "file not written yet: …".
- **Undo** reverts both.
- `rekordbox_imports` (the read-only snapshot of rekordbox's record) is retired:
  - the review compares against rbxport's own record;
  - the table is kept, read-only, for results made before the switch, and later dropped by a schema migration.
- Fill-empty-only, auto-apply at ≥ 0.90, and the never-overwrite-silently rules are unchanged.
- An Identify edit counts as "edited in rbxport", so a later delta import won't bring rekordbox's old tags back for that track.
- The guide's Reload Tag section is removed.

## What changes in the existing code (inventory)

**Startup and location**
- `src-tauri/src/lib.rs` `spawn_library_load`: change `rbl_db::detect()` and `Library::open_installed_read_only()` to open rbxport's library location. The `read_only = is_rekordbox_running()` at lines 125 and 148 goes.
- `crates/rbl-db/src/lib.rs`:
  - `detect`, `detect_from`, `options_location`, `default_library_dir`: kept for the importer only;
  - new `own_library_dir()` and `open_own(...)`;
  - `write_refusal_reason` gains H1 and applies H5 to `is_user_library`;
  - `is_rekordbox_running` gating is dropped for rbxport's library.
- `crates/rbl-db/src/new_library.rs`: target rbxport's folder, and stop writing `options.json`.
- `src-tauri/src/new_library.rs` and `src/views/library/NewLibraryDialog.tsx`: become the Welcome screen.

**rekordbox-running and read-only gates**, removed for rbxport's library
- `crates/rbl-db/src/write.rs` `prepare` (around line 2501) and the staged-write guard (around line 2989).
- `src-tauri/src/commands.rs` `library_summary` and `disable_read_only`. `RBX_DISABLE_READ_ONLY` goes away.
- `src-tauri/src/grid.rs` (383, 481), `link.rs` (337), `analysis.rs` (375), `file_journal.rs` (142).
- `crates/rbl-backup/src/lib.rs` (108).
- UI: `App.tsx`, `TrackTable.tsx`, `TreeView.tsx`, `SyncManager.tsx`, `AdvancedPane.tsx`, `lib/menu.ts`, `lib/contextMenus.ts`, the `ipc` types and mock.

**Kept as it is**
- `link.rs` `refusal()`: rekordbox really does hold the LINK ports while it runs. Only the wording changes.

**Library Protection** (`protectLibrary`, default `true`)
- It stays, as an optional lock on rbxport's own library.
- Its default becomes `false`: a replacement app shouldn't start locked.
- Its text stops mentioning rekordbox.

**Backups** (`src-tauri/src/backups.rs`, `crates/rbl-backup`)
- They back up rbxport's library, including `import.db`, so a restore keeps the baselines in step.
- The PowerShell restore script stops checking for rekordbox processes.

**Identify** (`src-tauri/src/identify*`, `src/views/identify/*`)
- Accept, Undo and auto-apply write the database as well.
- The "rekordbox: …" rows are removed and the Reload Tag text goes.

**rekordbox XML import** (`crates/rbl-db/src/xml.rs`)
- It stays as a second import source and follows the same delta rules.

**Examples and dev tools** (~45 `examples/*.rs`, `rbl-difftool`)
- They keep `open_installed_read_only` for the read-only study of rekordbox's real behaviour.
- They're not built into the app.

**Docs**
- `CLAUDE.md` "Library safety": rbxport's own library is now the protected real library too (H5).
- The user guide gets a "Library and import" page.

**Not changed:** the `rbl-index` loading and cache, `rbl-export`/`rbl-pdb`/`rbl-onelibrary`, `rbl-dbserver`/`rbl-nfs`, `rbl-analysis`, `rbl-deck`. They all take a `LibraryLocation` and don't care whose folder it's in.

## Phases (stop after each for approval)

| Phase | Scope | Done when |
|---|---|---|
| 0 | This spec | Approved |
| 1 | rbxport's library location, H1 and H5 guards, Welcome screen with **Create a new library**, flags split | App starts with no rekordbox at all; writing rekordbox's paths is refused (tested); all checks pass |
| 2 | **First import** into an empty library: verified copy, `share/` copy, `import.db` with mappings and baselines, summary, import log | Fixture import matches its source row-for-row; the original's hash is unchanged (tested); a dry run on a copy of the real library |
| 3 | Remove the rekordbox gates and wording; Library Protection default; backups of rbxport's library | No user-visible "quit rekordbox" except LINK; e2e passes |
| 4 | Identify writes the database and the file; Undo for both; retire `rekordbox_imports`; update the guide | Accept/Undo tested for the database and the file, with the lock on and off |
| 5 | **Delta import**: matching, the per-track tag rule, fill-only for cues, grids and analysis, playlists, history, report; XML import on the same rules | Fixture scenarios: new track, untouched track updated, edited track skipped, path match, ID collision, playlist append |

## Questions for later (not blocking Phase 1)

- **Q1.** Should the tag rule also count a rating or colour change made on a CDJ, written back over LINK, as an "edit in rbxport"? (The spec says yes, since it's in rbxport's database.)
- **Q2.** Should the delta report let you take rekordbox's tags for some skipped tracks anyway? (Proposed for later; Phase 5 only reports.)
- **Q3.** Should the database keep rekordbox's passphrase, or get its own key? Keeping it is simplest. It isn't a security boundary, since the passphrase is public in rekordbox's own install.
