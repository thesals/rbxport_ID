//! The first import from rekordbox: rekordbox's library, copied and checked,
//! becomes rbxport's own.
//!
//! rekordbox's files are only ever read. The database is copied with its
//! hash taken before and after, and only the copy is opened; `share/PIONEER/`
//! (analysis files and artwork) is copied file by file. Everything is built
//! in a staging folder beside rbxport's library folder and moved into place
//! only when complete, so a failure part-way leaves no library behind for
//! the next start to find.
//!
//! What was imported is recorded in `import.db` beside the library: which
//! rekordbox row became which rbxport row, and each track's tag fields and
//! each playlist's members as they were imported. A later import compares
//! with those to tell what was changed in rbxport since.
//!
//! Importing again, into a library rbxport already keeps, is
//! [`import_again`]: the same verified copy, merged by the rules in
//! [`delta`].

use std::fmt::Write as _;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::write::Writer;
use crate::{DbError, Library, LibraryLocation, OpenMode, Result, SchemaSupport};

pub mod delta;

pub use delta::{DeltaReport, FieldDiff, FillKind, Filled, PlaylistLine, Skipped, Take, TrackLine};

/// The bookkeeping file beside rbxport's `master.db`. Plain SQLite.
pub const IMPORT_DB: &str = "import.db";
/// The folder beside rbxport's `master.db` the import logs go in.
pub const LOG_DIR: &str = "import-log";
/// How many times the database is copied before giving up on a library
/// that keeps changing.
const COPY_ATTEMPTS: u32 = 3;
/// `import.db`'s schema version (`PRAGMA user_version`).
const IMPORT_DB_VERSION: i64 = 1;

pub const CHANGED_WHILE_COPYING: &str =
    "rekordbox changed the library while it was being copied; close rekordbox and try again";
pub const UNSAVED_CHANGES: &str =
    "rekordbox has changes it has not saved into its library file yet; close rekordbox and try again";

/// Where the import is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Stage {
    Checking,
    CopyingDatabase,
    /// A repeat import, reading both libraries to match their tracks.
    Comparing,
    CopyingFiles,
    /// A repeat import, writing rbxport's library.
    Merging,
    Recording,
    Finishing,
}

/// How far the import is: `done` of `total` in `stage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub stage: Stage,
    pub done: u64,
    pub total: u64,
}

/// What the import did.
#[derive(Debug, Clone)]
pub struct Summary {
    /// Live (not deleted) rows of each kind, as imported.
    pub tracks: u64,
    pub playlists: u64,
    pub cues: u64,
    pub history_sessions: u64,
    /// Files of `share/PIONEER/` copied whole.
    pub files_copied: u64,
    pub bytes_copied: u64,
    /// Each file that could not be copied, or anything else to look at.
    pub problems: Vec<String>,
    /// SHA-256 of rekordbox's `master.db` as it was copied.
    pub source_sha256: String,
    /// rekordbox's `master.db` hashed the same after the import as when it
    /// was copied.
    pub source_unchanged: bool,
    /// Where a folder that was in the library's place, with no library in
    /// it, was moved to. Nothing in it is deleted.
    pub set_aside: Option<PathBuf>,
    /// The import log.
    pub log: PathBuf,
    /// rbxport's library, now there.
    pub location: LibraryLocation,
}

/// rekordbox's library on this machine, for importing: the one its
/// installed agent's `options.json` names, when that database is there.
/// Never [`crate::OPTIONS_ENV`]'s, which names rbxport's library on a test
/// machine.
#[must_use]
pub fn find_rekordbox() -> Option<LibraryLocation> {
    crate::agent_options_location()
        .ok()
        .and_then(|options| crate::detect_from(&options).ok())
        .filter(|location| location.master_db.is_file())
}

/// Imports rekordbox's library at `source` as rbxport's library in
/// `target_dir`, which must have no `master.db`. Reads `source` only.
pub fn first_import(
    source: &LibraryLocation,
    target_dir: &Path,
    progress: &mut dyn FnMut(Progress),
) -> Result<Summary> {
    let started_at = rbl_core::time::now();
    check_target(source, target_dir)?;
    let parent = target_dir
        .parent()
        .ok_or_else(|| DbError::Import(format!("{} has no folder", target_dir.display())))?;
    std::fs::create_dir_all(parent)?;

    progress(Progress { stage: Stage::Checking, done: 0, total: 0 });
    let (files, mut problems) = share_files(&source.share_root);
    let share_bytes: u64 = files.iter().map(|f| f.len).sum();
    let needed = std::fs::metadata(&source.master_db)?.len() + share_bytes + share_bytes / 10 + (64 << 20);
    let free = fs2::available_space(parent)?;
    if free < needed {
        return Err(DbError::Import(format!(
            "Not enough free space: the import needs {} and {} is free",
            human_bytes(needed),
            human_bytes(free)
        )));
    }

    let staging = tempfile::Builder::new().prefix(".library-import-").tempdir_in(parent)?;
    let staged_db = staging.path().join("master.db");
    progress(Progress { stage: Stage::CopyingDatabase, done: 0, total: 1 });
    let source_sha256 = copy_verified(&source.master_db, &staged_db, &mut |_| {})?;
    let counts = adopt_copy(&staged_db, &source.passphrase)?;
    progress(Progress { stage: Stage::CopyingDatabase, done: 1, total: 1 });

    let copied = copy_share(&source.share_root, &staging.path().join("share"), &files, progress)?;
    problems.extend(copied.problems);

    progress(Progress { stage: Stage::Recording, done: 0, total: 1 });
    let staged = crate::own_location_in(staging.path())?;
    let import_id = record(&staged, &source.master_db, &source_sha256, &started_at)?;
    progress(Progress { stage: Stage::Recording, done: 1, total: 1 });

    progress(Progress { stage: Stage::Finishing, done: 0, total: 1 });
    let after = sha256_file(&source.master_db)?;
    let source_unchanged = after == source_sha256 && refuse_unsaved(&source.master_db).is_ok();
    let mut summary = Summary {
        tracks: counts.tracks,
        playlists: counts.playlists,
        cues: counts.cues,
        history_sessions: counts.history_sessions,
        files_copied: copied.files,
        bytes_copied: copied.bytes,
        problems,
        source_sha256,
        source_unchanged,
        set_aside: None,
        log: PathBuf::new(),
        location: crate::own_location_in(target_dir)?,
    };
    let log_name = write_log(staging.path(), source, target_dir, &summary, &after, &started_at)?;
    finish_record(&staged, import_id, &summary, &log_name)?;

    summary.set_aside = clear_target(target_dir)?;
    let built = staging.keep();
    std::fs::rename(&built, target_dir).map_err(|e| {
        DbError::Import(format!(
            "the import is complete in {} but could not be moved to {}: {e}",
            built.display(),
            target_dir.display()
        ))
    })?;
    summary.log = target_dir.join(LOG_DIR).join(log_name);
    progress(Progress { stage: Stage::Finishing, done: 1, total: 1 });
    Ok(summary)
}

/// What a repeat import did, and the checks around it.
#[derive(Debug, Clone)]
pub struct DeltaSummary {
    pub report: DeltaReport,
    /// SHA-256 of rekordbox's `master.db` as it was copied.
    pub source_sha256: String,
    /// rekordbox's `master.db` hashed the same after the import.
    pub source_unchanged: bool,
    /// The import log.
    pub log: PathBuf,
}

/// Imports rekordbox's library at `source` again, into the library `writer`
/// holds, which already has one (see [`delta`] for the rules). Reads
/// `source` only, through a verified copy made in a staging folder beside
/// rbxport's library folder and removed afterwards. Tracks in `take` take
/// rekordbox's tags although edited in rbxport, while they still differ as
/// the report showed.
pub fn import_again(
    source: &LibraryLocation,
    writer: &mut Writer,
    take: &Take,
    progress: &mut dyn FnMut(Progress),
) -> Result<DeltaSummary> {
    let started_at = rbl_core::time::now();
    let target = writer.library().location().clone();
    let dir = target
        .master_db
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| DbError::Import(format!("{} has no folder", target.master_db.display())))?;
    if !source.master_db.is_file() {
        return Err(DbError::Import(format!("there is no rekordbox library at {}", source.master_db.display())));
    }
    let mut rekordbox = crate::rekordbox_library_dirs();
    rekordbox.extend(source.master_db.parent().map(Path::to_path_buf));
    rekordbox.push(source.share_root.clone());
    if crate::is_within_any(&dir, &rekordbox) || crate::is_within_any(&target.share_root, &rekordbox) {
        return Err(DbError::WriteRefused(crate::REKORDBOX_REFUSAL.to_owned()));
    }

    progress(Progress { stage: Stage::Checking, done: 0, total: 0 });
    let parent = dir.parent().unwrap_or(&dir);
    let staging = tempfile::Builder::new().prefix(".rekordbox-copy-").tempdir_in(parent)?;
    let copy = staging.path().join("master.db");
    progress(Progress { stage: Stage::CopyingDatabase, done: 0, total: 1 });
    let source_sha256 = copy_verified(&source.master_db, &copy, &mut |_| {})?;
    let copied = Library::open(
        LibraryLocation {
            master_db: copy,
            share_root: source.share_root.clone(),
            passphrase: source.passphrase.clone(),
            // The staging copy: read only, and neither library.
            is_user_library: false,
            is_rekordbox_install: false,
        },
        OpenMode::ReadOnly,
    )?;
    if let SchemaSupport::Degraded { missing } = &copied.schema().support {
        return Err(DbError::Import(format!("rekordbox's library is missing what rbxport needs: {}", missing.join(", "))));
    }
    progress(Progress { stage: Stage::CopyingDatabase, done: 1, total: 1 });

    let mut import_db = open_import_db(&dir)?;
    let recorded = delta::Recorded::read(&import_db)?;
    let (report, book) = delta::merge(writer, &copied, &source.share_root, &recorded, take, progress)?;
    drop(copied);

    progress(Progress { stage: Stage::Recording, done: 0, total: 1 });
    let import_id = record_delta(
        writer.library().connection(),
        &mut import_db,
        &book,
        &source.master_db,
        &source_sha256,
        &started_at,
    )?;
    progress(Progress { stage: Stage::Recording, done: 1, total: 1 });

    progress(Progress { stage: Stage::Finishing, done: 0, total: 1 });
    let after = sha256_file(&source.master_db)?;
    let source_unchanged = after == source_sha256 && refuse_unsaved(&source.master_db).is_ok();
    let mut summary = DeltaSummary { report, source_sha256, source_unchanged, log: PathBuf::new() };
    let log_name = write_delta_log(&dir, source, &summary, &after, &started_at)?;
    finish_delta_record(&import_db, import_id, &summary, &log_name)?;
    summary.log = dir.join(LOG_DIR).join(log_name);
    drop(staging);
    progress(Progress { stage: Stage::Finishing, done: 1, total: 1 });
    Ok(summary)
}

/// Records a repeat import in `import.db`: the new mappings, and the
/// baselines that change, read from rbxport's library as it is now.
fn record_delta(
    library: &Connection,
    db: &mut Connection,
    book: &delta::Bookkeeping,
    source_db: &Path,
    source_sha256: &str,
    started_at: &str,
) -> Result<i64> {
    let tags: std::collections::HashMap<String, String> = tag_fields(library)?.into_iter().collect();
    let members = delta::members_now(library)?;
    let tx = db.transaction()?;
    tx.execute(
        "INSERT INTO imports (kind, started_at, source_db, source_sha256) VALUES ('delta', ?1, ?2, ?3)",
        params![started_at, source_db.display().to_string(), source_sha256],
    )?;
    let import_id = tx.last_insert_rowid();
    // A mapping replaces one for the same rekordbox row, or one that held
    // the same rbxport row for a rekordbox row that is gone.
    for (table, rows) in [("track_map", &book.tracks), ("playlist_map", &book.playlists), ("history_map", &book.histories)] {
        let mut insert = tx.prepare(&format!(
            "INSERT OR REPLACE INTO {table} (rb_id, rb_uuid, rbx_id, import_id) VALUES (?1, ?2, ?3, ?4)"
        ))?;
        for (rb_id, rb_uuid, rbx_id) in rows {
            insert.execute(params![rb_id, rb_uuid, rbx_id, import_id])?;
        }
    }
    for (table, column, ids, now) in [
        ("track_baseline", "fields", &book.baselines, &tags),
        ("playlist_baseline", "members", &book.playlist_baselines, &members),
    ] {
        let mut insert =
            tx.prepare(&format!("INSERT OR REPLACE INTO {table} (rbx_id, {column}, import_id) VALUES (?1, ?2, ?3)"))?;
        for id in ids {
            if let Some(value) = now.get(id) {
                insert.execute(params![id, value, import_id])?;
            }
        }
    }
    tx.commit()?;
    Ok(import_id)
}

fn finish_delta_record(db: &Connection, import_id: i64, summary: &DeltaSummary, log_name: &str) -> Result<()> {
    let n = |v: usize| i64::try_from(v).unwrap_or(i64::MAX);
    let m = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    let r = &summary.report;
    db.execute(
        "UPDATE imports SET finished_at = ?2, source_unchanged = ?3, tracks = ?4, playlists = ?5, cues = ?6,
             history_sessions = ?7, files_copied = ?8, bytes_copied = ?9, problems = ?10, log = ?11
         WHERE id = ?1",
        params![
            import_id,
            rbl_core::time::now(),
            summary.source_unchanged,
            n(r.new_tracks.len()),
            n(r.playlists_created.len()),
            m(r.cues),
            m(r.history_sessions),
            m(r.files_copied),
            m(r.bytes_copied),
            n(r.problems.len()),
            format!("{LOG_DIR}/{log_name}"),
        ],
    )?;
    Ok(())
}

/// Writes a repeat import's log beside the library and returns its name.
/// Every track the report names is listed, with the fields that were not
/// taken.
fn write_delta_log(
    dir: &Path,
    source: &LibraryLocation,
    summary: &DeltaSummary,
    after: &str,
    started_at: &str,
) -> Result<String> {
    let r = &summary.report;
    let mut text = String::new();
    let _ = writeln!(text, "rbxport: import from rekordbox into an existing library");
    let _ = writeln!(text, "Started:  {started_at}");
    let _ = writeln!(text, "Finished: {}", rbl_core::time::now());
    let _ = writeln!(text, "From rekordbox's library: {}", source.master_db.display());
    let _ = writeln!(text, "Into rbxport's library:   {}", dir.display());
    let _ = writeln!(text, "rekordbox's master.db SHA-256 when copied:       {}", summary.source_sha256);
    let _ = writeln!(text, "rekordbox's master.db SHA-256 after the import:  {after}");
    let _ = writeln!(
        text,
        "rekordbox's library unchanged: {}",
        if summary.source_unchanged { "yes" } else { "NO: rekordbox changed it after it was copied; the copy was whole" }
    );
    let tracks = |text: &mut String, title: &str, lines: &[TrackLine]| {
        let _ = writeln!(text, "{title}: {}", lines.len());
        for line in lines {
            let _ = writeln!(text, "  - {} [{}]", line.name, line.id);
        }
    };
    tracks(&mut text, "New tracks", &r.new_tracks);
    tracks(&mut text, "Tracks whose tags were updated from rekordbox", &r.updated);
    let _ = writeln!(text, "Tracks edited in rbxport, so rekordbox's tags were not taken: {}", r.skipped.len());
    for skipped in &r.skipped {
        let why = if skipped.linked_by_path { " (no earlier import to compare with)" } else { "" };
        let _ = writeln!(text, "  - {} [{}]{why}", skipped.track.name, skipped.track.id);
        for field in &skipped.fields {
            let _ = writeln!(text, "      {}: rbxport {:?}, rekordbox {:?}", field.field, field.rbxport, field.rekordbox);
        }
    }
    let _ = writeln!(text, "Tracks given what rbxport did not have: {}", r.filled.len());
    for filled in &r.filled {
        let what: Vec<&str> = filled.what.iter().map(|k| fill_name(*k)).collect();
        let _ = writeln!(text, "  - {} [{}]: {}", filled.track.name, filled.track.id, what.join(", "));
    }
    tracks(&mut text, "Matched by file path", &r.path_matches);
    let _ = writeln!(text, "Unchanged: {}", r.unchanged);
    let _ = writeln!(text, "Deleted in rbxport, left deleted: {}", r.kept_deleted);
    let _ = writeln!(text, "Left out, their file already came with another rekordbox track: {}", r.duplicates.len());
    for line in &r.duplicates {
        let _ = writeln!(text, "  - {line}");
    }
    let playlists = |text: &mut String, title: &str, lines: &[PlaylistLine]| {
        let _ = writeln!(text, "{title}: {}", lines.len());
        for line in lines {
            let _ = writeln!(text, "  - {} ({} tracks)", line.name, line.tracks);
        }
    };
    playlists(&mut text, "Playlists made", &r.playlists_created);
    playlists(&mut text, "Playlists with tracks appended", &r.playlists_appended);
    playlists(&mut text, "Playlists changed in rbxport, so nothing was appended", &r.playlists_skipped);
    let _ = writeln!(text, "History sessions added: {} ({} plays of tracks rbxport had)", r.history_sessions, r.plays_added);
    let _ = writeln!(text, "Cues copied: {}", r.cues);
    let _ = writeln!(text, "Analysis and artwork files copied: {} ({})", r.files_copied, human_bytes(r.bytes_copied));
    let _ = writeln!(text, "Problems: {}", r.problems.len());
    for problem in &r.problems {
        let _ = writeln!(text, "  - {problem}");
    }
    let logs = dir.join(LOG_DIR);
    std::fs::create_dir_all(&logs)?;
    let stamp = rbl_core::time::local_backup_stamp();
    let mut name = format!("{stamp}-import.txt");
    let mut n = 2;
    while logs.join(&name).exists() {
        name = format!("{stamp}-import-{n}.txt");
        n += 1;
    }
    std::fs::write(logs.join(&name), text)?;
    Ok(name)
}

/// What the log calls a kind of fill.
#[must_use]
pub fn fill_name(kind: FillKind) -> &'static str {
    match kind {
        FillKind::MemoryCues => "memory cues",
        FillKind::HotCues => "hot cues",
        FillKind::Analysis => "analysis (beat grid and waveforms)",
        FillKind::Key => "key",
        FillKind::Artwork => "artwork",
    }
}

/// Refuses a target that already holds a library, or that is rekordbox's
/// or inside rekordbox's folder.
fn check_target(source: &LibraryLocation, target_dir: &Path) -> Result<()> {
    if !source.master_db.is_file() {
        return Err(DbError::Import(format!("there is no rekordbox library at {}", source.master_db.display())));
    }
    let mut rekordbox = crate::rekordbox_library_dirs();
    rekordbox.extend(source.master_db.parent().map(Path::to_path_buf));
    rekordbox.push(source.share_root.clone());
    if crate::is_within_any(target_dir, &rekordbox) {
        return Err(DbError::WriteRefused(crate::REKORDBOX_REFUSAL.to_owned()));
    }
    if target_dir.join("master.db").exists() {
        return Err(DbError::Import(format!(
            "rbxport already has a library at {}; import into it from the File menu instead",
            target_dir.display()
        )));
    }
    Ok(())
}

/// Copies `source` to `dest`, which must not exist, and returns the hash
/// the two shared. The source is hashed before and after the copy and the
/// copy is hashed too: if they differ the copy is taken again, up to
/// [`COPY_ATTEMPTS`] times. A source with a `-wal` or `-journal` holding
/// anything is refused, since the database file alone is then not the whole
/// library; an empty one, as SQLite leaves after a clean close, is fine.
/// `between` runs after each copy, before the hashes are compared.
fn copy_verified(source: &Path, dest: &Path, between: &mut dyn FnMut(u32)) -> Result<String> {
    for attempt in 1..=COPY_ATTEMPTS {
        refuse_unsaved(source)?;
        let before = sha256_file(source)?;
        if dest.exists() {
            // The previous attempt's copy, in the staging folder.
            std::fs::remove_file(dest)?;
        }
        copy_file(source, dest)?.sync_all()?;
        between(attempt);
        let copied = sha256_file(dest)?;
        let after = sha256_file(source)?;
        refuse_unsaved(source)?;
        if before == copied && copied == after {
            return Ok(before);
        }
        tracing::warn!(attempt, "rekordbox's library changed while it was being copied; copying it again");
    }
    Err(DbError::Import(CHANGED_WHILE_COPYING.to_owned()))
}

fn refuse_unsaved(master_db: &Path) -> Result<()> {
    for suffix in ["-wal", "-journal"] {
        let mut beside = master_db.as_os_str().to_owned();
        beside.push(suffix);
        if std::fs::metadata(Path::new(&beside)).is_ok_and(|m| m.len() > 0) {
            return Err(DbError::Import(UNSAVED_CHANGES.to_owned()));
        }
    }
    Ok(())
}

/// Copies a file into a new one, which never takes the source's
/// attributes (a read-only flag would keep rbxport from writing its copy).
fn copy_file(from: &Path, to: &Path) -> std::io::Result<File> {
    let mut reader = File::open(from)?;
    let mut writer = File::create_new(to)?;
    std::io::copy(&mut reader, &mut writer)?;
    Ok(writer)
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[derive(Debug, Clone, Copy, Default)]
struct Counts {
    tracks: u64,
    playlists: u64,
    cues: u64,
    history_sessions: u64,
}

/// Makes the copy rbxport's: checked, encrypted with rbxport's passphrase
/// if rekordbox's was a different one, and with its journal folded in.
fn adopt_copy(db: &Path, source_passphrase: &str) -> Result<Counts> {
    let location = LibraryLocation {
        master_db: db.to_path_buf(),
        share_root: db.parent().map_or_else(|| PathBuf::from("share"), |p| p.join("share")),
        passphrase: source_passphrase.to_owned(),
        // The staging copy: neither the user's library yet nor rekordbox's.
        is_user_library: false,
        is_rekordbox_install: false,
    };
    let library = Library::open(location, OpenMode::ReadWrite)?;
    if let SchemaSupport::Degraded { missing } = &library.schema().support {
        return Err(DbError::Import(format!("rekordbox's library is missing what rbxport needs: {}", missing.join(", "))));
    }
    let conn = library.connection();
    let check: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if check != "ok" {
        return Err(DbError::Import(format!("the copy of rekordbox's library failed its integrity check: {check}")));
    }
    let count = |sql: &str| -> Result<u64> {
        let n: i64 = conn.query_row(sql, [], |r| r.get(0))?;
        Ok(u64::try_from(n).unwrap_or(0))
    };
    let counts = Counts {
        tracks: count("SELECT COUNT(*) FROM djmdContent WHERE rb_local_deleted = 0")?,
        playlists: count("SELECT COUNT(*) FROM djmdPlaylist WHERE rb_local_deleted = 0")?,
        cues: count("SELECT COUNT(*) FROM djmdCue WHERE rb_local_deleted = 0")?,
        history_sessions: count("SELECT COUNT(*) FROM djmdHistory WHERE rb_local_deleted = 0 AND Attribute = 0")?,
    };

    let own = crate::new_library::passphrase()?;
    if own != source_passphrase {
        // SQLCipher rekeys only outside WAL mode.
        let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        let wal = mode.eq_ignore_ascii_case("wal");
        if wal {
            conn.query_row("PRAGMA journal_mode = DELETE", [], |r| r.get::<_, String>(0))?;
        }
        conn.pragma_update(None, "rekey", &own)?;
        if wal {
            conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get::<_, String>(0))?;
        }
    }
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    drop(library);
    Ok(counts)
}

/// A file of `share/` to copy, by its path relative to `share/`.
#[derive(Debug, Clone)]
struct ShareFile {
    relative: PathBuf,
    len: u64,
}

/// The files under `share/PIONEER/`, and anything that could not be listed.
/// Links are not followed.
fn share_files(share_root: &Path) -> (Vec<ShareFile>, Vec<String>) {
    let mut files = Vec::new();
    let mut problems = Vec::new();
    let pioneer = share_root.join("PIONEER");
    if !pioneer.is_dir() {
        return (files, problems);
    }
    for entry in walkdir::WalkDir::new(&pioneer).follow_links(false) {
        match entry {
            Ok(entry) if entry.file_type().is_file() => {
                let Ok(relative) = entry.path().strip_prefix(share_root) else { continue };
                match entry.metadata() {
                    Ok(meta) => files.push(ShareFile { relative: relative.to_path_buf(), len: meta.len() }),
                    Err(e) => problems.push(format!("{}: {e}", relative.display())),
                }
            }
            Ok(_) => {}
            Err(e) => problems.push(format!("could not list part of rekordbox's share folder: {e}")),
        }
    }
    (files, problems)
}

#[derive(Debug, Default)]
struct Copied {
    files: u64,
    bytes: u64,
    problems: Vec<String>,
}

/// Copies `files` from one `share/` to another, checking each copy's size
/// against its source. A file that fails is a problem, not a failure: the
/// track keeps its row and can be analysed again.
fn copy_share(from: &Path, to: &Path, files: &[ShareFile], progress: &mut dyn FnMut(Progress)) -> Result<Copied> {
    for folder in ["PIONEER/Artwork", "PIONEER/USBANLZ"] {
        std::fs::create_dir_all(to.join(folder))?;
    }
    let total = u64::try_from(files.len()).unwrap_or(u64::MAX);
    let mut copied = Copied::default();
    progress(Progress { stage: Stage::CopyingFiles, done: 0, total });
    for (index, file) in files.iter().enumerate() {
        let source = from.join(&file.relative);
        let dest = to.join(&file.relative);
        let result = dest
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| copy_file(&source, &dest))
            .and_then(|written| Ok((written.metadata()?.len(), std::fs::metadata(&source)?.len())));
        match result {
            Ok((written, now)) if written == now => {
                copied.files += 1;
                copied.bytes += written;
            }
            Ok((written, now)) => copied
                .problems
                .push(format!("{}: copied {written} of {now} bytes", file.relative.display())),
            Err(e) => copied.problems.push(format!("{}: {e}", file.relative.display())),
        }
        let done = u64::try_from(index + 1).unwrap_or(u64::MAX);
        if done % 64 == 0 || done == total {
            progress(Progress { stage: Stage::CopyingFiles, done, total });
        }
    }
    Ok(copied)
}

/// Each track's tag fields as one JSON object, by track ID, as the import
/// records them and a later import compares them: the values a person edits,
/// with artists, album, genre and label by name, so a lookup row's ID never
/// matters.
pub fn tag_fields(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut statement = conn.prepare(
        "SELECT CAST(c.ID AS TEXT), json_object(
             'title', c.Title, 'artist', a.Name, 'album', al.Name, 'album_artist', aa.Name,
             'remixer', rm.Name, 'original_artist', oa.Name, 'composer', cp.Name,
             'lyricist', c.Lyricist, 'genre', g.Name, 'label', l.Name,
             'release_year', c.ReleaseYear, 'release_date', c.ReleaseDate,
             'track_no', c.TrackNo, 'disc_no', c.DiscNo, 'comment', c.Commnt,
             'rating', c.Rating, 'color', CAST(c.ColorID AS TEXT), 'isrc', c.ISRC,
             'my_tags', json((SELECT json_group_array(Name) FROM (
                 SELECT t.Name FROM djmdSongMyTag s JOIN djmdMyTag t ON t.ID = s.MyTagID
                 WHERE s.ContentID = c.ID AND s.rb_local_deleted = 0 ORDER BY t.Name))))
         FROM djmdContent c
         LEFT JOIN djmdArtist a ON a.ID = c.ArtistID
         LEFT JOIN djmdAlbum al ON al.ID = c.AlbumID
         LEFT JOIN djmdArtist aa ON aa.ID = al.AlbumArtistID
         LEFT JOIN djmdArtist rm ON rm.ID = c.RemixerID
         LEFT JOIN djmdArtist oa ON oa.ID = c.OrgArtistID
         LEFT JOIN djmdArtist cp ON cp.ID = c.ComposerID
         LEFT JOIN djmdGenre g ON g.ID = c.GenreID
         LEFT JOIN djmdLabel l ON l.ID = c.LabelID
         ORDER BY c.ID",
    )?;
    let rows = statement.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Each playlist's members in order, as a JSON array of track IDs.
fn playlist_members(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut statement = conn.prepare(
        "SELECT CAST(p.ID AS TEXT), json((SELECT json_group_array(ContentID) FROM (
             SELECT CAST(s.ContentID AS TEXT) AS ContentID FROM djmdSongPlaylist s
             WHERE s.PlaylistID = p.ID AND s.rb_local_deleted = 0 ORDER BY s.TrackNo, s.ID)))
         FROM djmdPlaylist p ORDER BY p.ID",
    )?;
    let rows = statement.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

fn ids_and_uuids(conn: &Connection, table: &str) -> Result<Vec<(String, Option<String>)>> {
    let mut statement = conn.prepare(&format!("SELECT CAST(ID AS TEXT), UUID FROM {table} ORDER BY ID"))?;
    let rows = statement.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

const IMPORT_SCHEMA: &str = "
    CREATE TABLE imports (
        id INTEGER PRIMARY KEY, kind TEXT NOT NULL, started_at TEXT NOT NULL, finished_at TEXT,
        source_db TEXT NOT NULL, source_sha256 TEXT NOT NULL, source_unchanged INTEGER,
        tracks INTEGER, playlists INTEGER, cues INTEGER, history_sessions INTEGER,
        files_copied INTEGER, bytes_copied INTEGER, problems INTEGER, log TEXT);
    CREATE TABLE track_map (rb_id TEXT PRIMARY KEY, rb_uuid TEXT, rbx_id TEXT NOT NULL UNIQUE,
        import_id INTEGER NOT NULL REFERENCES imports(id));
    CREATE TABLE track_baseline (rbx_id TEXT PRIMARY KEY, fields TEXT NOT NULL,
        import_id INTEGER NOT NULL REFERENCES imports(id));
    CREATE TABLE playlist_map (rb_id TEXT PRIMARY KEY, rb_uuid TEXT, rbx_id TEXT NOT NULL UNIQUE,
        import_id INTEGER NOT NULL REFERENCES imports(id));
    CREATE TABLE playlist_baseline (rbx_id TEXT PRIMARY KEY, members TEXT NOT NULL,
        import_id INTEGER NOT NULL REFERENCES imports(id));
    CREATE TABLE history_map (rb_id TEXT PRIMARY KEY, rb_uuid TEXT, rbx_id TEXT NOT NULL UNIQUE,
        import_id INTEGER NOT NULL REFERENCES imports(id));
";

/// Opens `import.db` beside a library, making it if it is not there.
pub fn open_import_db(library_dir: &Path) -> Result<Connection> {
    let conn = Connection::open(library_dir.join(IMPORT_DB))?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version == 0 {
        conn.execute_batch(IMPORT_SCHEMA)?;
        conn.pragma_update(None, "user_version", IMPORT_DB_VERSION)?;
    } else if version != IMPORT_DB_VERSION {
        return Err(DbError::Import(format!("{IMPORT_DB} is version {version}, which this rbxport does not know")));
    }
    Ok(conn)
}

/// Records the first import in the staged library's `import.db`: every row
/// is mapped to itself (the first import keeps rekordbox's IDs), with each
/// track's and playlist's baseline. Returns the import's ID.
fn record(staged: &LibraryLocation, source_db: &Path, source_sha256: &str, started_at: &str) -> Result<i64> {
    let library = Library::open(staged.clone(), OpenMode::ReadOnly)?;
    let conn = library.connection();
    let tracks = ids_and_uuids(conn, "djmdContent")?;
    let playlists = ids_and_uuids(conn, "djmdPlaylist")?;
    let histories = ids_and_uuids(conn, "djmdHistory")?;
    let baselines = tag_fields(conn)?;
    let members = playlist_members(conn)?;
    drop(library);

    let dir = staged.master_db.parent().unwrap_or_else(|| Path::new("."));
    let mut db = open_import_db(dir)?;
    let tx = db.transaction()?;
    tx.execute(
        "INSERT INTO imports (kind, started_at, source_db, source_sha256) VALUES ('first', ?1, ?2, ?3)",
        params![started_at, source_db.display().to_string(), source_sha256],
    )?;
    let import_id = tx.last_insert_rowid();
    for (table, rows) in [("track_map", &tracks), ("playlist_map", &playlists), ("history_map", &histories)] {
        let mut insert = tx.prepare(&format!("INSERT INTO {table} (rb_id, rb_uuid, rbx_id, import_id) VALUES (?1, ?2, ?1, ?3)"))?;
        for (id, uuid) in rows {
            insert.execute(params![id, uuid, import_id])?;
        }
    }
    for (table, column, rows) in [("track_baseline", "fields", &baselines), ("playlist_baseline", "members", &members)] {
        let mut insert = tx.prepare(&format!("INSERT INTO {table} (rbx_id, {column}, import_id) VALUES (?1, ?2, ?3)"))?;
        for (id, value) in rows {
            insert.execute(params![id, value, import_id])?;
        }
    }
    tx.commit()?;
    Ok(import_id)
}

fn finish_record(staged: &LibraryLocation, import_id: i64, summary: &Summary, log_name: &str) -> Result<()> {
    let dir = staged.master_db.parent().unwrap_or_else(|| Path::new("."));
    let db = open_import_db(dir)?;
    let n = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    db.execute(
        "UPDATE imports SET finished_at = ?2, source_unchanged = ?3, tracks = ?4, playlists = ?5, cues = ?6,
             history_sessions = ?7, files_copied = ?8, bytes_copied = ?9, problems = ?10, log = ?11
         WHERE id = ?1",
        params![
            import_id,
            rbl_core::time::now(),
            summary.source_unchanged,
            n(summary.tracks),
            n(summary.playlists),
            n(summary.cues),
            n(summary.history_sessions),
            n(summary.files_copied),
            n(summary.bytes_copied),
            n(u64::try_from(summary.problems.len()).unwrap_or(u64::MAX)),
            format!("{LOG_DIR}/{log_name}"),
        ],
    )?;
    Ok(())
}

/// Writes the import log into the staged library and returns its name.
fn write_log(
    staging: &Path,
    source: &LibraryLocation,
    target_dir: &Path,
    summary: &Summary,
    after: &str,
    started_at: &str,
) -> Result<String> {
    let mut text = String::new();
    let _ = writeln!(text, "rbxport: first import from rekordbox");
    let _ = writeln!(text, "Started:  {started_at}");
    let _ = writeln!(text, "Finished: {}", rbl_core::time::now());
    let _ = writeln!(text, "From rekordbox's library: {}", source.master_db.display());
    let _ = writeln!(text, "Into rbxport's library:   {}", target_dir.display());
    let _ = writeln!(text, "rekordbox's master.db SHA-256 when copied:       {}", summary.source_sha256);
    let _ = writeln!(text, "rekordbox's master.db SHA-256 after the import:  {after}");
    let _ = writeln!(
        text,
        "rekordbox's library unchanged: {}",
        if summary.source_unchanged { "yes" } else { "NO: rekordbox changed it after it was copied; the copy is still whole" }
    );
    let _ = writeln!(text, "Tracks: {}", summary.tracks);
    let _ = writeln!(text, "Playlists: {}", summary.playlists);
    let _ = writeln!(text, "Cues: {}", summary.cues);
    let _ = writeln!(text, "History sessions: {}", summary.history_sessions);
    let _ = writeln!(text, "Analysis and artwork files copied: {} ({})", summary.files_copied, human_bytes(summary.bytes_copied));
    let _ = writeln!(text, "Problems: {}", summary.problems.len());
    for problem in &summary.problems {
        let _ = writeln!(text, "  - {problem}");
    }
    let dir = staging.join(LOG_DIR);
    std::fs::create_dir_all(&dir)?;
    let name = format!("{}-first-import.txt", rbl_core::time::local_backup_stamp());
    std::fs::write(dir.join(&name), text)?;
    Ok(name)
}

/// Makes way for the library: an empty folder is removed, and a folder with
/// anything in it (but no `master.db`) is moved aside, never deleted.
fn clear_target(target_dir: &Path) -> Result<Option<PathBuf>> {
    if !target_dir.exists() {
        return Ok(None);
    }
    if target_dir.join("master.db").exists() {
        return Err(DbError::Import(format!(
            "a library appeared at {} during the import; it was left as it is",
            target_dir.display()
        )));
    }
    if std::fs::read_dir(target_dir)?.next().is_none() {
        std::fs::remove_dir(target_dir)?;
        return Ok(None);
    }
    let name = target_dir.file_name().map_or_else(|| "library".into(), |n| n.to_string_lossy().into_owned());
    let stamp = rbl_core::time::local_backup_stamp();
    let mut aside = target_dir.with_file_name(format!("{name}.before-import-{stamp}"));
    let mut n = 2;
    while aside.exists() {
        aside = target_dir.with_file_name(format!("{name}.before-import-{stamp}-{n}"));
        n += 1;
    }
    std::fs::rename(target_dir, &aside)?;
    Ok(Some(aside))
}

/// `1.3 GB`, `18.0 MB`, `512 bytes`.
#[must_use]
pub fn human_bytes(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss, reason = "a size shown to one decimal")]
    let value = bytes as f64;
    match bytes {
        0..1_000 => format!("{bytes} bytes"),
        1_000..1_000_000 => format!("{:.1} KB", value / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1} MB", value / 1e6),
        _ => format!("{:.2} GB", value / 1e9),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use rusqlite::types::Value;

    struct Setup {
        _root: tempfile::TempDir,
        source: LibraryLocation,
        target: PathBuf,
    }

    /// A fixture standing in for rekordbox's library, with analysis files,
    /// artwork, a cue, an artist and a My Tag; and where rbxport's would go.
    fn setup() -> Setup {
        let root = tempfile::tempdir().unwrap();
        let rb = root.path().join("rekordbox");
        std::fs::create_dir_all(&rb).unwrap();
        let source = crate::fixture::build(&rb, crate::fixture::Shape::default()).unwrap();
        let conn = Connection::open(&source.master_db).unwrap();
        conn.pragma_update(None, "cipher", "sqlcipher").unwrap();
        conn.pragma_update(None, "legacy", 4).unwrap();
        conn.pragma_update(None, "key", &source.passphrase).unwrap();
        conn.execute_batch(&format!(
            "INSERT INTO djmdArtist (ID, Name, created_at, updated_at) VALUES ('77', 'Somebody', 'x', 'x');
             UPDATE djmdContent SET ArtistID = '77', Commnt = 'a comment', AnalysisDataPath = '/PIONEER/USBANLZ/P001/0001/ANLZ0000.DAT'
                 WHERE ID = '{t0}';
             INSERT INTO djmdSongMyTag (ID, MyTagID, ContentID, created_at, updated_at)
                 VALUES ('m1', '{peak}', '{t0}', 'x', 'x');
             INSERT INTO djmdCue (ID, ContentID, InMsec, Kind, created_at, updated_at) VALUES ('c1', '{t0}', 1000, 1, 'x', 'x');",
            t0 = crate::fixture::track_id(0),
            peak = crate::fixture::MY_TAG_PEAK,
        ))
        .unwrap();
        drop(conn);
        for (path, bytes) in [
            ("PIONEER/USBANLZ/P001/0001/ANLZ0000.DAT", b"anlz-dat".as_slice()),
            ("PIONEER/USBANLZ/P001/0001/ANLZ0000.EXT", b"anlz-ext-longer"),
            ("PIONEER/Artwork/00001/a1.jpg", b"\xff\xd8 not really a jpeg"),
        ] {
            let at = source.share_root.join(path);
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            std::fs::write(at, bytes).unwrap();
        }
        let target = root.path().join("rbxport").join("library");
        Setup { _root: root, source, target }
    }

    fn import(s: &Setup) -> Result<Summary> {
        first_import(&s.source, &s.target, &mut |_| {})
    }

    /// Every file under `dir`, with its bytes.
    fn tree(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut files: Vec<_> = walkdir::WalkDir::new(dir)
            .into_iter()
            .map(std::result::Result::unwrap)
            .filter(|e| e.file_type().is_file())
            .map(|e| (e.path().strip_prefix(dir).unwrap().to_path_buf(), std::fs::read(e.path()).unwrap()))
            .collect();
        files.sort();
        files
    }

    /// Every row of every table, in rowid order.
    fn all_rows(location: &LibraryLocation) -> Vec<(String, Vec<Vec<Value>>)> {
        let library = Library::open(location.clone(), OpenMode::ReadOnly).unwrap();
        let conn = library.connection();
        let mut tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(std::result::Result::unwrap)
            .collect();
        tables.sort();
        tables
            .into_iter()
            .map(|table| {
                let mut statement = conn.prepare(&format!("SELECT * FROM `{table}` ORDER BY rowid")).unwrap();
                let width = statement.column_count();
                let rows = statement
                    .query_map([], |r| (0..width).map(|i| r.get::<_, Value>(i)).collect())
                    .unwrap()
                    .map(std::result::Result::unwrap)
                    .collect();
                (table, rows)
            })
            .collect()
    }

    #[test]
    fn the_import_matches_its_source_row_for_row_and_reads_only() {
        let s = setup();
        // An empty WAL beside the source, as SQLite leaves one, is fine.
        std::fs::write(s.source.master_db.with_extension("db-wal"), b"").unwrap();
        let source_dir = s.source.master_db.parent().unwrap().to_path_buf();
        let before = tree(&source_dir);
        let expected = all_rows(&s.source);

        let mut stages = Vec::new();
        let summary = first_import(&s.source, &s.target, &mut |p| stages.push(p.stage)).unwrap();

        assert_eq!(tree(&source_dir), before, "nothing of rekordbox's is changed, added or removed");
        assert!(summary.source_unchanged);
        assert_eq!(summary.source_sha256, sha256_file(&s.source.master_db).unwrap());
        assert_eq!(summary.location.master_db, s.target.join("master.db"));
        // Opened with rbxport's own passphrase, not the fixture's: rekeyed.
        let own = crate::own_location_in(&s.target).unwrap();
        assert_ne!(own.passphrase, s.source.passphrase);
        assert_eq!(all_rows(&own), expected, "every table, row for row");

        assert_eq!((summary.tracks, summary.playlists, summary.cues, summary.history_sessions), (40, 3, 1, 2));
        assert_eq!((summary.files_copied, summary.problems.len()), (3, 0));
        let pioneer = |dir: &Path| tree(&dir.join("PIONEER"));
        assert_eq!(pioneer(&s.target.join("share")), pioneer(&s.source.share_root));
        assert!(stages.contains(&Stage::CopyingFiles) && stages.last() == Some(&Stage::Finishing));

        let mut names: Vec<_> = std::fs::read_dir(&s.target)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, [LOG_DIR, IMPORT_DB, "master.db", "share"], "no journal or staging left behind");
        let siblings: Vec<_> = std::fs::read_dir(s.target.parent().unwrap()).unwrap().collect();
        assert_eq!(siblings.len(), 1, "the staging folder is gone");
        let log = std::fs::read_to_string(&summary.log).unwrap();
        assert!(log.contains("rekordbox's library unchanged: yes"), "{log}");
        assert!(!log.contains(&s.source.passphrase), "no passphrase in the log");
    }

    #[test]
    fn import_db_maps_every_row_to_itself_with_its_baseline() {
        let s = setup();
        import(&s).unwrap();
        let db = open_import_db(&s.target).unwrap();
        let one = |sql: &str| -> i64 { db.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(one("SELECT COUNT(*) FROM track_map WHERE rb_id = rbx_id"), 40);
        assert_eq!(one("SELECT COUNT(*) FROM track_baseline"), 40);
        assert_eq!(one("SELECT COUNT(*) FROM playlist_map"), 3);
        assert_eq!(one("SELECT COUNT(*) FROM history_map"), one("SELECT COUNT(*) FROM history_map WHERE rb_id = rbx_id"));
        assert_eq!(one("SELECT COUNT(*) FROM imports WHERE kind = 'first' AND finished_at IS NOT NULL AND source_unchanged = 1"), 1);

        let t0 = crate::fixture::track_id(0);
        let fields: String = db
            .query_row("SELECT fields FROM track_baseline WHERE rbx_id = ?1", [&t0], |r| r.get(0))
            .unwrap();
        let fields: serde_json::Value = serde_json::from_str(&fields).unwrap();
        assert_eq!(fields["title"], "Track 000");
        assert_eq!(fields["artist"], "Somebody");
        assert_eq!(fields["comment"], "a comment");
        assert_eq!(fields["my_tags"], serde_json::json!(["Peak"]));
        assert_eq!(fields["album"], serde_json::Value::Null);
        let uuid: String = db.query_row("SELECT rb_uuid FROM track_map WHERE rb_id = ?1", [&t0], |r| r.get(0)).unwrap();
        assert!(uuid.starts_with("fixture-content-"));

        let members: String = db
            .query_row("SELECT members FROM playlist_baseline WHERE rbx_id = ?1", [crate::fixture::playlist_id(0)], |r| r.get(0))
            .unwrap();
        let members: Vec<String> = serde_json::from_str(&members).unwrap();
        assert_eq!(members, (0..5).map(crate::fixture::track_id).collect::<Vec<_>>());

        // What a later import would compare with reads the same from the library.
        let library = Library::open(crate::own_location_in(&s.target).unwrap(), OpenMode::ReadOnly).unwrap();
        let now = tag_fields(library.connection()).unwrap();
        let recorded: Vec<(String, String)> = db
            .prepare("SELECT rbx_id, fields FROM track_baseline ORDER BY rbx_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(std::result::Result::unwrap)
            .collect();
        assert_eq!(now, recorded);
    }

    #[test]
    fn the_imported_library_is_writable_as_rbxport_s() {
        let s = setup();
        let summary = import(&s).unwrap();
        let mut location = summary.location;
        location.is_user_library = false; // a temp folder; the test gate would refuse the real one
        let backups = tempfile::tempdir().unwrap();
        let mut writer = crate::write::Writer::open(location, backups.path()).unwrap();
        writer.create_playlist("Made in rbxport", "root").unwrap();
    }

    #[test]
    fn an_existing_library_is_never_imported_over() {
        let s = setup();
        std::fs::create_dir_all(&s.target).unwrap();
        std::fs::write(s.target.join("master.db"), b"rbxport's own").unwrap();
        assert!(matches!(import(&s), Err(DbError::Import(_))));
        assert_eq!(std::fs::read(s.target.join("master.db")).unwrap(), b"rbxport's own");
        assert_eq!(std::fs::read_dir(&s.target).unwrap().count(), 1);
    }

    #[test]
    fn unsaved_changes_beside_rekordbox_s_library_are_refused() {
        let s = setup();
        std::fs::write(s.source.master_db.with_extension("db-wal"), b"frames").unwrap();
        let err = import(&s).unwrap_err().to_string();
        assert!(err.contains("close rekordbox"), "{err}");
        assert!(!s.target.exists());
        let parent = s.target.parent().unwrap();
        assert!(!parent.exists() || std::fs::read_dir(parent).unwrap().next().is_none(), "no staging left behind");
    }

    #[test]
    fn a_library_inside_rekordbox_s_folder_is_refused() {
        let s = setup();
        let inside = s.source.master_db.parent().unwrap().join("rbxport");
        assert!(matches!(first_import(&s.source, &inside, &mut |_| {}), Err(DbError::WriteRefused(_))));
        assert!(!inside.exists());
    }

    #[test]
    fn a_folder_in_the_library_s_place_is_moved_aside_not_deleted() {
        let s = setup();
        std::fs::create_dir_all(s.target.join("share")).unwrap();
        std::fs::write(s.target.join("notes.txt"), b"keep me").unwrap();
        let summary = import(&s).unwrap();
        let aside = summary.set_aside.expect("moved aside");
        assert_eq!(std::fs::read(aside.join("notes.txt")).unwrap(), b"keep me");
        assert!(s.target.join("master.db").is_file());
    }

    #[test]
    fn a_copy_that_changed_is_taken_again_and_one_that_keeps_changing_fails() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("master.db");
        std::fs::write(&source, b"version 1").unwrap();
        let dest = root.path().join("copy.db");

        let hash = copy_verified(&source, &dest, &mut |attempt| {
            if attempt == 1 {
                std::fs::write(&source, b"version 2").unwrap();
            }
        })
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"version 2");
        assert_eq!(hash, sha256_file(&source).unwrap());

        std::fs::remove_file(&dest).unwrap();
        let mut n = 0;
        let err = copy_verified(&source, &dest, &mut |_| {
            n += 1;
            std::fs::write(&source, format!("version {}", n + 2)).unwrap();
        })
        .unwrap_err();
        assert_eq!(err.to_string(), CHANGED_WHILE_COPYING);
        assert_eq!(n, COPY_ATTEMPTS);
    }

    #[test]
    fn sizes_read_as_people_say_them() {
        assert_eq!(human_bytes(512), "512 bytes");
        assert_eq!(human_bytes(18_000_000), "18.0 MB");
        assert_eq!(human_bytes(1_260_000_000), "1.26 GB");
    }
}
