//! The identify store: results, proposals, review state, original-tag
//! snapshots and fetched artwork, in a SQLite file owned by this feature.
//!
//! It is never rekordbox's `master.db`: [`Store::open`] refuses a file that
//! is not an identify store, and nothing here knows a passphrase.
//!
//! The heart of it is undo. A track's ORIGINAL [`TagSnapshot`] is recorded
//! once, just before the first change is written to the file
//! ([`Store::save_original_snapshot`]), and is immutable afterwards:
//! identifying the track again, applying or undoing a change, or the file
//! changing on disk never replaces it. Two layers hold that: the code only
//! ever inserts a snapshot when none exists, and triggers make the database
//! itself refuse any UPDATE or DELETE of one.
//!
//! Snapshots are taken only for tracks about to be written, not for every
//! track scanned: the raw ID3 block includes embedded artwork, and a whole
//! library of those would run to gigabytes. They are stored as JSON for the
//! parsed values and as BLOBs for the raw ID3 bytes (not base64).

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::error::{IdentifyError, Result};

/// The schema this build creates and understands, kept in
/// `PRAGMA user_version`. A newer file is refused rather than guessed at.
pub const SCHEMA_VERSION: i64 = 1;

/// `PRAGMA application_id` of an identify store ("RBID"), so a SQLite file
/// that belongs to something else is recognised and refused.
const APPLICATION_ID: i64 = 0x5242_4944;

/// How long a write waits for another connection's lock before failing.
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

const SCHEMA_V1: &str = r"
CREATE TABLE tracks (
    id             INTEGER PRIMARY KEY,
    -- Exactly as the caller gave it (backslashes, case, non-ASCII).
    path           TEXT    NOT NULL UNIQUE,
    file_size      INTEGER NOT NULL,
    modified_unix  INTEGER NOT NULL,
    duration_secs  REAL    NOT NULL,
    fingerprint    TEXT,
    -- ExistingTags as last read, JSON.
    existing_json  TEXT    NOT NULL,
    created_unix   INTEGER NOT NULL,
    updated_unix   INTEGER NOT NULL
);

-- The ORIGINAL tags of a track, taken before its first write. One row per
-- track, inserted once, never updated or deleted (see the triggers).
CREATE TABLE snapshots (
    track_id       INTEGER PRIMARY KEY REFERENCES tracks(id),
    -- TagSnapshot without raw_id3 (format, values, has_artwork), JSON.
    snapshot_json  TEXT    NOT NULL,
    -- raw_id3: whether it was Some, then its parts as raw bytes (the model
    -- carries them as base64). NULL part = None.
    has_raw_id3    INTEGER NOT NULL CHECK (has_raw_id3 IN (0, 1)),
    raw_id3_v2     BLOB,
    raw_id3_v1     BLOB,
    -- The file as it was when the snapshot was taken.
    file_size      INTEGER NOT NULL,
    modified_unix  INTEGER NOT NULL,
    taken_unix     INTEGER NOT NULL
);
CREATE TRIGGER snapshots_never_update BEFORE UPDATE ON snapshots
BEGIN SELECT RAISE(ABORT, 'an original tag snapshot is immutable'); END;
CREATE TRIGGER snapshots_never_delete BEFORE DELETE ON snapshots
BEGIN SELECT RAISE(ABORT, 'an original tag snapshot is immutable'); END;

-- Every identification of a track. The newest is current; older ones stay
-- as history.
CREATE TABLE results (
    id               INTEGER PRIMARY KEY,
    track_id         INTEGER NOT NULL REFERENCES tracks(id),
    is_current       INTEGER NOT NULL CHECK (is_current IN (0, 1)),
    decision         TEXT    NOT NULL
        CHECK (decision IN ('AutoApply', 'Review', 'Unidentified')),
    state            TEXT    NOT NULL
        CHECK (state IN ('Unidentified', 'Pending', 'Accepted', 'Rejected',
                         'Applied', 'AutoApplied', 'Undone')),
    -- The best candidate's score; NULL without candidates.
    score            REAL,
    candidates_json  TEXT    NOT NULL,
    proposal_json    TEXT,
    enrichment_json  TEXT,
    warnings_json    TEXT    NOT NULL,
    created_unix     INTEGER NOT NULL,
    updated_unix     INTEGER NOT NULL
);
CREATE UNIQUE INDEX results_one_current ON results(track_id) WHERE is_current = 1;
CREATE INDEX results_current_state ON results(state, score) WHERE is_current = 1;

-- Each time a result's changes were written to the file: exactly what was
-- written (the user may have unticked fields). A re-apply after an undo adds
-- a row rather than replacing one.
CREATE TABLE applications (
    id            INTEGER PRIMARY KEY,
    result_id     INTEGER NOT NULL REFERENCES results(id),
    -- Vec<FieldChange>, JSON.
    changes_json  TEXT    NOT NULL,
    -- ArtworkRef, JSON; NULL when no artwork was written.
    artwork_json  TEXT,
    -- The file right after the write, so a later change by something else
    -- can be told apart before undoing.
    file_size_after      INTEGER NOT NULL,
    modified_unix_after  INTEGER NOT NULL,
    -- The library side of the change (rekordbox content ID and previous
    -- values), JSON the store keeps without reading. NULL when none.
    library_snapshot     TEXT,
    applied_unix  INTEGER NOT NULL
);
CREATE INDEX applications_result ON applications(result_id);

-- Fetched artwork, deduplicated by content hash.
CREATE TABLE artwork (
    sha256        TEXT    PRIMARY KEY,
    mime          TEXT    NOT NULL,
    bytes         BLOB    NOT NULL,
    created_unix  INTEGER NOT NULL
);
";

/// The identify store. One connection; not shared across threads (move it
/// to the worker that uses it).
pub struct Store {
    conn: Connection,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").field("path", &self.conn.path()).finish()
    }
}

impl Store {
    /// Opens the store at `path`, creating it (and its schema) when the file
    /// does not exist. Refuses a file that is not an identify store and one
    /// whose schema is newer than this build.
    pub fn open(path: &Path) -> Result<Self> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path, flags)
            .map_err(|e| sql("open the identify store", &e))?;
        Self::init(conn, true)
    }

    /// A store that lives only as long as the value: for tests.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(|e| sql("open an in-memory store", &e))?;
        Self::init(conn, false)
    }

    fn init(conn: Connection, on_disk: bool) -> Result<Self> {
        conn.busy_timeout(BUSY_TIMEOUT).map_err(|e| sql("set the busy timeout", &e))?;
        // Look before changing anything: a foreign or newer file is left as
        // it was found. Reading also proves the file is plain SQLite (an
        // encrypted master.db fails right here).
        let version: i64 =
            conn.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(|e| sql("read the schema version", &e))?;
        let application_id: i64 =
            conn.query_row("PRAGMA application_id", [], |r| r.get(0)).map_err(|e| sql("read the application id", &e))?;
        let objects: i64 =
            conn.query_row("SELECT COUNT(*) FROM sqlite_master", [], |r| r.get(0)).map_err(|e| sql("read the schema", &e))?;
        if application_id != APPLICATION_ID && (application_id != 0 || objects != 0) {
            return Err(IdentifyError::Store("the file is not an identify store".to_owned()));
        }
        if version > SCHEMA_VERSION {
            return Err(IdentifyError::Store(format!(
                "the identify store has schema version {version}, newer than the {SCHEMA_VERSION} this build understands; \
                 update rbxport or remove the store"
            )));
        }

        if on_disk {
            let mode: String =
                conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0)).map_err(|e| sql("enable WAL", &e))?;
            if !mode.eq_ignore_ascii_case("wal") {
                return Err(IdentifyError::Store(format!("could not enable WAL journaling (got {mode})")));
            }
        }
        // FULL: in WAL mode a commit is durable once it returns, which undo
        // depends on (a snapshot must not vanish in a power cut).
        conn.pragma_update(None, "synchronous", "FULL").map_err(|e| sql("set synchronous", &e))?;
        conn.pragma_update(None, "foreign_keys", true).map_err(|e| sql("enable foreign keys", &e))?;

        let store = Self { conn };
        store.migrate(version)?;
        Ok(store)
    }

    fn migrate(&self, from: i64) -> Result<()> {
        if from == SCHEMA_VERSION {
            return Ok(());
        }
        let tx = self.conn.unchecked_transaction().map_err(|e| sql("begin", &e))?;
        if from == 0 {
            tx.execute_batch(SCHEMA_V1).map_err(|e| sql("create the schema", &e))?;
        }
        tx.pragma_update(None, "application_id", APPLICATION_ID).map_err(|e| sql("set the application id", &e))?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION).map_err(|e| sql("set the schema version", &e))?;
        tx.commit().map_err(|e| sql("commit the schema", &e))
    }

    /// The schema version of the open store.
    pub fn schema_version(&self) -> Result<i64> {
        self.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(|e| sql("read the schema version", &e))
    }

    /// Whether a table exists; for tests of the schema.
    #[cfg(test)]
    fn has_table(&self, name: &str) -> Result<bool> {
        use rusqlite::OptionalExtension;
        self.conn
            .query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1", [name], |_| Ok(()))
            .optional()
            .map(|r| r.is_some())
            .map_err(|e| sql("read the schema", &e))
    }
}

/// A SQLite error as a store error, saying what was being done. Never
/// carries a bound value (a path or tag text): SQLite's own messages name
/// tables and columns, not values, and the other variants are reduced to
/// their kind.
fn sql(doing: &str, e: &rusqlite::Error) -> IdentifyError {
    let reason = match e {
        rusqlite::Error::SqliteFailure(code, Some(message)) => format!("{message} ({:?})", code.code),
        rusqlite::Error::SqliteFailure(code, None) => code.to_string(),
        rusqlite::Error::QueryReturnedNoRows => "no such row".to_owned(),
        rusqlite::Error::InvalidColumnType(_, name, ty) => format!("column {name} holds {ty}"),
        rusqlite::Error::InvalidColumnIndex(i) => format!("no column {i}"),
        rusqlite::Error::InvalidColumnName(name) => format!("no column {name}"),
        _ => "unexpected SQLite error".to_owned(),
    };
    IdentifyError::Store(format!("could not {doing}: {reason}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_new_store_has_the_schema() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        for table in ["tracks", "snapshots", "results", "applications", "artwork"] {
            assert!(store.has_table(table).unwrap(), "{table}");
        }
    }
}
