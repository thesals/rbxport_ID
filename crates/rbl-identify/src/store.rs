//! The identify store: results, proposals, review state, original-tag
//! snapshots and fetched artwork, in a SQLite file owned by this feature.
//!
//! It is never rekordbox's `master.db`: [`Store::open`] refuses a file that
//! is not an identify store, and nothing here knows a passphrase.
//!
//! The heart of it is undo. Every write to a file is recorded in two steps
//! around the write itself: [`Store::begin_application`] commits what will
//! be written together with the tags the file has just before it (the
//! `before` snapshot) BEFORE the file is touched, and
//! [`Store::complete_application`] or [`Store::abandon_application`] records
//! how it ended. A crash or a full disk between the two leaves an open
//! application whose before-state is already safe ([`Store::open_applications`]
//! lists them for reconciliation), never a write that cannot be undone.
//! Undoing a write restores exactly what that write replaced
//! ([`Store::application_snapshot`]), hand edits made between two writes
//! included. The track's ORIGINAL tags are the before-snapshot of its
//! earliest written application ([`Store::original_snapshot`]).
//!
//! Applications, their snapshots and their outcomes are insert-only:
//! triggers make the database itself refuse any UPDATE, DELETE or second
//! INSERT of one.
//!
//! Version 2 adds the rekordbox library track each result was identified
//! for ([`LibraryTrack`]), the file's tags right after each write (what undo
//! checks the file still holds before restoring) with the library values
//! written, and a permanent record of each undo. A version 1 store is
//! migrated in place on open; its results simply have no library track.
//!
//! Version 3 adds a read-only import of the rekordbox library record each
//! result was identified for ([`RekordboxRecord`]: title, artist, album,
//! label, genre, year, comment and ISRC as the library held them), saved
//! with the result. Identification never writes to the library; this copy
//! is how its data reaches the identify store. The library-values columns
//! of version 2 (`library_snapshot`, `library_after`) are kept for writes
//! an older build recorded, but nothing writes them any more.
//!
//! Version 4 adds waiting writes ([`WaitingWrite`]): a write refused
//! because another program had the file open is kept, with the fields to
//! write, and made once the file is free. Its result stays `Accepted`
//! meanwhile. Like every record of a write it is insert-only: a waiting
//! write and the row saying how it ended are never changed.
//!
//! Snapshots are taken only for writes, not for every track scanned: the raw
//! ID3 block includes embedded artwork, and a whole library of those would
//! run to gigabytes. They are stored as JSON for the parsed values and as
//! BLOBs for the raw ID3 bytes (not base64).

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{IdentifyError, Result};
use crate::model::{
    ArtworkRef, CandidateSource, Decision, Enrichment, ExistingTags, Field, FieldChange, FrameValues, Proposal, RawId3, Scored,
    TagSnapshot,
};
use crate::pipeline::TrackResult;

/// The schema this build creates and understands, kept in
/// `PRAGMA user_version`. A newer file is refused rather than guessed at.
pub const SCHEMA_VERSION: i64 = 4;

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

-- Each write of a result's changes, begun before the file is touched:
-- exactly what is to be written (the user may have unticked fields). How it
-- ended is in application_outcomes; none yet means it is still open. A
-- re-apply after an undo adds a row rather than replacing one.
CREATE TABLE applications (
    id            INTEGER PRIMARY KEY,
    result_id     INTEGER NOT NULL REFERENCES results(id),
    -- Vec<FieldChange>, JSON.
    changes_json  TEXT    NOT NULL,
    -- ArtworkRef, JSON; NULL when no artwork is written.
    artwork_json  TEXT,
    -- The library side of the change (rekordbox content ID and previous
    -- values), JSON the store keeps without reading. NULL when none.
    library_snapshot     TEXT,
    begun_unix    INTEGER NOT NULL
);
CREATE INDEX applications_result ON applications(result_id);
-- What was written, and so what undo works from, is never rewritten.
CREATE TRIGGER applications_never_update BEFORE UPDATE ON applications
BEGIN SELECT RAISE(ABORT, 'an application is a permanent record'); END;
CREATE TRIGGER applications_never_delete BEFORE DELETE ON applications
BEGIN SELECT RAISE(ABORT, 'an application is a permanent record'); END;
-- INSERT OR REPLACE deletes the old row without firing DELETE triggers
-- (unless recursive_triggers is on), so these insert-only tables refuse a
-- second insert of a key here, whatever its conflict clause.
CREATE TRIGGER applications_never_replace BEFORE INSERT ON applications
WHEN EXISTS (SELECT 1 FROM applications WHERE id = NEW.id)
BEGIN SELECT RAISE(ABORT, 'an application is a permanent record'); END;

-- How an application ended: 'written' (the file now holds the changes) or
-- 'abandoned' (the file was left unchanged or put back). One row each,
-- never updated or deleted.
CREATE TABLE application_outcomes (
    application_id       INTEGER PRIMARY KEY REFERENCES applications(id),
    outcome              TEXT    NOT NULL CHECK (outcome IN ('written', 'abandoned')),
    -- The file right after a write, so a later change by something else can
    -- be told apart before undoing. NULL when abandoned.
    file_size_after      INTEGER,
    modified_unix_after  INTEGER,
    -- Why it was abandoned. NULL when written.
    reason               TEXT,
    completed_unix       INTEGER NOT NULL,
    CHECK ((outcome = 'written') = (file_size_after IS NOT NULL AND modified_unix_after IS NOT NULL))
);
CREATE TRIGGER application_outcomes_never_update BEFORE UPDATE ON application_outcomes
BEGIN SELECT RAISE(ABORT, 'an application outcome is a permanent record'); END;
CREATE TRIGGER application_outcomes_never_delete BEFORE DELETE ON application_outcomes
BEGIN SELECT RAISE(ABORT, 'an application outcome is a permanent record'); END;
CREATE TRIGGER application_outcomes_never_replace BEFORE INSERT ON application_outcomes
WHEN EXISTS (SELECT 1 FROM application_outcomes WHERE application_id = NEW.application_id)
BEGIN SELECT RAISE(ABORT, 'an application outcome is a permanent record'); END;

-- The file's tags just before each write, committed with the application
-- before the file is touched: what undoing that write restores. One row per
-- application, never updated or deleted. The track's original tags are the
-- snapshot of its earliest written application.
CREATE TABLE application_snapshots (
    application_id INTEGER PRIMARY KEY REFERENCES applications(id),
    -- TagSnapshot without raw_id3 (format, values, has_artwork), JSON.
    snapshot_json  TEXT    NOT NULL,
    -- raw_id3: whether it was Some, then its parts as raw bytes (the model
    -- carries them as base64). NULL part = None.
    has_raw_id3    INTEGER NOT NULL CHECK (has_raw_id3 IN (0, 1)),
    raw_id3_v2     BLOB,
    raw_id3_v1     BLOB,
    taken_unix     INTEGER NOT NULL
);
CREATE TRIGGER application_snapshots_never_update BEFORE UPDATE ON application_snapshots
BEGIN SELECT RAISE(ABORT, 'a tag snapshot is immutable'); END;
CREATE TRIGGER application_snapshots_never_delete BEFORE DELETE ON application_snapshots
BEGIN SELECT RAISE(ABORT, 'a tag snapshot is immutable'); END;
CREATE TRIGGER application_snapshots_never_replace BEFORE INSERT ON application_snapshots
WHEN EXISTS (SELECT 1 FROM application_snapshots WHERE application_id = NEW.application_id)
BEGIN SELECT RAISE(ABORT, 'a tag snapshot is immutable'); END;

-- Fetched artwork, deduplicated by content hash.
CREATE TABLE artwork (
    sha256        TEXT    PRIMARY KEY,
    mime          TEXT    NOT NULL,
    bytes         BLOB    NOT NULL,
    created_unix  INTEGER NOT NULL
);
";

/// Version 2, on top of version 1. Only columns and tables are added: every
/// insert-only rule of version 1 holds as it was.
const SCHEMA_V2: &str = r"
-- The rekordbox library track the result was identified for: its content
-- ID and its path as the library holds it. NULL for a version 1 result.
ALTER TABLE results ADD COLUMN library_track_id TEXT;
ALTER TABLE results ADD COLUMN library_path TEXT;

-- The file's tags right after a write (as application_snapshots holds the
-- before-state) and the library values written, JSON the store keeps
-- without reading. Inserted with the outcome, so as permanent as it is.
-- NULL for an abandoned application and for one written by version 1.
ALTER TABLE application_outcomes ADD COLUMN after_snapshot_json TEXT;
ALTER TABLE application_outcomes ADD COLUMN after_has_raw_id3 INTEGER;
ALTER TABLE application_outcomes ADD COLUMN after_raw_id3_v2 BLOB;
ALTER TABLE application_outcomes ADD COLUMN after_raw_id3_v1 BLOB;
ALTER TABLE application_outcomes ADD COLUMN library_after TEXT;

-- Each undo of a written application: one row, never updated or deleted.
CREATE TABLE application_undos (
    application_id INTEGER PRIMARY KEY REFERENCES applications(id),
    undone_unix    INTEGER NOT NULL
);
CREATE TRIGGER application_undos_never_update BEFORE UPDATE ON application_undos
BEGIN SELECT RAISE(ABORT, 'an undo is a permanent record'); END;
CREATE TRIGGER application_undos_never_delete BEFORE DELETE ON application_undos
BEGIN SELECT RAISE(ABORT, 'an undo is a permanent record'); END;
CREATE TRIGGER application_undos_never_replace BEFORE INSERT ON application_undos
WHEN EXISTS (SELECT 1 FROM application_undos WHERE application_id = NEW.application_id)
BEGIN SELECT RAISE(ABORT, 'an undo is a permanent record'); END;
";

/// Version 3, on top of version 2: the rekordbox record imported with each
/// result. Insert-only, like every record of a write.
const SCHEMA_V3: &str = r"
-- The rekordbox library record a result was identified for, as the library
-- held it when the job ran: read from the library, never written back. One
-- row per result (none for a result without a library track, or saved
-- before version 3), never updated or deleted. Absent values are ''.
CREATE TABLE rekordbox_imports (
    result_id     INTEGER PRIMARY KEY REFERENCES results(id),
    track_id      TEXT    NOT NULL,
    title         TEXT    NOT NULL,
    artist        TEXT    NOT NULL,
    album         TEXT    NOT NULL,
    label         TEXT    NOT NULL,
    genre         TEXT    NOT NULL,
    year          TEXT    NOT NULL,
    comment       TEXT    NOT NULL,
    isrc          TEXT    NOT NULL,
    imported_unix INTEGER NOT NULL
);
CREATE TRIGGER rekordbox_imports_never_update BEFORE UPDATE ON rekordbox_imports
BEGIN SELECT RAISE(ABORT, 'an imported rekordbox record is a permanent record'); END;
CREATE TRIGGER rekordbox_imports_never_delete BEFORE DELETE ON rekordbox_imports
BEGIN SELECT RAISE(ABORT, 'an imported rekordbox record is a permanent record'); END;
CREATE TRIGGER rekordbox_imports_never_replace BEFORE INSERT ON rekordbox_imports
WHEN EXISTS (SELECT 1 FROM rekordbox_imports WHERE result_id = NEW.result_id)
BEGIN SELECT RAISE(ABORT, 'an imported rekordbox record is a permanent record'); END;
";

/// Version 4, on top of version 3: waiting writes. Insert-only.
const SCHEMA_V4: &str = r"
-- A write refused because another program had the file open, kept to be
-- made once the file is free: the result and exactly the fields to write
-- (the ones the user ticked, or auto-apply chose). Its result stays Accepted
-- meanwhile. One row each, never updated or deleted; how it ended is in
-- waiting_write_ends, and none there means it is still waiting. A result has
-- at most one waiting write at a time.
CREATE TABLE waiting_writes (
    id           INTEGER PRIMARY KEY,
    result_id    INTEGER NOT NULL REFERENCES results(id),
    -- Vec<Field>, JSON.
    fields_json  TEXT    NOT NULL,
    -- Why it is waiting, in the user's terms.
    reason       TEXT    NOT NULL,
    since_unix   INTEGER NOT NULL
);
CREATE INDEX waiting_writes_result ON waiting_writes(result_id);
CREATE TRIGGER waiting_writes_never_update BEFORE UPDATE ON waiting_writes
BEGIN SELECT RAISE(ABORT, 'a waiting write is a permanent record'); END;
CREATE TRIGGER waiting_writes_never_delete BEFORE DELETE ON waiting_writes
BEGIN SELECT RAISE(ABORT, 'a waiting write is a permanent record'); END;
CREATE TRIGGER waiting_writes_never_replace BEFORE INSERT ON waiting_writes
WHEN EXISTS (SELECT 1 FROM waiting_writes WHERE id = NEW.id)
BEGIN SELECT RAISE(ABORT, 'a waiting write is a permanent record'); END;

-- How a waiting write ended: 'written' (made), 'dropped' (refused when it
-- was tried, or its result went back to review), 'cancelled' (by the user)
-- or 'superseded' (a newer waiting write for the same result). One row
-- each, never updated or deleted.
CREATE TABLE waiting_write_ends (
    waiting_id  INTEGER PRIMARY KEY REFERENCES waiting_writes(id),
    outcome     TEXT    NOT NULL CHECK (outcome IN ('written', 'dropped', 'cancelled', 'superseded')),
    reason      TEXT,
    ended_unix  INTEGER NOT NULL
);
CREATE TRIGGER waiting_write_ends_never_update BEFORE UPDATE ON waiting_write_ends
BEGIN SELECT RAISE(ABORT, 'a waiting write outcome is a permanent record'); END;
CREATE TRIGGER waiting_write_ends_never_delete BEFORE DELETE ON waiting_write_ends
BEGIN SELECT RAISE(ABORT, 'a waiting write outcome is a permanent record'); END;
CREATE TRIGGER waiting_write_ends_never_replace BEFORE INSERT ON waiting_write_ends
WHEN EXISTS (SELECT 1 FROM waiting_write_ends WHERE waiting_id = NEW.waiting_id)
BEGIN SELECT RAISE(ABORT, 'a waiting write outcome is a permanent record'); END;
";

/// A track's row id in the store.
pub type TrackId = i64;
/// An identification result's row id in the store.
pub type ResultId = i64;
/// The row id of one recorded write.
pub type ApplicationId = i64;
/// The row id of one waiting write.
pub type WaitingId = i64;

/// A write kept until its file is free ([`Store::defer_write`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitingWrite {
    pub id: WaitingId,
    pub result: ResultId,
    /// The file, as the track is keyed.
    pub path: String,
    /// Exactly the fields to write.
    pub fields: Vec<Field>,
    /// Why it is waiting, in the user's terms.
    pub reason: String,
    pub since_unix: i64,
}

/// How a waiting write ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitingEnd {
    Written,
    /// Refused when it was tried, or its result went back to review.
    Dropped,
    Cancelled,
    Superseded,
}

impl WaitingEnd {
    fn as_str(self) -> &'static str {
        match self {
            Self::Written => "written",
            Self::Dropped => "dropped",
            Self::Cancelled => "cancelled",
            Self::Superseded => "superseded",
        }
    }
}

/// Which file a track is, and the state it was in when read. Size and
/// modification time tell a changed file.
///
/// The path is the key and is stored exactly as given: no case folding, no
/// slash or `\\?\` normalisation, no resolving. `C:\Music\a.mp3` and
/// `c:/music/A.mp3` are two tracks to the store. Build keys with
/// [`TrackKey::for_file`], which canonicalises the path
/// ([`crate::paths::canonical_key`]), so one file always has one spelling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackKey {
    pub path: String,
    pub file_size: u64,
    pub modified_unix: i64,
}

impl TrackKey {
    /// The key of an existing file as it is now: its canonical path, size
    /// and modification time (whole seconds since the Unix epoch; 0 for a
    /// time before it).
    pub fn for_file(path: &Path) -> Result<Self> {
        let canonical = crate::paths::canonical_key(path)?;
        let meta = std::fs::metadata(&canonical)
            .map_err(|e| IdentifyError::Io(std::io::Error::new(e.kind(), format!("{}: {e}", path.display()))))?;
        let modified_unix = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        Ok(Self { path: canonical, file_size: meta.len(), modified_unix })
    }
}

/// The rekordbox library track a file was identified as: its content ID
/// (`djmdContent.ID`) and its path as the library holds it, which may be
/// spelled differently from the store's canonical key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryTrack {
    pub track_id: String,
    pub path: String,
}

/// A rekordbox library record as it was when a track was identified:
/// imported READ-ONLY from the library into the identify store, never
/// written back. Absent values are empty strings.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RekordboxRecord {
    /// `djmdContent.ID`.
    pub track_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub label: String,
    pub genre: String,
    /// The release year as text (`"2014"`); empty when unknown.
    pub year: String,
    pub comment: String,
    pub isrc: String,
    /// When it was read from the library, seconds since the Unix epoch.
    pub imported_unix: i64,
}

/// Where a result stands in the review flow. Stored as its variant name.
///
/// Allowed transitions ([`Store::set_state`] unless noted):
///
/// | from          | to                                            |
/// |---------------|-----------------------------------------------|
/// | `Pending`     | `Accepted`, `Rejected`                        |
/// | `Accepted`    | `Pending`, `Rejected`; `Applied` or `AutoApplied` only through [`Store::complete_application`] |
/// | `Rejected`    | `Pending`                                     |
/// | `Applied`     | `Undone`                                      |
/// | `AutoApplied` | `Undone`                                      |
/// | `Undone`      | `Pending`                                     |
/// | `Unidentified`| none: identify the track again instead        |
///
/// Everything else, including a state to itself, is refused. Only a
/// track's current result changes state.
///
/// A result the decision marked for auto-apply starts `Accepted`, like one
/// the user accepted; `complete_application` then makes it `AutoApplied` (the
/// stored decision tells the two apart). So a write that fails leaves it
/// `Accepted` with nothing recorded, and `Applied`/`AutoApplied` exist only
/// with the record of what was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ReviewState {
    /// No candidate was good enough; nothing to review.
    Unidentified,
    /// Waiting for the user.
    Pending,
    /// The user accepted it; not written yet.
    Accepted,
    Rejected,
    /// Written to the file, after review.
    Applied,
    /// Written to the file by auto-apply (empty fields only, user opted
    /// in), without review.
    AutoApplied,
    /// Was applied, and the original tags were put back.
    Undone,
}

impl ReviewState {
    const ALL: [Self; 7] =
        [Self::Unidentified, Self::Pending, Self::Accepted, Self::Rejected, Self::Applied, Self::AutoApplied, Self::Undone];

    /// The stored text: the variant name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unidentified => "Unidentified",
            Self::Pending => "Pending",
            Self::Accepted => "Accepted",
            Self::Rejected => "Rejected",
            Self::Applied => "Applied",
            Self::AutoApplied => "AutoApplied",
            Self::Undone => "Undone",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|state| state.as_str() == s)
            .ok_or_else(|| IdentifyError::Store("unknown review state in the store".to_owned()))
    }

    /// Whether [`Store::set_state`] may move a result from `self` to `to`.
    /// `Applied` and `AutoApplied` are never reached this way: only
    /// [`Store::complete_application`], which records the write, sets them.
    #[must_use]
    pub fn may_become(self, to: Self) -> bool {
        use ReviewState::{Accepted, Applied, AutoApplied, Pending, Rejected, Undone};
        matches!(
            (self, to),
            (Pending, Accepted | Rejected)
                | (Accepted, Pending | Rejected)
                | (Rejected | Undone, Pending)
                | (Applied | AutoApplied, Undone)
        )
    }

    /// Tags have been written for a result in this state.
    #[must_use]
    pub fn is_applied(self) -> bool {
        matches!(self, Self::Applied | Self::AutoApplied)
    }
}

/// The state a fresh result starts in.
fn initial_state(decision: Decision) -> ReviewState {
    match decision {
        // Not AutoApplied yet: nothing has been written.
        Decision::AutoApply => ReviewState::Accepted,
        Decision::Review => ReviewState::Pending,
        Decision::Unidentified => ReviewState::Unidentified,
    }
}

fn decision_str(decision: Decision) -> &'static str {
    match decision {
        Decision::AutoApply => "AutoApply",
        Decision::Review => "Review",
        Decision::Unidentified => "Unidentified",
    }
}

fn parse_decision(s: &str) -> Result<Decision> {
    match s {
        "AutoApply" => Ok(Decision::AutoApply),
        "Review" => Ok(Decision::Review),
        "Unidentified" => Ok(Decision::Unidentified),
        _ => Err(IdentifyError::Store("unknown decision in the store".to_owned())),
    }
}

/// A queue filter, as the review screen offers them. Each lists current
/// results only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Filter {
    Unidentified,
    /// `Pending`.
    NeedsReview,
    /// `Applied` and `AutoApplied`: the undoable ones.
    Applied,
    /// Results with a waiting write, newest first.
    Waiting,
    All,
}

impl Filter {
    /// The states the filter takes, as a SQL condition on `r.state`.
    fn condition(self) -> &'static str {
        match self {
            Self::Unidentified => "r.state = 'Unidentified'",
            Self::NeedsReview => "r.state = 'Pending'",
            Self::Applied => "r.state IN ('Applied', 'AutoApplied')",
            Self::Waiting => "w.id IS NOT NULL",
            Self::All => "1",
        }
    }

    /// The order the filter lists in.
    fn order(self) -> &'static str {
        match self {
            Self::Waiting => "w.since_unix DESC, w.id DESC",
            _ => "r.score IS NULL, r.score DESC, t.path, r.id",
        }
    }
}

/// One row of the review queue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueItem {
    pub track: TrackId,
    pub result: ResultId,
    pub path: String,
    pub state: ReviewState,
    pub decision: Decision,
    /// The best candidate's score; `None` without candidates.
    pub score: Option<f64>,
    /// The file's tags as last read.
    pub existing: ExistingTags,
    pub proposal: Option<Proposal>,
    /// When the result last changed.
    pub updated_unix: i64,
    /// The library track it was identified for; `None` for a version 1
    /// result.
    pub library: Option<LibraryTrack>,
    /// Where the proposed (best) candidate came from; `None` without a
    /// proposal.
    pub source: Option<CandidateSource>,
    /// The write waiting for the file to be free; `None` without one.
    pub waiting: Option<WaitingWrite>,
}

/// A result in full: its queue row, all its candidates (best first), the
/// Discogs enrichment and the warnings.
pub type ResultDetail = (QueueItem, Vec<Scored>, Option<Enrichment>, Vec<String>);

/// One write of a result's changes, from [`Store::begin_application`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Application {
    pub id: ApplicationId,
    pub result: ResultId,
    /// The file it writes, as the track is keyed.
    pub path: String,
    /// Exactly what is (or was) written.
    pub changes: Vec<FieldChange>,
    pub artwork: Option<ArtworkRef>,
    /// The library side of the change (rekordbox content ID and previous
    /// values), JSON the store keeps without reading. Only an older build,
    /// which also wrote the library, recorded one; undo ignores it.
    pub library_snapshot: Option<String>,
    pub begun_unix: i64,
    /// `None` while the application is open.
    pub outcome: Option<Outcome>,
    /// The library values the write left, JSON the store keeps without
    /// reading; recorded only by an older build, and ignored.
    pub library_after: Option<String>,
    /// When the write was undone; `None` if it has not been.
    pub undone_unix: Option<i64>,
}

/// How an application ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    /// The file holds the changes. `file_after` is the file right after the
    /// write: a different size or time later means something else changed
    /// it since, and undo must not blindly restore.
    Written { file_after: TrackKey, completed_unix: i64 },
    /// The file was left unchanged or put back; or, with a reason starting
    /// [`RECONCILED_UNKNOWN`], startup could not tell what an interrupted
    /// write left and closed it without touching the file.
    Abandoned { reason: String, completed_unix: i64 },
}

/// What [`Store::reconcile_open_applications`] made of one open application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reconciliation {
    /// The file still held exactly its before-snapshot: the write never
    /// happened, and the application is now abandoned.
    Abandoned { application: ApplicationId },
    /// The file held exactly what the write planned: it landed before its
    /// outcome was recorded, and is now recorded as written (and undoable).
    Completed { application: ApplicationId },
    /// The file was read and held neither (part of the changes, or someone
    /// else's): it cannot be told what happened, so the application is closed as
    /// unknown (abandoned with a [`RECONCILED_UNKNOWN`] reason) and its result
    /// returned to review, so the track is free again. The file is not
    /// touched; its before-snapshot stays in the store.
    Unknown { application: ApplicationId, reason: String },
    /// Left open, because it can be told later: the file is not there (a
    /// volume that is not mounted), it could not be read (held open by a
    /// player, rekordbox or a virus scanner, or a passing I/O error), or no
    /// snapshot was recorded. A file that opens but whose tags cannot be
    /// parsed is not left open: it is [`Reconciliation::Unknown`].
    NeedsUser { application: Box<Application>, reason: String },
}

/// The abandon reason recorded for an application reconciliation found
/// unchanged.
pub const RECONCILED_UNCHANGED: &str = "reconciled at startup: the file still held its tags from before the write";

/// How the abandon reason of an application closed as unknown begins.
pub const RECONCILED_UNKNOWN: &str = "unknown: reconciled at startup, the file held neither its tags from before the write nor \
     exactly the written ones";

/// The warning a result closed as unknown at startup carries.
pub const RECONCILED_UNKNOWN_WARNING: &str =
    "A write to this file was interrupted and what it left could not be told. Check the file's tags.";

/// The warning a result carries when an interrupted write's file could
/// not have its tags parsed at startup.
pub const RECONCILED_UNPARSEABLE_WARNING: &str =
    "A write to this file was interrupted, and the file's tags cannot be read now. Check the file's tags.";

/// Why a review change is refused while a write is waiting.
const WAITING_REFUSAL: &str = "A write to this file is waiting for the file to be closed in the other program. \
     Cancel the waiting write first.";

/// A file as reconciliation reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileNow {
    /// Its tags and its key (size, time) now.
    Read { tags: TagSnapshot, key: TrackKey },
    /// It is not there.
    Missing,
    /// Another program holds it open, so it cannot be read now.
    Locked(String),
    /// It opens, but its tags cannot be parsed.
    Unparseable(String),
    /// It is there but could not be read (a passing I/O error).
    Unreadable(String),
}

/// The columns [`application_row`] reads, from `applications a`,
/// `results r`, `tracks t` and `application_outcomes o` (left joined).
const APPLICATION_COLUMNS: &str = "a.id, a.result_id, t.path, a.changes_json, a.artwork_json, a.library_snapshot, \
     a.begun_unix, o.outcome, o.file_size_after, o.modified_unix_after, o.reason, o.completed_unix, o.library_after, \
     u.undone_unix";
const APPLICATION_FROM: &str = "applications a JOIN results r ON r.id = a.result_id JOIN tracks t ON t.id = r.track_id \
     LEFT JOIN application_outcomes o ON o.application_id = a.id \
     LEFT JOIN application_undos u ON u.application_id = a.id";

/// An application row as stored, before its text is decoded.
struct RawApplication {
    id: ApplicationId,
    result: ResultId,
    path: String,
    changes: String,
    artwork: Option<String>,
    library_snapshot: Option<String>,
    begun_unix: i64,
    outcome: Option<String>,
    file_size_after: Option<i64>,
    modified_unix_after: Option<i64>,
    reason: Option<String>,
    completed_unix: Option<i64>,
    library_after: Option<String>,
    undone_unix: Option<i64>,
}

fn application_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawApplication> {
    Ok(RawApplication {
        id: row.get(0)?,
        result: row.get(1)?,
        path: row.get(2)?,
        changes: row.get(3)?,
        artwork: row.get(4)?,
        library_snapshot: row.get(5)?,
        begun_unix: row.get(6)?,
        outcome: row.get(7)?,
        file_size_after: row.get(8)?,
        modified_unix_after: row.get(9)?,
        reason: row.get(10)?,
        completed_unix: row.get(11)?,
        library_after: row.get(12)?,
        undone_unix: row.get(13)?,
    })
}

impl RawApplication {
    fn decode(self) -> Result<Application> {
        let corrupt = || IdentifyError::Store("an application outcome in the store is incomplete".to_owned());
        let outcome = match self.outcome.as_deref() {
            None => None,
            Some("written") => Some(Outcome::Written {
                file_after: TrackKey {
                    path: self.path.clone(),
                    file_size: self.file_size_after.and_then(|n| u64::try_from(n).ok()).ok_or_else(corrupt)?,
                    modified_unix: self.modified_unix_after.ok_or_else(corrupt)?,
                },
                completed_unix: self.completed_unix.ok_or_else(corrupt)?,
            }),
            Some("abandoned") => Some(Outcome::Abandoned {
                reason: self.reason.unwrap_or_default(),
                completed_unix: self.completed_unix.ok_or_else(corrupt)?,
            }),
            Some(_) => return Err(IdentifyError::Store("unknown application outcome in the store".to_owned())),
        };
        Ok(Application {
            id: self.id,
            result: self.result,
            path: self.path,
            changes: from_json(&self.changes, "applied changes")?,
            artwork: self.artwork.as_deref().map(|a| from_json(a, "artwork")).transpose()?,
            library_snapshot: self.library_snapshot,
            begun_unix: self.begun_unix,
            outcome,
            library_after: self.library_after,
            undone_unix: self.undone_unix,
        })
    }
}

const QUEUE_COLUMNS: &str = "t.id, r.id, t.path, r.state, r.decision, r.score, t.existing_json, r.proposal_json, r.updated_unix, \
     r.library_track_id, r.library_path, \
     CASE WHEN r.proposal_json IS NOT NULL THEN json_extract(r.candidates_json, '$[0].candidate.source') END, \
     w.id, w.since_unix, w.reason, w.fields_json";

/// What [`QUEUE_COLUMNS`] selects from: a result, its track and its
/// waiting write, if it has one.
const QUEUE_FROM: &str = "results r JOIN tracks t ON t.id = r.track_id \
     LEFT JOIN waiting_writes w ON w.result_id = r.id \
         AND NOT EXISTS (SELECT 1 FROM waiting_write_ends e WHERE e.waiting_id = w.id)";

/// How many columns [`QUEUE_COLUMNS`] selects.
const QUEUE_COLUMN_COUNT: usize = 16;

fn queue_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawQueueItem> {
    Ok(RawQueueItem {
        track: row.get(0)?,
        result: row.get(1)?,
        path: row.get(2)?,
        state: row.get(3)?,
        decision: row.get(4)?,
        score: row.get(5)?,
        existing: row.get(6)?,
        proposal: row.get(7)?,
        updated_unix: row.get(8)?,
        library_track_id: row.get(9)?,
        library_path: row.get(10)?,
        source: row.get(11)?,
        waiting_id: row.get(12)?,
        waiting_since: row.get(13)?,
        waiting_reason: row.get(14)?,
        waiting_fields: row.get(15)?,
    })
}

/// A queue row as stored, before its text is decoded.
struct RawQueueItem {
    track: TrackId,
    result: ResultId,
    path: String,
    state: String,
    decision: String,
    score: Option<f64>,
    existing: String,
    proposal: Option<String>,
    updated_unix: i64,
    library_track_id: Option<String>,
    library_path: Option<String>,
    source: Option<String>,
    waiting_id: Option<WaitingId>,
    waiting_since: Option<i64>,
    waiting_reason: Option<String>,
    waiting_fields: Option<String>,
}

impl RawQueueItem {
    fn decode(self) -> Result<QueueItem> {
        let waiting = match (self.waiting_id, self.waiting_fields) {
            (Some(id), Some(fields)) => Some(WaitingWrite {
                id,
                result: self.result,
                path: self.path.clone(),
                fields: from_json(&fields, "waiting fields")?,
                reason: self.waiting_reason.unwrap_or_default(),
                since_unix: self.waiting_since.unwrap_or_default(),
            }),
            _ => None,
        };
        Ok(QueueItem {
            waiting,
            track: self.track,
            result: self.result,
            path: self.path,
            state: ReviewState::parse(&self.state)?,
            decision: parse_decision(&self.decision)?,
            score: self.score,
            existing: from_json(&self.existing, "existing tags")?,
            proposal: self.proposal.as_deref().map(|p| from_json(p, "proposal")).transpose()?,
            updated_unix: self.updated_unix,
            library: self
                .library_track_id
                .map(|track_id| LibraryTrack { track_id, path: self.library_path.unwrap_or_default() }),
            source: self
                .source
                .map(|s| serde_json::from_value(serde_json::Value::String(s)))
                .transpose()
                .map_err(|_| IdentifyError::Store("unknown candidate source in the store".to_owned()))?,
        })
    }
}

/// The identify store.
///
/// It holds one rusqlite `Connection`, which is `Send` but not `Sync`: the
/// app (Phase 3) must keep the store behind a `Mutex` or on a single owning
/// thread, never share it unguarded. The file must be on a local disk: WAL
/// journaling needs shared memory, which network file systems do not
/// provide reliably.
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
        if from <= 1 {
            tx.execute_batch(SCHEMA_V2).map_err(|e| sql("migrate the schema to version 2", &e))?;
        }
        if from <= 2 {
            tx.execute_batch(SCHEMA_V3).map_err(|e| sql("migrate the schema to version 3", &e))?;
        }
        if from <= 3 {
            tx.execute_batch(SCHEMA_V4).map_err(|e| sql("migrate the schema to version 4", &e))?;
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
        self.conn
            .query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1", [name], |_| Ok(()))
            .optional()
            .map(|r| r.is_some())
            .map_err(|e| sql("read the schema", &e))
    }
}

impl Store {
    /// Records a track as just read: inserts it, or updates its latest
    /// size, time, duration, fingerprint and tags. Takes no snapshot: those
    /// belong to writes ([`Store::begin_application`]).
    pub fn upsert_track(
        &self,
        key: &TrackKey,
        duration_secs: f64,
        fingerprint: Option<&str>,
        existing: &ExistingTags,
    ) -> Result<TrackId> {
        upsert_track(&self.conn, key, duration_secs, fingerprint, existing)
    }

    /// The track's ORIGINAL tags: the before-snapshot of its earliest
    /// application with outcome written, exactly as taken. `None` until one
    /// has been written; an open or abandoned application does not count
    /// (the file may never have changed). Later writes, undos and
    /// re-identifications never change it; this is the "restore original"
    /// path, while undoing one write restores [`Store::application_snapshot`].
    pub fn original_snapshot(&self, id: TrackId) -> Result<Option<TagSnapshot>> {
        read_snapshot(
            &self.conn,
            "SELECT s.snapshot_json, s.has_raw_id3, s.raw_id3_v2, s.raw_id3_v1
             FROM application_snapshots s
             JOIN applications a ON a.id = s.application_id
             JOIN results r ON r.id = a.result_id
             JOIN application_outcomes o ON o.application_id = a.id AND o.outcome = 'written'
             WHERE r.track_id = ?1 ORDER BY a.id LIMIT 1",
            id,
        )
    }

    /// The file's tags just before this application, exactly as taken: what
    /// undoing it restores, and what reconciling an open one compares
    /// against. Available from [`Store::begin_application`] on, whatever the
    /// outcome.
    pub fn application_snapshot(&self, application: ApplicationId) -> Result<Option<TagSnapshot>> {
        read_snapshot(
            &self.conn,
            "SELECT snapshot_json, has_raw_id3, raw_id3_v2, raw_id3_v1
             FROM application_snapshots WHERE application_id = ?1",
            application,
        )
    }

    /// The file's tags right after this application's write, exactly as
    /// recorded by [`Store::complete_application_with`]; `None` when it was
    /// not written or was completed without them.
    pub fn application_after_snapshot(&self, application: ApplicationId) -> Result<Option<TagSnapshot>> {
        read_snapshot(
            &self.conn,
            "SELECT after_snapshot_json, after_has_raw_id3, after_raw_id3_v2, after_raw_id3_v1
             FROM application_outcomes WHERE application_id = ?1 AND after_snapshot_json IS NOT NULL",
            application,
        )
    }

    /// The track's latest written application, undone or not: the only one
    /// undo may act on.
    pub fn latest_written_application(&self, track: TrackId) -> Result<Option<Application>> {
        let query = format!(
            "SELECT {APPLICATION_COLUMNS} FROM {APPLICATION_FROM}
             WHERE r.track_id = ?1 AND o.outcome = 'written' ORDER BY a.id DESC LIMIT 1"
        );
        Ok(self.applications(&query, [track])?.pop())
    }

    /// Records that a written application has been undone (the file and the
    /// library put back), and moves its result to `Undone`, in one
    /// transaction. Refused unless it is its track's latest written
    /// application, not undone yet, and its result is current and applied.
    pub fn record_undo(&self, application: ApplicationId) -> Result<()> {
        let tx = self.conn.unchecked_transaction().map_err(|e| sql("begin", &e))?;
        let row: Option<(ResultId, TrackId, Option<String>, bool)> = tx
            .query_row(
                "SELECT a.result_id, r.track_id, o.outcome,
                        EXISTS (SELECT 1 FROM application_undos u WHERE u.application_id = a.id)
                 FROM applications a JOIN results r ON r.id = a.result_id
                 LEFT JOIN application_outcomes o ON o.application_id = a.id
                 WHERE a.id = ?1",
                [application],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(|e| sql("read the application", &e))?;
        let Some((result, track, outcome, undone)) = row else {
            return Err(IdentifyError::Store(format!("no application {application}")));
        };
        if outcome.as_deref() != Some("written") {
            return Err(IdentifyError::Store(format!("application {application} was never written")));
        }
        if undone {
            return Err(IdentifyError::Store(format!("application {application} has already been undone")));
        }
        let latest: Option<ApplicationId> = tx
            .query_row(
                "SELECT a.id FROM applications a JOIN results r ON r.id = a.result_id
                 JOIN application_outcomes o ON o.application_id = a.id AND o.outcome = 'written'
                 WHERE r.track_id = ?1 ORDER BY a.id DESC LIMIT 1",
                [track],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| sql("read the applications", &e))?;
        if latest != Some(application) {
            return Err(IdentifyError::Store(format!(
                "application {application} is not the track's latest write; only that one can be undone"
            )));
        }
        let from = current_state(&tx, result)?;
        if !from.may_become(ReviewState::Undone) {
            return Err(IdentifyError::Store(format!("result {result} is {}, not applied", from.as_str())));
        }
        tx.execute(
            "INSERT INTO application_undos (application_id, undone_unix) VALUES (?1, ?2)",
            params![application, now_unix()],
        )
        .map_err(|e| sql("record the undo", &e))?;
        set_state_unchecked(&tx, result, ReviewState::Undone)?;
        tx.commit().map_err(|e| sql("commit the undo", &e))
    }

    /// The track with this path, if the store knows it.
    pub fn track_id(&self, path: &str) -> Result<Option<TrackId>> {
        self.conn
            .query_row("SELECT id FROM tracks WHERE path = ?1", [path], |r| r.get(0))
            .optional()
            .map_err(|e| sql("find the track", &e))
    }

    /// Stores a new identification of a track as its current result; the
    /// previous one stays as history. Starts `Pending`; `Unidentified` for
    /// that decision; `Accepted` for `AutoApply`, which
    /// [`Store::complete_application`] turns into `AutoApplied` once written. A track whose current result has
    /// been applied (`Applied` or `AutoApplied`) keeps it unless
    /// `allow_replacing_applied`: replacing it would hide the undo for tags
    /// already written. Refused while the track has an open application.
    #[allow(clippy::too_many_arguments)] // the contract's shape
    pub fn save_result(
        &self,
        id: TrackId,
        decision: Decision,
        candidates: &[Scored],
        proposal: Option<&Proposal>,
        enrichment: Option<&Enrichment>,
        warnings: &[String],
        allow_replacing_applied: bool,
    ) -> Result<ResultId> {
        let tx = self.conn.unchecked_transaction().map_err(|e| sql("begin", &e))?;
        let result = save_result(
            &tx,
            id,
            &NewResult { decision, candidates, proposal, enrichment, warnings, library: None },
            allow_replacing_applied,
        )?;
        tx.commit().map_err(|e| sql("commit the result", &e))?;
        Ok(result)
    }

    /// Stores one identification as the pipeline returned it, in one
    /// transaction: the fetched artwork (so the caller can drop the bytes),
    /// the track as read with its fingerprint (`key` should come from
    /// [`TrackKey::for_file`] when the file was read), and the result with
    /// the library track it was identified for ([`Store::save_result`]'s
    /// rules apply).
    ///
    /// All or nothing: a refusal (artwork that does not add up, an applied
    /// result, an open write) leaves no image, track row or result behind.
    /// A proposal whose artwork hash differs from the fetched bytes, or that
    /// names artwork neither fetched nor stored, is refused. A fingerprint
    /// that could not be computed keeps the one stored for an unchanged
    /// file.
    pub fn save_track_result(
        &self,
        key: &TrackKey,
        result: &TrackResult,
        library: Option<&LibraryTrack>,
        allow_replacing_applied: bool,
    ) -> Result<(TrackId, ResultId)> {
        self.save_track_result_with(key, result, library, None, allow_replacing_applied)
    }

    /// [`Store::save_track_result`], also importing the track's rekordbox
    /// record (read from the library by the caller) in the same transaction,
    /// so a result and its import are stored together or not at all.
    pub fn save_track_result_with(
        &self,
        key: &TrackKey,
        result: &TrackResult,
        library: Option<&LibraryTrack>,
        rekordbox: Option<&RekordboxRecord>,
        allow_replacing_applied: bool,
    ) -> Result<(TrackId, ResultId)> {
        let wanted = result.proposal.as_ref().and_then(|p| p.artwork.as_ref()).map(|a| a.sha256.as_str());
        let tx = self.conn.unchecked_transaction().map_err(|e| sql("begin", &e))?;
        match (&result.artwork, wanted) {
            (Some(image), Some(wanted)) if sha256_hex(&image.bytes) != wanted => {
                return Err(IdentifyError::Store("the fetched artwork is not the image the proposal names".to_owned()));
            }
            (None, Some(wanted)) if artwork(&tx, wanted)?.is_none() => {
                return Err(IdentifyError::Store("the proposal names artwork that was neither fetched nor stored".to_owned()));
            }
            _ => {}
        }
        if let Some(image) = &result.artwork {
            put_artwork(&tx, &image.bytes, &image.mime)?;
        }
        let track = upsert_track(&tx, key, result.duration_secs, result.encoded_fingerprint.as_deref(), &result.existing)?;
        let saved = save_result(
            &tx,
            track,
            &NewResult {
                decision: result.decision,
                candidates: &result.candidates,
                proposal: result.proposal.as_ref(),
                enrichment: result.enrichment.as_ref(),
                warnings: &result.warnings,
                library,
            },
            allow_replacing_applied,
        )?;
        if let Some(record) = rekordbox {
            insert_rekordbox_record(&tx, saved, record)?;
        }
        tx.commit().map_err(|e| sql("commit the track result", &e))?;
        Ok((track, saved))
    }

    /// Moves a current result to another state, if the table on
    /// [`ReviewState`] allows it.
    pub fn set_state(&self, result: ResultId, state: ReviewState) -> Result<()> {
        let tx = self.conn.unchecked_transaction().map_err(|e| sql("begin", &e))?;
        let from = current_state(&tx, result)?;
        // While a write is open the result must stay Accepted: otherwise a
        // file that was written could never be completed (complete_application
        // needs Accepted) nor truthfully abandoned, and so never undone.
        let track: TrackId = tx
            .query_row("SELECT track_id FROM results WHERE id = ?1", [result], |r| r.get(0))
            .map_err(|e| sql("read the result", &e))?;
        if has_open_application(&tx, track)? {
            return Err(IdentifyError::Store(format!(
                "result {result}: a write is in progress; it must be completed or abandoned first"
            )));
        }
        if open_waiting(&tx, result)?.is_some() {
            return Err(IdentifyError::Store(WAITING_REFUSAL.to_owned()));
        }
        if !from.may_become(state) {
            return Err(IdentifyError::Store(format!(
                "result {result} cannot go from {} to {}",
                from.as_str(),
                state.as_str()
            )));
        }
        set_state_unchecked(&tx, result, state)?;
        tx.commit().map_err(|e| sql("commit the state", &e))
    }

    /// Step one of a write, before the file is touched: commits what will be
    /// written (`applied`, exactly; the user may have unticked some of the
    /// proposal, and `artwork`), the file's tags right now (`before`, what
    /// undoing the write will restore) and the library side of the change.
    /// The result stays `Accepted` until [`Store::complete_application`].
    ///
    /// Refused unless the result is its track's current one and `Accepted`,
    /// and while the track has another application open. A `raw_id3` part
    /// that is not unpadded standard base64 is refused, storing nothing,
    /// since it could not be given back identical.
    pub fn begin_application(
        &self,
        result: ResultId,
        applied: &[FieldChange],
        artwork: Option<&ArtworkRef>,
        before: &TagSnapshot,
        library_snapshot: Option<&str>,
    ) -> Result<ApplicationId> {
        let before = EncodedSnapshot::new(before)?;
        let changes = to_json(applied, "applied changes")?;
        let artwork = artwork.map(|a| to_json(a, "artwork")).transpose()?;
        let tx = self.conn.unchecked_transaction().map_err(|e| sql("begin", &e))?;
        let from = current_state(&tx, result)?;
        if from != ReviewState::Accepted {
            return Err(IdentifyError::Store(format!(
                "result {result} is {}; only an accepted result can be applied",
                from.as_str()
            )));
        }
        let track: TrackId = tx
            .query_row("SELECT track_id FROM results WHERE id = ?1", [result], |r| r.get(0))
            .map_err(|e| sql("find the result's track", &e))?;
        if has_open_application(&tx, track)? {
            return Err(IdentifyError::Store(format!(
                "track {track} already has a write in progress; complete or abandon it first"
            )));
        }
        let now = now_unix();
        let application: ApplicationId = tx
            .query_row(
                "INSERT INTO applications (result_id, changes_json, artwork_json, library_snapshot, begun_unix)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 RETURNING id",
                params![result, changes, artwork, library_snapshot, now],
                |r| r.get(0),
            )
            .map_err(|e| sql("record the application", &e))?;
        tx.execute(
            "INSERT INTO application_snapshots (application_id, snapshot_json, has_raw_id3, raw_id3_v2, raw_id3_v1,
                                                taken_unix)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![application, before.text, before.has_raw, before.v2, before.v1, now],
        )
        .map_err(|e| sql("record the tag snapshot", &e))?;
        tx.commit().map_err(|e| sql("commit the application", &e))?;
        Ok(application)
    }

    /// Step two of a write that succeeded: the file now holds the changes
    /// and has the size and time in `file_after`. The result becomes
    /// `AutoApplied` when its stored decision is `AutoApply` and this is its
    /// first written application, otherwise `Applied` (a user accepted it,
    /// or re-accepted it after an undo, which is a review). The track takes
    /// the new size and time, so the next scan does not read the write as
    /// someone else's change.
    pub fn complete_application(&self, id: ApplicationId, file_after: &TrackKey) -> Result<()> {
        self.complete_application_with(id, file_after, None, None)
    }

    /// [`Store::complete_application`], also recording the file's tags right
    /// after the write (`after`, what undo checks the file still holds) and
    /// the library values the write left (`library_after`, JSON the store
    /// keeps without reading).
    pub fn complete_application_with(
        &self,
        id: ApplicationId,
        file_after: &TrackKey,
        after: Option<&TagSnapshot>,
        library_after: Option<&str>,
    ) -> Result<()> {
        let size = file_size(file_after.file_size)?;
        let after = after.map(EncodedSnapshot::new).transpose()?;
        let tx = self.conn.unchecked_transaction().map_err(|e| sql("begin", &e))?;
        let (result, track, path) = open_application(&tx, id)?;
        if path != file_after.path {
            return Err(IdentifyError::Store(format!("application {id} belongs to another file than the one written")));
        }
        let from = current_state(&tx, result)?;
        if from != ReviewState::Accepted {
            return Err(IdentifyError::Store(format!("result {result} is {}, not accepted", from.as_str())));
        }
        let decision: String = tx
            .query_row("SELECT decision FROM results WHERE id = ?1", [result], |r| r.get(0))
            .map_err(|e| sql("read the result's decision", &e))?;
        let auto = parse_decision(&decision)? == Decision::AutoApply && !has_written_application(&tx, result)?;
        let to = if auto { ReviewState::AutoApplied } else { ReviewState::Applied };
        tx.execute(
            "INSERT INTO application_outcomes (application_id, outcome, file_size_after, modified_unix_after,
                                               completed_unix, after_snapshot_json, after_has_raw_id3,
                                               after_raw_id3_v2, after_raw_id3_v1, library_after)
             VALUES (?1, 'written', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                size,
                file_after.modified_unix,
                now_unix(),
                after.as_ref().map(|a| a.text.as_str()),
                after.as_ref().map(|a| a.has_raw),
                after.as_ref().and_then(|a| a.v2.as_deref()),
                after.as_ref().and_then(|a| a.v1.as_deref()),
                library_after,
            ],
        )
        .map_err(|e| sql("record the outcome", &e))?;
        tx.execute(
            "UPDATE tracks SET file_size = ?2, modified_unix = ?3 WHERE id = ?1",
            params![track, size, file_after.modified_unix],
        )
        .map_err(|e| sql("update the track", &e))?;
        set_state_unchecked(&tx, result, to)?;
        end_waiting(&tx, result, WaitingEnd::Written, None)?;
        tx.commit().map_err(|e| sql("commit the outcome", &e))
    }

    /// Step two of a write that did not happen: the file was left unchanged
    /// (or has been put back to the before-snapshot). The result stays
    /// `Accepted`; the application is kept, as a record, but never counts
    /// for undo or the original snapshot.
    pub fn abandon_application(&self, id: ApplicationId, reason: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction().map_err(|e| sql("begin", &e))?;
        open_application(&tx, id)?;
        tx.execute(
            "INSERT INTO application_outcomes (application_id, outcome, reason, completed_unix)
             VALUES (?1, 'abandoned', ?2, ?3)",
            params![id, reason, now_unix()],
        )
        .map_err(|e| sql("record the outcome", &e))?;
        tx.commit().map_err(|e| sql("commit the outcome", &e))
    }

    /// Applications begun but neither completed nor abandoned, oldest first:
    /// writes interrupted by a crash, a full disk or a lost volume.
    ///
    /// The job must reconcile each before writing anything else (Phase 3/4,
    /// on startup):
    ///
    /// 1. Read the file's tags now.
    /// 2. If they equal the before-snapshot ([`Store::application_snapshot`])
    ///    the write never happened: [`Store::abandon_application`].
    /// 3. If they hold every planned change (`changes`, `artwork`) and are
    ///    otherwise the before-snapshot, the write happened:
    ///    [`Store::complete_application`] with the file's current size and
    ///    time.
    /// 4. Anything else (a partial write, or the file changed since): put
    ///    the before-snapshot back, then abandon; if that is impossible, leave
    ///    the application open and tell the user. Never guess.
    ///
    /// [`Store::reconcile_open_applications`] does step 2 and leaves every
    /// other case open for the user.
    pub fn open_applications(&self) -> Result<Vec<Application>> {
        let query = format!("SELECT {APPLICATION_COLUMNS} FROM {APPLICATION_FROM} WHERE o.application_id IS NULL ORDER BY a.id");
        self.applications(&query, [])
    }

    /// The startup pass over [`Store::open_applications`]: for each, reads
    /// the file's tags now with `read_current` and compares them with the
    /// before-snapshot, exactly (raw ID3 bytes included).
    ///
    /// - Unchanged: the write never happened, so the application is
    ///   abandoned ([`Reconciliation::Abandoned`]).
    /// - Anything else (the changes are there, partly there, the file was
    ///   changed by something else, or it cannot be read): the application
    ///   stays open and is returned as [`Reconciliation::NeedsUser`]. Nothing
    ///   is completed or restored on a guess; while it is open the track
    ///   cannot be identified again or written.
    ///
    /// Only a store error stops the pass. Nothing here writes to a file.
    pub fn reconcile_open_applications<F>(&self, mut read_current: F) -> Result<Vec<Reconciliation>>
    where
        F: FnMut(&Application) -> FileNow,
    {
        let mut outcomes = Vec::new();
        for application in self.open_applications()? {
            let Some(before) = self.application_snapshot(application.id)? else {
                outcomes.push(Reconciliation::NeedsUser {
                    application: Box::new(application),
                    reason: "no tag snapshot was recorded before the write".to_owned(),
                });
                continue;
            };
            let id = application.id;
            let unknown = match read_current(&application) {
                FileNow::Missing => {
                    outcomes.push(Reconciliation::NeedsUser {
                        application: Box::new(application),
                        reason: "the file is not there".to_owned(),
                    });
                    continue;
                }
                // Held by a player now: tried again at exit and next start.
                FileNow::Locked(e) => {
                    outcomes.push(Reconciliation::NeedsUser {
                        application: Box::new(application),
                        reason: format!("the file is open in another program: {e}"),
                    });
                    continue;
                }
                // The file is readable but its tags are not: what the write
                // left can never be told, so it is closed now, its
                // before-snapshot kept.
                FileNow::Unparseable(e) => {
                    let why = format!("the file's tags could not be parsed: {e}");
                    self.close_unknown(&application, &why, RECONCILED_UNPARSEABLE_WARNING)?;
                    outcomes.push(Reconciliation::Unknown { application: id, reason: why });
                    continue;
                }
                // Not knowing is not "neither": a later start may read it.
                FileNow::Unreadable(e) => {
                    outcomes.push(Reconciliation::NeedsUser {
                        application: Box::new(application),
                        reason: format!("the file could not be read: {e}"),
                    });
                    continue;
                }
                FileNow::Read { tags, .. } if tags == before => {
                    self.abandon_application(id, RECONCILED_UNCHANGED)?;
                    outcomes.push(Reconciliation::Abandoned { application: id });
                    continue;
                }
                FileNow::Read { tags, key } if crate::write::holds_exactly(&before, &tags, &application.changes) => {
                    let key = TrackKey { path: application.path.clone(), ..key };
                    self.complete_application_with(id, &key, Some(&tags), None)?;
                    outcomes.push(Reconciliation::Completed { application: id });
                    continue;
                }
                FileNow::Read { .. } => "the file's tags are neither the ones from before the write nor the written ones".to_owned(),
            };
            self.close_unknown(&application, &unknown, RECONCILED_UNKNOWN_WARNING)?;
            outcomes.push(Reconciliation::Unknown { application: id, reason: unknown });
        }
        Ok(outcomes)
    }

    /// Closes an open application as unknown and returns its result to
    /// review: pending, and an auto-apply decided `Review`.
    fn close_unknown(&self, application: &Application, why: &str, warning: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction().map_err(|e| sql("begin", &e))?;
        open_application(&tx, application.id)?;
        tx.execute(
            "INSERT INTO application_outcomes (application_id, outcome, reason, completed_unix)
             VALUES (?1, 'abandoned', ?2, ?3)",
            params![application.id, format!("{RECONCILED_UNKNOWN} ({why})"), now_unix()],
        )
        .map_err(|e| sql("record the outcome", &e))?;
        return_to_review(&tx, application.result, Some(warning))?;
        tx.commit().map_err(|e| sql("commit the outcome", &e))
    }

    /// [`Store::reconcile_open_applications`] reading each file's tags from
    /// disk at its stored path, after deleting any staged file the
    /// interrupted write left beside it ([`crate::write::remove_stale_temps`]).
    pub fn reconcile_open_applications_from_files(&self) -> Result<Vec<Reconciliation>> {
        self.reconcile_open_applications(|application| {
            let path = Path::new(&application.path);
            if !path.exists() {
                return FileNow::Missing;
            }
            crate::write::remove_stale_temps(path);
            let tags = match crate::tags::read(path) {
                Ok(info) => info.snapshot,
                // Held open (a sharing or lock violation): later.
                Err(e) if crate::write::held_open(path) => return FileNow::Locked(e.to_string()),
                // It opens and reads, but its tags do not parse.
                Err(e) if opens(path) => return FileNow::Unparseable(e.to_string()),
                Err(e) => return FileNow::Unreadable(e.to_string()),
            };
            match TrackKey::for_file(path) {
                Ok(key) => FileNow::Read { tags, key },
                Err(e) => FileNow::Unreadable(e.to_string()),
            }
        })
    }

    /// What the last written application of a result wrote; empty when
    /// nothing was.
    pub fn applied_changes(&self, result: ResultId) -> Result<Vec<FieldChange>> {
        Ok(self.last_application(result)?.map(|a| a.changes).unwrap_or_default())
    }

    /// The last application of a result with outcome written: what undo
    /// works from (restore [`Store::application_snapshot`] of its `id`, after
    /// checking the file still matches its `file_after`). Open and abandoned
    /// applications are ignored.
    pub fn last_application(&self, result: ResultId) -> Result<Option<Application>> {
        let query = format!(
            "SELECT {APPLICATION_COLUMNS} FROM {APPLICATION_FROM}
             WHERE a.result_id = ?1 AND o.outcome = 'written' ORDER BY a.id DESC LIMIT 1"
        );
        Ok(self.applications(&query, [result])?.pop())
    }

    /// One application, whatever its outcome.
    pub fn application(&self, id: ApplicationId) -> Result<Option<Application>> {
        let query = format!("SELECT {APPLICATION_COLUMNS} FROM {APPLICATION_FROM} WHERE a.id = ?1");
        Ok(self.applications(&query, [id])?.pop())
    }

    fn applications<P: rusqlite::Params>(&self, query: &str, params: P) -> Result<Vec<Application>> {
        let mut stmt = self.conn.prepare(query).map_err(|e| sql("read the applications", &e))?;
        let rows = stmt
            .query_map(params, application_row)
            .map_err(|e| sql("read the applications", &e))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| sql("read the applications", &e))?;
        rows.into_iter().map(RawApplication::decode).collect()
    }

    /// A page of current results, best score first (no score last), then
    /// by path.
    pub fn list(&self, filter: Filter, offset: u32, limit: u32) -> Result<Vec<QueueItem>> {
        let query = format!(
            "SELECT {QUEUE_COLUMNS} FROM {QUEUE_FROM}
             WHERE r.is_current = 1 AND {}
             ORDER BY {}
             LIMIT ?1 OFFSET ?2",
            filter.condition(),
            filter.order()
        );
        let mut stmt = self.conn.prepare(&query).map_err(|e| sql("list the queue", &e))?;
        let rows = stmt
            .query_map(params![limit, offset], queue_item)
            .map_err(|e| sql("list the queue", &e))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| sql("list the queue", &e))?;
        rows.into_iter().map(RawQueueItem::decode).collect()
    }

    /// How many current results the filter takes.
    pub fn count(&self, filter: Filter) -> Result<u64> {
        let query = format!("SELECT COUNT(*) FROM {QUEUE_FROM} WHERE r.is_current = 1 AND {}", filter.condition());
        let n: i64 = self.conn.query_row(&query, [], |r| r.get(0)).map_err(|e| sql("count the queue", &e))?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// One result in full (current or history): its queue row, all its
    /// candidates, the enrichment and the warnings.
    pub fn result(&self, id: ResultId) -> Result<Option<ResultDetail>> {
        let query = format!(
            "SELECT {QUEUE_COLUMNS}, r.candidates_json, r.enrichment_json, r.warnings_json
             FROM {QUEUE_FROM} WHERE r.id = ?1"
        );
        let at = QUEUE_COLUMN_COUNT;
        let row = self
            .conn
            .query_row(&query, [id], |r| {
                Ok((queue_item(r)?, r.get::<_, String>(at)?, r.get::<_, Option<String>>(at + 1)?, r.get::<_, String>(at + 2)?))
            })
            .optional()
            .map_err(|e| sql("read the result", &e))?;
        let Some((item, candidates, enrichment, warnings)) = row else {
            return Ok(None);
        };
        Ok(Some((
            item.decode()?,
            from_json(&candidates, "candidates")?,
            enrichment.as_deref().map(|e| from_json(e, "enrichment")).transpose()?,
            from_json(&warnings, "warnings")?,
        )))
    }

    /// The user's edits to a proposal before it is written: each
    /// `(field, value)` replaces that field's proposed text. Only fields the
    /// proposal already lists can be edited, never artwork (an image is not
    /// text) and never the sub-genre (it is appended to the comment as the
    /// file holds it at write time, so a typed whole comment could remove
    /// text). Returns the proposal as now stored.
    ///
    /// An edited proposal is the user's, not the services': a result decided
    /// for auto-apply becomes a review (its decision `Review`), so it is never
    /// written unattended and its write is recorded as `Applied`.
    ///
    /// Refused unless the result is its track's current one and `Pending` or
    /// `Accepted`, and while the track has a write open. The candidates are
    /// kept, so what the services proposed can always be derived again.
    pub fn edit_proposal(&self, result: ResultId, edits: &[(Field, String)]) -> Result<Proposal> {
        let tx = self.conn.unchecked_transaction().map_err(|e| sql("begin", &e))?;
        let from = current_state(&tx, result)?;
        if !matches!(from, ReviewState::Pending | ReviewState::Accepted) {
            return Err(IdentifyError::Store(format!(
                "result {result} is {}; only a pending or accepted result can be edited",
                from.as_str()
            )));
        }
        let (track, proposal): (TrackId, Option<String>) = tx
            .query_row("SELECT track_id, proposal_json FROM results WHERE id = ?1", [result], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .map_err(|e| sql("read the result", &e))?;
        if has_open_application(&tx, track)? {
            return Err(IdentifyError::Store(format!(
                "result {result}: a write is in progress; it must be completed or abandoned first"
            )));
        }
        if open_waiting(&tx, result)?.is_some() {
            return Err(IdentifyError::Store(WAITING_REFUSAL.to_owned()));
        }
        let mut proposal: Proposal = match proposal {
            Some(json) => from_json(&json, "proposal")?,
            None => return Err(IdentifyError::Store(format!("result {result} has no proposal to edit"))),
        };
        for (field, value) in edits {
            if *field == Field::Artwork {
                return Err(IdentifyError::Store("artwork cannot be edited as text".to_owned()));
            }
            if *field == Field::SubGenre {
                return Err(IdentifyError::Store(
                    "the sub-genre is appended to the comment as it is; it cannot be edited".to_owned(),
                ));
            }
            let change = proposal
                .changes
                .iter_mut()
                .find(|c| c.field == *field)
                .ok_or_else(|| IdentifyError::Store(format!("result {result} proposes no change to {field:?}")))?;
            change.proposed.clone_from(value);
        }
        let json = to_json(&proposal, "proposal")?;
        tx.execute(
            "UPDATE results SET proposal_json = ?2, updated_unix = ?3,
                    decision = CASE decision WHEN 'AutoApply' THEN 'Review' ELSE decision END
             WHERE id = ?1",
            params![result, json, now_unix()],
        )
        .map_err(|e| sql("store the edited proposal", &e))?;
        tx.commit().map_err(|e| sql("commit the edit", &e))?;
        Ok(proposal)
    }

    /// An auto-apply that could not be written becomes a review: its
    /// decision `Review` (so a later write is recorded as `Applied`, by the
    /// user), its state `Pending` (it shows under Needs review), and `reason`
    /// appended to its warnings. Refused unless the result is current,
    /// decided `AutoApply` and `Accepted`, and while its track has a write
    /// open (that one must be reconciled first).
    pub fn demote_auto_apply(&self, result: ResultId, reason: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction().map_err(|e| sql("begin", &e))?;
        let from = current_state(&tx, result)?;
        let (track, decision): (TrackId, String) = tx
            .query_row("SELECT track_id, decision FROM results WHERE id = ?1", [result], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|e| sql("read the result", &e))?;
        if from != ReviewState::Accepted || parse_decision(&decision)? != Decision::AutoApply {
            return Err(IdentifyError::Store(format!("result {result} is not an unwritten auto-apply")));
        }
        if has_open_application(&tx, track)? {
            return Err(IdentifyError::Store(format!(
                "result {result}: a write is in progress; it must be completed or abandoned first"
            )));
        }
        return_to_review(&tx, result, Some(reason))?;
        tx.commit().map_err(|e| sql("commit the demotion", &e))
    }

    /// Bulk accept: every current `Pending` result scoring at least
    /// `threshold` becomes `Accepted`. Returns them, best first.
    pub fn accept_at_or_above(&self, threshold: f64) -> Result<Vec<ResultId>> {
        if !threshold.is_finite() {
            return Err(IdentifyError::Store("the accept threshold must be a number".to_owned()));
        }
        let tx = self.conn.unchecked_transaction().map_err(|e| sql("begin", &e))?;
        let ids = {
            let mut stmt = tx
                .prepare(
                    "SELECT r.id FROM results r JOIN tracks t ON t.id = r.track_id
                     WHERE r.is_current = 1 AND r.state = 'Pending' AND r.score >= ?1
                     ORDER BY r.score DESC, t.path, r.id",
                )
                .map_err(|e| sql("find results to accept", &e))?;
            let ids = stmt
                .query_map([threshold], |r| r.get::<_, ResultId>(0))
                .map_err(|e| sql("find results to accept", &e))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| sql("find results to accept", &e))?;
            ids
        };
        for &id in &ids {
            set_state_unchecked(&tx, id, ReviewState::Accepted)?;
        }
        tx.commit().map_err(|e| sql("commit the accepts", &e))?;
        Ok(ids)
    }

    /// Keeps a write refused because another program has the file open, to
    /// be made once it is free: the result (current, `Pending` or
    /// `Accepted`; a pending one is accepted) and exactly the `fields` to
    /// write, with `reason`. A result keeps one waiting write: one already
    /// waiting with the same fields is kept as it is (its time too), one
    /// with other fields is superseded. Refused while the track has a write
    /// open. Returns the waiting write's id.
    pub fn defer_write(&self, result: ResultId, fields: &[Field], reason: &str) -> Result<WaitingId> {
        let tx = self.conn.unchecked_transaction().map_err(|e| sql("begin", &e))?;
        let from = current_state(&tx, result)?;
        if !matches!(from, ReviewState::Pending | ReviewState::Accepted) {
            return Err(IdentifyError::Store(format!("result {result} is {}; it cannot wait to be written", from.as_str())));
        }
        let track: TrackId = tx
            .query_row("SELECT track_id FROM results WHERE id = ?1", [result], |r| r.get(0))
            .map_err(|e| sql("read the result", &e))?;
        if has_open_application(&tx, track)? {
            return Err(IdentifyError::Store(format!(
                "result {result}: a write is in progress; it must be completed or abandoned first"
            )));
        }
        if from == ReviewState::Pending {
            set_state_unchecked(&tx, result, ReviewState::Accepted)?;
        }
        if let Some(existing) = open_waiting(&tx, result)? {
            if existing.fields == fields {
                tx.commit().map_err(|e| sql("commit the waiting write", &e))?;
                return Ok(existing.id);
            }
            end_waiting(&tx, result, WaitingEnd::Superseded, None)?;
        }
        let id = tx
            .query_row(
                "INSERT INTO waiting_writes (result_id, fields_json, reason, since_unix) VALUES (?1, ?2, ?3, ?4) RETURNING id",
                params![result, to_json(fields, "waiting fields")?, reason, now_unix()],
                |r| r.get(0),
            )
            .map_err(|e| sql("record the waiting write", &e))?;
        tx.commit().map_err(|e| sql("commit the waiting write", &e))?;
        Ok(id)
    }

    /// Every write still waiting, oldest first.
    pub fn waiting_writes(&self) -> Result<Vec<WaitingWrite>> {
        type Row = (WaitingId, ResultId, String, String, String, i64);
        let mut stmt = self
            .conn
            .prepare(
                "SELECT w.id, w.result_id, t.path, w.fields_json, w.reason, w.since_unix
                 FROM waiting_writes w JOIN results r ON r.id = w.result_id JOIN tracks t ON t.id = r.track_id
                 WHERE NOT EXISTS (SELECT 1 FROM waiting_write_ends e WHERE e.waiting_id = w.id)
                 ORDER BY w.id",
            )
            .map_err(|e| sql("read the waiting writes", &e))?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
            .map_err(|e| sql("read the waiting writes", &e))?
            .collect::<rusqlite::Result<Vec<Row>>>()
            .map_err(|e| sql("read the waiting writes", &e))?;
        rows.into_iter()
            .map(|(id, result, path, fields, reason, since_unix)| {
                Ok(WaitingWrite { id, result, path, fields: from_json(&fields, "waiting fields")?, reason, since_unix })
            })
            .collect()
    }

    /// The write waiting for this result, if there is one.
    pub fn waiting_write(&self, result: ResultId) -> Result<Option<WaitingWrite>> {
        open_waiting(&self.conn, result)
    }

    /// Ends a result's waiting write unmade because it was refused when it
    /// was tried (`warning` says why), and returns the result to review:
    /// `Pending`, an auto-apply decided `Review`, `warning` added to its
    /// warnings. A result that is no longer current or accepted, or whose
    /// track has a write open, keeps its state. Returns whether the result's
    /// state changed.
    pub fn drop_waiting(&self, result: ResultId, warning: &str) -> Result<bool> {
        let tx = self.conn.unchecked_transaction().map_err(|e| sql("begin", &e))?;
        if open_waiting(&tx, result)?.is_none() {
            return Err(IdentifyError::Store(format!("result {result} has no waiting write")));
        }
        let changed = is_reviewable_again(&tx, result)?;
        if changed {
            return_to_review(&tx, result, Some(warning))?;
        } else {
            end_waiting(&tx, result, WaitingEnd::Dropped, Some(warning))?;
        }
        tx.commit().map_err(|e| sql("commit the dropped write", &e))?;
        Ok(changed)
    }

    /// The user cancels a result's waiting write: it ends unmade and the
    /// result goes back to review (`Pending`, an auto-apply decided
    /// `Review`). Refused when it has none, and while its track has a write
    /// open.
    pub fn cancel_waiting(&self, result: ResultId) -> Result<()> {
        let tx = self.conn.unchecked_transaction().map_err(|e| sql("begin", &e))?;
        if open_waiting(&tx, result)?.is_none() {
            return Err(IdentifyError::Store(format!("result {result} has no waiting write")));
        }
        if !is_reviewable_again(&tx, result)? {
            return Err(IdentifyError::Store(format!(
                "result {result}: a write is in progress; it must be completed or abandoned first"
            )));
        }
        end_waiting(&tx, result, WaitingEnd::Cancelled, None)?;
        return_to_review(&tx, result, None)?;
        tx.commit().map_err(|e| sql("commit the cancel", &e))
    }

    /// The rekordbox record imported with a result; `None` for a result
    /// without one (no library track, or saved before version 3).
    pub fn rekordbox_record(&self, result: ResultId) -> Result<Option<RekordboxRecord>> {
        self.conn
            .query_row(
                "SELECT track_id, title, artist, album, label, genre, year, comment, isrc, imported_unix
                 FROM rekordbox_imports WHERE result_id = ?1",
                [result],
                |r| {
                    Ok(RekordboxRecord {
                        track_id: r.get(0)?,
                        title: r.get(1)?,
                        artist: r.get(2)?,
                        album: r.get(3)?,
                        label: r.get(4)?,
                        genre: r.get(5)?,
                        year: r.get(6)?,
                        comment: r.get(7)?,
                        isrc: r.get(8)?,
                        imported_unix: r.get(9)?,
                    })
                },
            )
            .optional()
            .map_err(|e| sql("read the imported rekordbox record", &e))
    }

    /// Stores an image once per content and returns its lowercase hex
    /// SHA-256. Storing the same bytes again keeps the first copy.
    pub fn put_artwork(&self, bytes: &[u8], mime: &str) -> Result<String> {
        put_artwork(&self.conn, bytes, mime)
    }

    /// An image and its MIME type, by hash.
    pub fn artwork(&self, sha256: &str) -> Result<Option<(Vec<u8>, String)>> {
        artwork(&self.conn, sha256)
    }
}


/// Whether the file opens and gives up its first byte: it is not held,
/// missing or unreadable.
fn opens(path: &Path) -> bool {
    let mut byte = [0_u8; 1];
    std::fs::File::open(path).and_then(|mut f| std::io::Read::read(&mut f, &mut byte)).is_ok()
}

/// [`Store::upsert_track`] on a connection or transaction.
fn upsert_track(
    conn: &Connection,
    key: &TrackKey,
    duration_secs: f64,
    fingerprint: Option<&str>,
    existing: &ExistingTags,
) -> Result<TrackId> {
    let size = file_size(key.file_size)?;
    let existing = to_json(existing, "existing tags")?;
    // One statement: atomic without an explicit transaction. A
    // fingerprint that could not be computed this time keeps the last
    // one, unless the file has changed (then the old one is stale).
    conn.query_row(
        "INSERT INTO tracks (path, file_size, modified_unix, duration_secs, fingerprint, existing_json,
                             created_unix, updated_unix)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
         ON CONFLICT (path) DO UPDATE SET
             fingerprint = CASE
                 WHEN excluded.file_size = tracks.file_size AND excluded.modified_unix = tracks.modified_unix
                 THEN COALESCE(excluded.fingerprint, tracks.fingerprint)
                 ELSE excluded.fingerprint END,
             file_size = excluded.file_size,
             modified_unix = excluded.modified_unix,
             duration_secs = excluded.duration_secs,
             existing_json = excluded.existing_json,
             updated_unix = excluded.updated_unix
         RETURNING id",
        params![key.path, size, key.modified_unix, duration_secs, fingerprint, existing, now_unix()],
        |r| r.get(0),
    )
    .map_err(|e| sql("record the track", &e))
}

/// What [`save_result`] stores.
struct NewResult<'a> {
    decision: Decision,
    candidates: &'a [Scored],
    proposal: Option<&'a Proposal>,
    enrichment: Option<&'a Enrichment>,
    warnings: &'a [String],
    library: Option<&'a LibraryTrack>,
}

/// [`Store::save_result`] inside the caller's transaction: checks every
/// refusal before it writes anything.
fn save_result(conn: &Connection, id: TrackId, new: &NewResult<'_>, allow_replacing_applied: bool) -> Result<ResultId> {
    let score = new.candidates.first().map(|c| c.score);
    let candidates = to_json(new.candidates, "candidates")?;
    let proposal = new.proposal.map(|p| to_json(p, "proposal")).transpose()?;
    let enrichment = new.enrichment.map(|e| to_json(e, "enrichment")).transpose()?;
    let warnings = to_json(new.warnings, "warnings")?;
    let now = now_unix();
    let current: Option<String> = conn
        .query_row("SELECT state FROM results WHERE track_id = ?1 AND is_current = 1", [id], |r| r.get(0))
        .optional()
        .map_err(|e| sql("read the current result", &e))?;
    if has_open_application(conn, id)? {
        return Err(IdentifyError::Store(format!(
            "track {id} has a write in progress; complete or abandon it before identifying again"
        )));
    }
    let waiting: Option<WaitingId> = conn
        .query_row(
            "SELECT w.id FROM waiting_writes w JOIN results r ON r.id = w.result_id
             WHERE r.track_id = ?1 AND NOT EXISTS (SELECT 1 FROM waiting_write_ends e WHERE e.waiting_id = w.id)
             LIMIT 1",
            [id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| sql("read the waiting writes", &e))?;
    if waiting.is_some() {
        return Err(IdentifyError::Store(format!(
            "track {id} has a write waiting for the file to be closed; cancel it before identifying again"
        )));
    }
    if let Some(state) = current.as_deref().map(ReviewState::parse).transpose()? {
        if state.is_applied() && !allow_replacing_applied {
            return Err(IdentifyError::Store(format!(
                "track {id} has an applied result ({}); undo it before identifying again",
                state.as_str()
            )));
        }
    }
    conn.execute("UPDATE results SET is_current = 0 WHERE track_id = ?1 AND is_current = 1", [id])
        .map_err(|e| sql("retire the previous result", &e))?;
    conn.query_row(
        "INSERT INTO results (track_id, is_current, decision, state, score, candidates_json, proposal_json,
                              enrichment_json, warnings_json, created_unix, updated_unix, library_track_id,
                              library_path)
         VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10, ?11)
         RETURNING id",
        params![
            id,
            decision_str(new.decision),
            initial_state(new.decision).as_str(),
            score,
            candidates,
            proposal,
            enrichment,
            warnings,
            now,
            new.library.map(|l| l.track_id.as_str()),
            new.library.map(|l| l.path.as_str()),
        ],
        |r| r.get(0),
    )
    .map_err(|e| sql("store the result", &e))
}

/// Inserts the rekordbox record imported with `result`.
fn insert_rekordbox_record(conn: &Connection, result: ResultId, r: &RekordboxRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO rekordbox_imports (result_id, track_id, title, artist, album, label, genre, year, comment, isrc,
                                        imported_unix)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![result, r.track_id, r.title, r.artist, r.album, r.label, r.genre, r.year, r.comment, r.isrc, r.imported_unix],
    )
    .map_err(|e| sql("import the rekordbox record", &e))?;
    Ok(())
}

/// [`Store::put_artwork`] on a connection or transaction.
fn put_artwork(conn: &Connection, bytes: &[u8], mime: &str) -> Result<String> {
    let hash = sha256_hex(bytes);
    conn.execute(
        "INSERT OR IGNORE INTO artwork (sha256, mime, bytes, created_unix) VALUES (?1, ?2, ?3, ?4)",
        params![hash, mime, bytes, now_unix()],
    )
    .map_err(|e| sql("store the artwork", &e))?;
    Ok(hash)
}

/// [`Store::artwork`] on a connection or transaction.
fn artwork(conn: &Connection, sha256: &str) -> Result<Option<(Vec<u8>, String)>> {
    conn.query_row("SELECT bytes, mime FROM artwork WHERE sha256 = ?1", [sha256], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()
        .map_err(|e| sql("read the artwork", &e))
}

/// The state of a result, which must be its track's current one.
fn current_state(conn: &Connection, result: ResultId) -> Result<ReviewState> {
    let row: Option<(String, bool)> = conn
        .query_row("SELECT state, is_current FROM results WHERE id = ?1", [result], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()
        .map_err(|e| sql("read the result's state", &e))?;
    match row {
        None => Err(IdentifyError::Store(format!("no result {result}"))),
        Some((_, false)) => Err(IdentifyError::Store(format!("result {result} has been superseded by a newer one"))),
        Some((state, true)) => ReviewState::parse(&state),
    }
}

fn set_state_unchecked(conn: &Connection, result: ResultId, state: ReviewState) -> Result<()> {
    conn.execute(
        "UPDATE results SET state = ?2, updated_unix = ?3 WHERE id = ?1",
        params![result, state.as_str(), now_unix()],
    )
    .map_err(|e| sql("change the result's state", &e))?;
    Ok(())
}

fn has_written_application(conn: &Connection, result: ResultId) -> Result<bool> {
    conn.query_row(
        "SELECT 1 FROM applications a JOIN application_outcomes o ON o.application_id = a.id
         WHERE a.result_id = ?1 AND o.outcome = 'written' LIMIT 1",
        [result],
        |_| Ok(()),
    )
    .optional()
    .map(|r| r.is_some())
    .map_err(|e| sql("read the applications", &e))
}

/// Whether any application for the track is still open.
fn has_open_application(conn: &Connection, track: TrackId) -> Result<bool> {
    conn.query_row(
        "SELECT 1 FROM applications a JOIN results r ON r.id = a.result_id
         WHERE r.track_id = ?1
           AND NOT EXISTS (SELECT 1 FROM application_outcomes o WHERE o.application_id = a.id)
         LIMIT 1",
        [track],
        |_| Ok(()),
    )
    .optional()
    .map(|r| r.is_some())
    .map_err(|e| sql("read the applications", &e))
}

/// A result's write still waiting, if it has one.
fn open_waiting(conn: &Connection, result: ResultId) -> Result<Option<WaitingWrite>> {
    let row: Option<(WaitingId, String, String, String, i64)> = conn
        .query_row(
            "SELECT w.id, t.path, w.fields_json, w.reason, w.since_unix
             FROM waiting_writes w JOIN results r ON r.id = w.result_id JOIN tracks t ON t.id = r.track_id
             WHERE w.result_id = ?1 AND NOT EXISTS (SELECT 1 FROM waiting_write_ends e WHERE e.waiting_id = w.id)
             ORDER BY w.id DESC LIMIT 1",
            [result],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()
        .map_err(|e| sql("read the waiting write", &e))?;
    row.map(|(id, path, fields, reason, since_unix)| {
        Ok(WaitingWrite { id, result, path, fields: from_json(&fields, "waiting fields")?, reason, since_unix })
    })
    .transpose()
}

/// Ends every write still waiting for `result` as `outcome`.
fn end_waiting(conn: &Connection, result: ResultId, outcome: WaitingEnd, reason: Option<&str>) -> Result<()> {
    conn.execute(
        "INSERT INTO waiting_write_ends (waiting_id, outcome, reason, ended_unix)
         SELECT w.id, ?2, ?3, ?4 FROM waiting_writes w
         WHERE w.result_id = ?1 AND NOT EXISTS (SELECT 1 FROM waiting_write_ends e WHERE e.waiting_id = w.id)",
        params![result, outcome.as_str(), reason, now_unix()],
    )
    .map_err(|e| sql("end the waiting write", &e))?;
    Ok(())
}

/// Whether a result may go back to review: current, `Accepted`, and its
/// track has no write open.
fn is_reviewable_again(conn: &Connection, result: ResultId) -> Result<bool> {
    let row: Option<(String, bool, TrackId)> = conn
        .query_row("SELECT state, is_current, track_id FROM results WHERE id = ?1", [result], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()
        .map_err(|e| sql("read the result", &e))?;
    let Some((state, current, track)) = row else { return Ok(false) };
    Ok(current && ReviewState::parse(&state)? == ReviewState::Accepted && !has_open_application(conn, track)?)
}

/// Returns a result that was going to be written to review, inside the
/// caller's transaction: `Pending`, an auto-apply decided `Review` (a later
/// write is then the user's, recorded `Applied`), `warning` (if any) added
/// to its warnings, and any waiting write ended as dropped. A result that
/// is not current or not `Accepted` keeps its state.
fn return_to_review(conn: &Connection, result: ResultId, warning: Option<&str>) -> Result<()> {
    end_waiting(conn, result, WaitingEnd::Dropped, warning)?;
    let row: Option<(String, bool, String)> = conn
        .query_row("SELECT state, is_current, warnings_json FROM results WHERE id = ?1", [result], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()
        .map_err(|e| sql("read the result", &e))?;
    let Some((state, true, warnings)) = row else { return Ok(()) };
    if ReviewState::parse(&state)? != ReviewState::Accepted {
        return Ok(());
    }
    let mut warnings: Vec<String> = from_json(&warnings, "warnings")?;
    if let Some(warning) = warning {
        warnings.push(warning.to_owned());
    }
    conn.execute(
        "UPDATE results SET state = 'Pending', warnings_json = ?2, updated_unix = ?3,
                decision = CASE decision WHEN 'AutoApply' THEN 'Review' ELSE decision END
         WHERE id = ?1",
        params![result, to_json(&warnings, "warnings")?, now_unix()],
    )
    .map_err(|e| sql("return the result to review", &e))?;
    Ok(())
}

/// An application that exists and has no outcome yet: its result, track and
/// path.
fn open_application(conn: &Connection, id: ApplicationId) -> Result<(ResultId, TrackId, String)> {
    let row: Option<(ResultId, TrackId, String, bool)> = conn
        .query_row(
            "SELECT a.result_id, t.id, t.path,
                    EXISTS (SELECT 1 FROM application_outcomes o WHERE o.application_id = a.id)
             FROM applications a JOIN results r ON r.id = a.result_id JOIN tracks t ON t.id = r.track_id
             WHERE a.id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .map_err(|e| sql("read the application", &e))?;
    match row {
        None => Err(IdentifyError::Store(format!("no application {id}"))),
        Some((.., true)) => Err(IdentifyError::Store(format!("application {id} already has an outcome"))),
        Some((result, track, path, false)) => Ok((result, track, path)),
    }
}

fn file_size(size: u64) -> Result<i64> {
    i64::try_from(size).map_err(|_| IdentifyError::Store("file size out of range".to_owned()))
}

/// Seconds since the Unix epoch; 0 for a clock set before it.
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn to_json<T: Serialize + ?Sized>(value: &T, what: &str) -> Result<String> {
    // The serde error is left out: it can quote the value (tag text).
    serde_json::to_string(value).map_err(|e| IdentifyError::Store(format!("could not encode the {what} ({:?})", e.classify())))
}

fn from_json<T: DeserializeOwned>(json: &str, what: &str) -> Result<T> {
    // Position only: serde's message can quote the stored text.
    serde_json::from_str(json).map_err(|e| {
        IdentifyError::Store(format!(
            "the stored {what} is unreadable ({:?} error at line {} column {})",
            e.classify(),
            e.line(),
            e.column()
        ))
    })
}

/// The text part of a [`TagSnapshot`]: everything but `raw_id3`, which is
/// stored as BLOBs. Deserialises as a `TagSnapshot` with no `raw_id3`.
#[derive(Serialize)]
struct SnapshotText<'a> {
    format: &'a str,
    values: &'a BTreeMap<Field, Vec<FrameValues>>,
    has_artwork: bool,
}

/// A snapshot as it is stored: the text part as JSON, the raw ID3 parts as
/// bytes.
struct EncodedSnapshot {
    text: String,
    has_raw: bool,
    v2: Option<Vec<u8>>,
    v1: Option<Vec<u8>>,
}

impl EncodedSnapshot {
    fn new(snapshot: &TagSnapshot) -> Result<Self> {
        let text = to_json(
            &SnapshotText { format: &snapshot.format, values: &snapshot.values, has_artwork: snapshot.has_artwork },
            "tag snapshot",
        )?;
        let raw = snapshot.raw_id3.as_ref();
        Ok(Self {
            text,
            has_raw: raw.is_some(),
            v2: raw.and_then(|r| r.v2.as_deref()).map(|b| decode_raw(b, "ID3v2")).transpose()?,
            v1: raw.and_then(|r| r.v1.as_deref()).map(|b| decode_raw(b, "ID3v1")).transpose()?,
        })
    }
}

/// Reads one stored snapshot with `query` (selecting the four snapshot
/// columns for `id`), re-encoding its raw parts as the model's base64.
fn read_snapshot(conn: &Connection, query: &str, id: i64) -> Result<Option<TagSnapshot>> {
    type Row = (String, bool, Option<Vec<u8>>, Option<Vec<u8>>);
    let row: Option<Row> = conn
        .query_row(query, [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .optional()
        .map_err(|e| sql("read the tag snapshot", &e))?;
    let Some((text, has_raw, v2, v1)) = row else {
        return Ok(None);
    };
    let mut snapshot: TagSnapshot = from_json(&text, "tag snapshot")?;
    snapshot.raw_id3 = has_raw.then(|| RawId3 { v2: v2.map(|b| BASE64.encode(b)), v1: v1.map(|b| BASE64.encode(b)) });
    Ok(Some(snapshot))
}

/// The model's encoding of raw ID3 bytes: unpadded standard base64.
const BASE64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD_NO_PAD;

/// Raw tag bytes from the model's base64, refused unless encoding them
/// again gives back exactly the same text.
fn decode_raw(b64: &str, what: &str) -> Result<Vec<u8>> {
    let bytes = BASE64
        .decode(b64)
        .map_err(|_| IdentifyError::Store(format!("the snapshot's raw {what} is not unpadded standard base64")))?;
    if BASE64.encode(&bytes) != b64 {
        return Err(IdentifyError::Store(format!("the snapshot's raw {what} is not canonical base64")));
    }
    Ok(bytes)
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
    use std::collections::BTreeMap;

    use super::*;
    use crate::model::{
        ArtworkSource, Candidate, CandidateSource, Field, FrameValues, RawId3, ReleaseInfo, ScoreBreakdown,
    };

    fn key(path: &str) -> TrackKey {
        TrackKey { path: path.to_owned(), file_size: 1_000, modified_unix: 1_700_000_000 }
    }

    fn tags(title: &str) -> ExistingTags {
        ExistingTags { artist: "Artist".into(), title: title.into(), ..ExistingTags::default() }
    }

    fn snapshot(title: &str) -> TagSnapshot {
        let mut values = BTreeMap::new();
        values.insert(Field::Title, vec![FrameValues { frame: "TIT2".into(), values: vec![title.into()] }]);
        values.insert(
            Field::CatalogNumber,
            vec![FrameValues { frame: "TXXX:Catalog #".into(), values: vec!["CAT 001".into(), "cat001".into()] }],
        );
        TagSnapshot {
            format: "Id3v2".into(),
            values,
            has_artwork: false,
            raw_id3: Some(RawId3 { v2: Some(BASE64.encode(format!("ID3 {title}"))), v1: None }),
        }
    }

    fn scored(title: &str, score: f64) -> Scored {
        Scored {
            candidate: Candidate {
                recording_mbid: format!("mbid-{title}"),
                artists: vec!["Artist".into()],
                artist_credit: "Artist feat. Other".into(),
                title: title.into(),
                disambiguation: String::new(),
                remixers: vec!["Remixer".into()],
                length_ms: Some(360_000),
                isrcs: vec!["GBAAA0000001".into()],
                genres: vec!["house".into()],
                releases: vec![ReleaseInfo { mbid: "rel".into(), title: "EP".into(), ..ReleaseInfo::default() }],
                acoustid_score: Some(0.97),
                source: CandidateSource::Fingerprint,
            },
            score,
            breakdown: ScoreBreakdown { acoustid: Some(0.97), duration: 1.0, text: 0.9, mix: 1.0, rejected: None },
        }
    }

    fn proposal() -> Proposal {
        Proposal::new(
            vec![FieldChange { field: Field::Label, current: String::new(), proposed: "Defected".into() }],
            Some(ArtworkRef {
                source: ArtworkSource::CoverArtArchive,
                url: "https://coverartarchive.org/release/rel/front-500".into(),
                sha256: "00".repeat(32),
                mime: "image/jpeg".into(),
                bytes: 12,
            }),
            false,
        )
    }

    fn enrichment() -> Enrichment {
        Enrichment {
            discogs_release_id: Some(123),
            release_title: "The EP".into(),
            label: "Defected".into(),
            catalog_number: "DFTD001".into(),
            year: Some(2014),
            styles: vec!["Tech House".into(), "Deep House".into()],
            genres: vec!["Electronic".into()],
        }
    }

    /// A track with a review result scoring `score`.
    fn reviewed(store: &Store, path: &str, top: f64) -> (TrackId, ResultId) {
        let track = store.upsert_track(&key(path), 360.0, Some("AQAB"), &tags("T")).unwrap();
        let result = store
            .save_result(track, Decision::Review, &[scored("T", top)], Some(&proposal()), None, &[], false)
            .unwrap();
        (track, result)
    }

    /// Writes `result`'s proposal, as Phase 4 will.
    fn apply(store: &Store, result: ResultId) {
        store.set_state(result, ReviewState::Accepted).unwrap();
        let path = store.result(result).unwrap().unwrap().0.path;
        let after = TrackKey { path, file_size: 2_000, modified_unix: 1_800_000_000 };
        record_applied(store, result, &proposal().changes, None, &snapshot("T"), &after, None).unwrap();
    }

    #[test]
    fn an_unwritten_auto_apply_becomes_a_pending_review_with_its_reason() {
        let store = Store::open_in_memory().unwrap();
        let track = store.upsert_track(&key("a.mp3"), 360.0, None, &tags("T")).unwrap();
        let auto = store.save_result(track, Decision::AutoApply, &[], Some(&proposal()), None, &[], false).unwrap();
        store.demote_auto_apply(auto, "the file is read-only").unwrap();
        let (item, _, _, warnings) = store.result(auto).unwrap().unwrap();
        assert_eq!((item.state, item.decision), (ReviewState::Pending, Decision::Review));
        assert_eq!(warnings, ["the file is read-only"]);
        assert!(store.demote_auto_apply(auto, "again").is_err(), "only an unwritten auto-apply");
        let review = store.save_result(track, Decision::Review, &[], Some(&proposal()), None, &[], false).unwrap();
        store.set_state(review, ReviewState::Accepted).unwrap();
        assert!(store.demote_auto_apply(review, "x").is_err(), "a user's accept is left alone");
    }

    #[test]
    fn a_new_store_has_the_schema() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        for table in ["tracks", "results", "applications", "application_snapshots", "artwork"] {
            assert!(store.has_table(table).unwrap(), "{table}");
        }
    }

    #[test]
    fn upsert_returns_the_same_id_and_keeps_the_latest_info() {
        let store = Store::open_in_memory().unwrap();
        let a = store.upsert_track(&key("a.mp3"), 360.0, Some("AQAB"), &tags("Old")).unwrap();
        let b = store.upsert_track(&key("a.mp3"), 361.0, None, &tags("New")).unwrap();
        assert_eq!(a, b);
        let (existing, fingerprint): (String, Option<String>) = store
            .conn
            .query_row("SELECT existing_json, fingerprint FROM tracks WHERE id = ?1", [a], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(from_json::<ExistingTags>(&existing, "x").unwrap().title, "New");
        assert_eq!(fingerprint.as_deref(), Some("AQAB"), "an unchanged file keeps its fingerprint");
        let changed = TrackKey { file_size: 5, ..key("a.mp3") };
        store.upsert_track(&changed, 361.0, None, &tags("New")).unwrap();
        let fingerprint: Option<String> =
            store.conn.query_row("SELECT fingerprint FROM tracks WHERE id = ?1", [a], |r| r.get(0)).unwrap();
        assert_eq!(fingerprint, None, "a changed file's old fingerprint is stale");
        assert_eq!(store.track_id("a.mp3").unwrap(), Some(a));
        assert_eq!(store.track_id("b.mp3").unwrap(), None);
    }

    /// Begins and completes a write in one go: the old one-step shape, for
    /// tests about everything but the two steps.
    fn record_applied(
        store: &Store,
        result: ResultId,
        applied: &[FieldChange],
        artwork: Option<&ArtworkRef>,
        before: &TagSnapshot,
        file_after: &TrackKey,
        library_snapshot: Option<&str>,
    ) -> Result<ApplicationId> {
        let app = store.begin_application(result, applied, artwork, before, library_snapshot)?;
        store.complete_application(app, file_after)?;
        Ok(app)
    }

    /// Accepts `result` and records a write whose before-state is `before`.
    fn write(store: &Store, result: ResultId, before: &TagSnapshot) -> ApplicationId {
        if store.result(result).unwrap().unwrap().0.state != ReviewState::Accepted {
            store.set_state(result, ReviewState::Accepted).unwrap();
        }
        let path = store.result(result).unwrap().unwrap().0.path;
        let after = TrackKey { path, file_size: 2_000, modified_unix: 1_800_000_000 };
        record_applied(store, result, &proposal().changes, None, before, &after, None).unwrap()
    }

    #[test]
    fn a_before_snapshot_round_trips_exactly() {
        let store = Store::open_in_memory().unwrap();
        // Every shape of raw_id3 comes back as it went in.
        for (n, raw) in [
            Some(RawId3 { v2: Some(BASE64.encode("ID3 header and frames")), v1: None }),
            None,
            Some(RawId3::default()),
            Some(RawId3 { v2: Some(String::new()), v1: Some(BASE64.encode([0u8; 128])) }),
        ]
        .into_iter()
        .enumerate()
        {
            let (track, result) = reviewed(&store, &format!("{n}.mp3"), 0.8);
            assert_eq!(store.original_snapshot(track).unwrap(), None, "nothing before the first write");
            let before = TagSnapshot { raw_id3: raw, ..snapshot("Original Title (Extended Mix)") };
            let app = write(&store, result, &before);
            assert_eq!(store.application_snapshot(app).unwrap(), Some(before.clone()));
            assert_eq!(store.original_snapshot(track).unwrap(), Some(before));
            assert_eq!(store.last_application(result).unwrap().unwrap().id, app);
        }
        assert_eq!(store.application_snapshot(9_999).unwrap(), None);
    }

    #[test]
    fn every_write_keeps_its_own_before_snapshot() {
        // apply -> undo -> hand edit -> re-identify -> apply: undoing the
        // second write must give back the hand edit, not the first original.
        let store = Store::open_in_memory().unwrap();
        let (track, first) = reviewed(&store, "a.mp3", 0.8);
        let original = snapshot("Original");
        let first_app = write(&store, first, &original);
        store.set_state(first, ReviewState::Undone).unwrap();

        let hand_edited = snapshot("Hand Edited");
        store.upsert_track(&TrackKey { file_size: 3_000, ..key("a.mp3") }, 360.0, None, &tags("Hand Edited")).unwrap();
        let second = store
            .save_result(track, Decision::Review, &[scored("U", 0.9)], Some(&proposal()), None, &[], false)
            .unwrap();
        let second_app = write(&store, second, &hand_edited);

        assert_ne!(first_app, second_app);
        assert_eq!(store.application_snapshot(second_app).unwrap(), Some(hand_edited));
        assert_eq!(store.application_snapshot(first_app).unwrap(), Some(original.clone()));
        assert_eq!(store.last_application(second).unwrap().unwrap().id, second_app);
        assert_eq!(store.original_snapshot(track).unwrap(), Some(original), "the first write's");
    }

    #[test]
    fn the_original_snapshot_is_never_replaced() {
        let store = Store::open_in_memory().unwrap();
        let (track, first) = reviewed(&store, "a.mp3", 0.8);
        let original = snapshot("Original");
        write(&store, first, &original);
        // A re-scan after the tags were applied (the file changed).
        let after = TrackKey { file_size: 2_000, modified_unix: 1_800_000_000, ..key("a.mp3") };
        store.upsert_track(&after, 360.0, None, &tags("Applied Title")).unwrap();
        // Undo, re-identify, re-apply; replace an applied result; auto-apply; undo.
        store.set_state(first, ReviewState::Undone).unwrap();
        let second =
            store.save_result(track, Decision::Review, &[scored("U", 0.9)], Some(&proposal()), None, &[], false).unwrap();
        write(&store, second, &snapshot("Second"));
        let third = store.save_result(track, Decision::AutoApply, &[], None, None, &[], true).unwrap();
        write(&store, third, &snapshot("Third"));
        store.set_state(third, ReviewState::Undone).unwrap();

        assert_eq!(store.original_snapshot(track).unwrap(), Some(original));
    }

    #[test]
    fn the_database_itself_refuses_to_change_a_snapshot_or_its_application() {
        let store = Store::open_in_memory().unwrap();
        let (track, result) = reviewed(&store, "a.mp3", 0.8);
        let app = write(&store, result, &snapshot("Original"));
        for statement in [
            "UPDATE application_snapshots SET snapshot_json = '{}'",
            "DELETE FROM application_snapshots",
            "INSERT INTO application_snapshots (application_id, snapshot_json, has_raw_id3, taken_unix)
             SELECT application_id, '{}', 0, 0 FROM application_snapshots",
            "INSERT OR REPLACE INTO application_snapshots (application_id, snapshot_json, has_raw_id3, taken_unix)
             SELECT application_id, '{}', 0, 0 FROM application_snapshots",
            "INSERT OR IGNORE INTO application_snapshots (application_id, snapshot_json, has_raw_id3, taken_unix)
             SELECT application_id, '{}', 0, 0 FROM application_snapshots",
        ] {
            let err = store.conn.execute(statement, []).unwrap_err();
            assert!(err.to_string().contains("immutable"), "{statement}: {err}");
        }
        for statement in ["UPDATE applications SET changes_json = '[]'", "DELETE FROM applications"] {
            let err = store.conn.execute(statement, []).unwrap_err();
            assert!(err.to_string().contains("permanent"), "{statement}: {err}");
        }
        assert_eq!(store.application_snapshot(app).unwrap(), Some(snapshot("Original")));
        assert_eq!(store.original_snapshot(track).unwrap(), Some(snapshot("Original")));
    }

    #[test]
    fn a_before_snapshot_with_base64_that_would_not_come_back_identical_is_refused() {
        let store = Store::open_in_memory().unwrap();
        let (track, result) = reviewed(&store, "a.mp3", 0.8);
        store.set_state(result, ReviewState::Accepted).unwrap();
        for bad in ["not base64!", "SUQz==" /* padded */] {
            let before = TagSnapshot { raw_id3: Some(RawId3 { v2: Some(bad.into()), v1: None }), ..snapshot("x") };
            assert!(record_applied(&store, result, &[], None, &before, &key("a.mp3"), None).is_err(), "{bad}");
        }
        // Nothing half-stored: no application, state unchanged.
        assert_eq!(store.last_application(result).unwrap(), None);
        assert_eq!(store.original_snapshot(track).unwrap(), None);
        assert_eq!(store.result(result).unwrap().unwrap().0.state, ReviewState::Accepted);
    }

    #[test]
    fn raw_id3_is_stored_as_bytes_not_base64() {
        let store = Store::open_in_memory().unwrap();
        let (_, result) = reviewed(&store, "a.mp3", 0.8);
        let v2: Vec<u8> = (0..2 * 1024 * 1024_u32).map(|i| i.wrapping_mul(2_654_435_761).to_le_bytes()[2]).collect();
        let v1 = vec![b'T'; 128];
        let before = TagSnapshot {
            raw_id3: Some(RawId3 { v2: Some(BASE64.encode(&v2)), v1: Some(BASE64.encode(&v1)) }),
            ..snapshot("x")
        };
        let app = write(&store, result, &before);
        let (stored_v2, stored_v1, json): (Vec<u8>, Vec<u8>, String) = store
            .conn
            .query_row("SELECT raw_id3_v2, raw_id3_v1, snapshot_json FROM application_snapshots", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(stored_v2, v2, "the BLOB is the raw bytes");
        assert_eq!(stored_v1, v1);
        assert!(json.len() < 1_000, "no base64 in the JSON part ({} bytes)", json.len());
        assert_eq!(store.application_snapshot(app).unwrap(), Some(before));
    }

    #[test]
    fn no_state_change_while_a_write_is_open() {
        let store = Store::open_in_memory().unwrap();
        let (_, result) = reviewed(&store, "a.mp3", 0.8);
        store.set_state(result, ReviewState::Accepted).unwrap();
        let app = store.begin_application(result, &proposal().changes, None, &snapshot("Before"), None).unwrap();
        for state in [ReviewState::Rejected, ReviewState::Pending] {
            let err = store.set_state(result, state).unwrap_err().to_string();
            assert!(err.contains("write is in progress"), "{state:?}: {err}");
        }
        assert_eq!(store.result(result).unwrap().unwrap().0.state, ReviewState::Accepted);
        // Completing the write lifts it; undo then works as before.
        let after = TrackKey { file_size: 2_000, modified_unix: 1_800_000_000, ..key("a.mp3") };
        store.complete_application(app, &after).unwrap();
        store.set_state(result, ReviewState::Undone).unwrap();
    }

    #[test]
    fn the_newest_result_is_current_and_older_ones_stay() {
        let store = Store::open_in_memory().unwrap();
        let (track, first) = reviewed(&store, "a.mp3", 0.6);
        let second = store
            .save_result(track, Decision::Review, &[scored("B", 0.8)], Some(&proposal()), None, &[], false)
            .unwrap();
        assert_ne!(first, second);
        let all = store.list(Filter::All, 0, 10).unwrap();
        assert_eq!(all.len(), 1, "one current result per track");
        assert_eq!(all[0].result, second);
        // The first is history: still readable, no longer changeable.
        assert!(store.result(first).unwrap().is_some());
        assert!(store.set_state(first, ReviewState::Accepted).is_err());
        assert_eq!(store.count(Filter::All).unwrap(), 1);
    }

    #[test]
    fn a_new_result_starts_in_the_state_its_decision_implies() {
        let store = Store::open_in_memory().unwrap();
        for (path, decision, state) in [
            ("a", Decision::Review, ReviewState::Pending),
            ("b", Decision::Unidentified, ReviewState::Unidentified),
            // Not AutoApplied before anything is written.
            ("c", Decision::AutoApply, ReviewState::Accepted),
        ] {
            let track = store.upsert_track(&key(path), 1.0, None, &tags("T")).unwrap();
            let id = store.save_result(track, decision, &[], None, None, &[], false).unwrap();
            let (item, ..) = store.result(id).unwrap().unwrap();
            assert_eq!((item.state, item.decision, item.score), (state, decision, None));
        }
    }

    #[test]
    fn the_state_machine_allows_only_its_transitions() {
        use ReviewState::{Accepted, Applied, AutoApplied, Pending, Rejected, Undone, Unidentified};
        let allowed = [
            (Pending, Accepted),
            (Pending, Rejected),
            (Accepted, Pending),
            (Accepted, Rejected),
            (Rejected, Pending),
            (Applied, Undone),
            (AutoApplied, Undone),
            (Undone, Pending),
        ];
        for from in ReviewState::ALL {
            for to in ReviewState::ALL {
                assert_eq!(from.may_become(to), allowed.contains(&(from, to)), "{from:?} -> {to:?}");
            }
        }
        assert!(!Unidentified.may_become(Pending));
        assert!(!Accepted.may_become(Applied), "only complete_application reaches Applied");
    }

    #[test]
    fn set_state_follows_the_table_in_the_store() {
        let store = Store::open_in_memory().unwrap();
        let (_, result) = reviewed(&store, "a.mp3", 0.8);
        let state = |s: &Store| s.result(result).unwrap().unwrap().0.state;

        assert!(store.set_state(result, ReviewState::Undone).is_err());
        assert!(store.set_state(result, ReviewState::Applied).is_err());
        assert!(store.set_state(result, ReviewState::Pending).is_err(), "a state to itself is refused");
        assert_eq!(state(&store), ReviewState::Pending, "a refused change changes nothing");

        store.set_state(result, ReviewState::Rejected).unwrap();
        assert!(store.set_state(result, ReviewState::Accepted).is_err());
        store.set_state(result, ReviewState::Pending).unwrap();
        store.set_state(result, ReviewState::Accepted).unwrap();
        assert_eq!(state(&store), ReviewState::Accepted);
        assert!(store.set_state(9_999, ReviewState::Accepted).is_err());
    }

    #[test]
    fn an_application_keeps_exactly_what_was_written() {
        let store = Store::open_in_memory().unwrap();
        let (_, result) = reviewed(&store, "a.mp3", 0.8);
        let written = vec![FieldChange { field: Field::Label, current: String::new(), proposed: "Defected".into() }];
        let art = proposal().artwork;
        let after = TrackKey { file_size: 2_048, modified_unix: 1_800_000_123, ..key("a.mp3") };

        // Not before it is accepted.
        assert!(store.begin_application(result, &written, art.as_ref(), &snapshot("T"), None).is_err());
        store.set_state(result, ReviewState::Accepted).unwrap();
        let app = store
            .begin_application(result, &written, art.as_ref(), &snapshot("T"), Some(r#"{"content_id":7}"#))
            .unwrap();
        // Not completed for another file; that changes nothing.
        let other = TrackKey { path: "b.mp3".into(), ..after.clone() };
        assert!(store.complete_application(app, &other).is_err());
        assert_eq!(store.application(app).unwrap().unwrap().outcome, None);

        store.complete_application(app, &after).unwrap();
        assert_eq!(store.applied_changes(result).unwrap(), written);
        let application = store.last_application(result).unwrap().unwrap();
        assert_eq!((application.id, application.result, application.path.as_str()), (app, result, "a.mp3"));
        assert_eq!(application.artwork, art);
        assert_eq!(application.library_snapshot.as_deref(), Some(r#"{"content_id":7}"#));
        assert!(application.begun_unix > 0);
        let Some(Outcome::Written { file_after, completed_unix }) = application.outcome else {
            panic!("not written: {:?}", application.outcome);
        };
        assert_eq!(file_after, after);
        assert!(completed_unix > 0);
        assert_eq!(store.result(result).unwrap().unwrap().0.state, ReviewState::Applied);
        // The track takes the file's new size and time.
        let (size, modified): (i64, i64) = store
            .conn
            .query_row("SELECT file_size, modified_unix FROM tracks WHERE path = 'a.mp3'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((size, modified), (2_048, 1_800_000_123));
        // Completed twice, or begun again once applied: refused.
        assert!(store.complete_application(app, &after).is_err());
        assert!(store.abandon_application(app, "late").is_err());
        assert!(store.begin_application(result, &written, None, &snapshot("T"), None).is_err());
        assert_eq!(store.applied_changes(9_999).unwrap(), Vec::new());
        assert!(store.complete_application(9_999, &after).is_err());
    }

    #[test]
    fn a_begun_application_is_safe_before_the_file_is_touched() {
        let store = Store::open_in_memory().unwrap();
        let (track, result) = reviewed(&store, "a.mp3", 0.8);
        store.set_state(result, ReviewState::Accepted).unwrap();
        let before = snapshot("Before");
        let app = store.begin_application(result, &proposal().changes, None, &before, None).unwrap();

        // The process could die here: the before-state is already stored.
        assert_eq!(store.application_snapshot(app).unwrap(), Some(before));
        assert_eq!(store.result(result).unwrap().unwrap().0.state, ReviewState::Accepted);
        assert_eq!(store.count(Filter::Applied).unwrap(), 0);
        assert_eq!(store.last_application(result).unwrap(), None, "not undoable: nothing written");
        assert_eq!(store.original_snapshot(track).unwrap(), None);
        let open = store.open_applications().unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!((open[0].id, open[0].outcome.as_ref()), (app, None));
        assert_eq!(open[0].changes, proposal().changes);

        // One write in flight per track; no re-identification under it.
        assert!(store.begin_application(result, &proposal().changes, None, &snapshot("x"), None).is_err());
        assert!(store.save_result(track, Decision::Review, &[], None, None, &[], true).is_err());

        store.complete_application(app, &key("a.mp3")).unwrap();
        assert_eq!(store.open_applications().unwrap(), Vec::new());
        assert_eq!(store.result(result).unwrap().unwrap().0.state, ReviewState::Applied);
        assert_eq!(store.last_application(result).unwrap().unwrap().id, app);
        assert_eq!(store.original_snapshot(track).unwrap(), Some(snapshot("Before")));
    }

    #[test]
    fn an_abandoned_application_never_counts_for_undo() {
        let store = Store::open_in_memory().unwrap();
        let (track, result) = reviewed(&store, "a.mp3", 0.8);
        store.set_state(result, ReviewState::Accepted).unwrap();
        let abandoned = store.begin_application(result, &proposal().changes, None, &snapshot("Never"), None).unwrap();
        store.abandon_application(abandoned, "disk full").unwrap();

        assert_eq!(store.result(result).unwrap().unwrap().0.state, ReviewState::Accepted);
        assert_eq!(store.open_applications().unwrap(), Vec::new());
        assert_eq!(store.last_application(result).unwrap(), None);
        assert_eq!(store.original_snapshot(track).unwrap(), None);
        let record = store.application(abandoned).unwrap().unwrap();
        assert!(matches!(record.outcome, Some(Outcome::Abandoned { ref reason, .. }) if reason == "disk full"));
        assert!(store.complete_application(abandoned, &key("a.mp3")).is_err(), "an outcome is final");

        // A retry is a new application; only it counts.
        let retried = store.begin_application(result, &proposal().changes, None, &snapshot("Real"), None).unwrap();
        store.complete_application(retried, &key("a.mp3")).unwrap();
        assert_eq!(store.last_application(result).unwrap().unwrap().id, retried);
        assert_eq!(store.original_snapshot(track).unwrap(), Some(snapshot("Real")));
        // The abandoned one's snapshot is kept, but nothing points undo at it.
        assert_eq!(store.application_snapshot(abandoned).unwrap(), Some(snapshot("Never")));
    }

    #[test]
    fn a_version_1_store_is_migrated_and_keeps_its_rows_and_rules() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identify.db");
        {
            // A store exactly as version 1 left it, with one written result.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.pragma_update(None, "application_id", APPLICATION_ID).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
            conn.execute_batch(
                r#"INSERT INTO tracks VALUES (1, 'old.mp3', 1000, 1700000000, 360.0, NULL,
                       '{"artist":"","title":"","album":"","label":"","year":null,"isrc":"","genre":""}', 1, 1);
                   INSERT INTO results (id, track_id, is_current, decision, state, score, candidates_json,
                                        proposal_json, warnings_json, created_unix, updated_unix)
                       VALUES (1, 1, 1, 'Review', 'Applied', 0.8, '[]', NULL, '[]', 1, 1);
                   INSERT INTO applications VALUES (1, 1, '[]', NULL, '{}', 1);
                   INSERT INTO application_snapshots
                       VALUES (1, '{"format":"Id3v2","values":{},"has_artwork":false}', 0, NULL, NULL, 1);
                   INSERT INTO application_outcomes VALUES (1, 'written', 1, 1, NULL, 1);"#,
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        assert!(store.has_table("application_undos").unwrap());
        assert!(store.has_table("rekordbox_imports").unwrap());
        assert_eq!(store.rekordbox_record(1).unwrap(), None, "nothing was imported before version 3");
        let item = &store.list(Filter::All, 0, 10).unwrap()[0];
        assert_eq!((item.path.as_str(), item.state, item.library.as_ref()), ("old.mp3", ReviewState::Applied, None));
        let app = store.last_application(item.result).unwrap().unwrap();
        assert_eq!((app.library_after, app.undone_unix), (None, None));
        assert_eq!(store.application_after_snapshot(app.id).unwrap(), None, "version 1 kept no after-state");
        // The insert-only rules still hold, the new table's too.
        for statement in ["DELETE FROM application_outcomes", "UPDATE applications SET begun_unix = 0"] {
            assert!(store.conn.execute(statement, []).is_err(), "{statement}");
        }
        store.record_undo(app.id).unwrap();
        assert!(store.conn.execute("DELETE FROM application_undos", []).is_err());
        assert!(store.conn.execute("UPDATE application_undos SET undone_unix = 0", []).is_err());
        drop(store);
        // Opening again is a no-op.
        assert_eq!(Store::open(&path).unwrap().schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn a_version_2_store_gains_the_rekordbox_imports_and_keeps_its_results() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identify.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(SCHEMA_V2).unwrap();
            conn.pragma_update(None, "application_id", APPLICATION_ID).unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        assert!(store.has_table("rekordbox_imports").unwrap());
        let (_, old) = store.save_track_result(&key("a.mp3"), &track_result(None, None), None, false).unwrap();
        assert_eq!(store.rekordbox_record(old).unwrap(), None);
    }

    #[test]
    fn a_rekordbox_record_is_imported_with_its_result_and_never_changed() {
        let store = Store::open_in_memory().unwrap();
        let record = RekordboxRecord {
            track_id: "123456".into(),
            title: "Heart (Original Mix)".into(),
            artist: "Someone".into(),
            album: String::new(),
            label: "Label".into(),
            genre: "House".into(),
            year: "2014".into(),
            comment: "a note".into(),
            isrc: "GBAAA1400001".into(),
            imported_unix: 1_800_000_000,
        };
        let library = LibraryTrack { track_id: "123456".into(), path: "D:/Music/a.mp3".into() };
        let (_, saved) = store
            .save_track_result_with(&key("a.mp3"), &track_result(None, None), Some(&library), Some(&record), false)
            .unwrap();
        assert_eq!(store.rekordbox_record(saved).unwrap(), Some(record.clone()));
        for statement in ["UPDATE rekordbox_imports SET title = 'x'", "DELETE FROM rekordbox_imports"] {
            assert!(store.conn.execute(statement, []).is_err(), "{statement}");
        }
        assert!(insert_rekordbox_record(&store.conn, saved, &record).is_err(), "one per result");
        // A refused save imports nothing either.
        apply(&store, saved);
        let again = store.save_track_result_with(&key("a.mp3"), &track_result(None, None), None, Some(&record), false);
        assert!(again.is_err());
        let count: i64 = store.conn.query_row("SELECT COUNT(*) FROM rekordbox_imports", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn a_track_result_keeps_its_library_track_and_fingerprint() {
        let store = Store::open_in_memory().unwrap();
        let mut result = track_result(None, None);
        result.encoded_fingerprint = Some("AQADtEmUaEkS".into());
        let library = LibraryTrack { track_id: "123456".into(), path: "D:/Music/a.mp3".into() };
        let (track, saved) = store.save_track_result(&key("a.mp3"), &result, Some(&library), false).unwrap();
        assert_eq!(store.result(saved).unwrap().unwrap().0.library, Some(library));
        let print: Option<String> =
            store.conn.query_row("SELECT fingerprint FROM tracks WHERE id = ?1", [track], |r| r.get(0)).unwrap();
        assert_eq!(print.as_deref(), Some("AQADtEmUaEkS"));
        // A later run without one keeps it for the unchanged file.
        result.encoded_fingerprint = None;
        store.save_track_result(&key("a.mp3"), &result, None, false).unwrap();
        let print: Option<String> =
            store.conn.query_row("SELECT fingerprint FROM tracks WHERE id = ?1", [track], |r| r.get(0)).unwrap();
        assert_eq!(print.as_deref(), Some("AQADtEmUaEkS"));
    }

    #[test]
    fn a_refused_track_result_leaves_no_image_or_track_change_behind() {
        let store = Store::open_in_memory().unwrap();
        let (track, result) = reviewed(&store, "a.mp3", 0.8);
        apply(&store, result);
        let before: (i64, String) = store
            .conn
            .query_row("SELECT file_size, existing_json FROM tracks WHERE id = ?1", [track], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        let bytes: &[u8] = b"\xFF\xD8\xFF\xE0 new";
        let mut again = track_result(Some(bytes), Some(sha256_hex(bytes)));
        again.existing = tags("Changed");
        let changed = TrackKey { file_size: 4_321, ..key("a.mp3") };
        // The track's result is applied: refused, and nothing is kept.
        assert!(store.save_track_result(&changed, &again, None, false).is_err());
        assert_eq!(store.artwork(&sha256_hex(bytes)).unwrap(), None, "no orphan image");
        let after: (i64, String) = store
            .conn
            .query_row("SELECT file_size, existing_json FROM tracks WHERE id = ?1", [track], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(after, before, "the track row is untouched");
    }

    #[test]
    fn an_edited_auto_apply_result_becomes_a_review_and_the_sub_genre_is_not_editable() {
        let store = Store::open_in_memory().unwrap();
        let track = store.upsert_track(&key("a.mp3"), 360.0, None, &tags("T")).unwrap();
        let mut auto = proposal();
        auto.changes.push(FieldChange { field: Field::SubGenre, current: "c".into(), proposed: "c; Acid".into() });
        let result =
            store.save_result(track, Decision::AutoApply, &[scored("T", 0.97)], Some(&auto), None, &[], false).unwrap();
        assert!(store.edit_proposal(result, &[(Field::SubGenre, "Acid".into())]).is_err());
        assert_eq!(store.result(result).unwrap().unwrap().0.decision, Decision::AutoApply, "a refused edit changes nothing");
        store.edit_proposal(result, &[(Field::Label, "Mine".into())]).unwrap();
        let item = store.result(result).unwrap().unwrap().0;
        assert_eq!((item.decision, item.state), (Decision::Review, ReviewState::Accepted));
        // Written now, it is a reviewed write, not an unattended one.
        let after = TrackKey { path: item.path, file_size: 2, modified_unix: 2 };
        record_applied(&store, result, &auto.changes, None, &snapshot("T"), &after, None).unwrap();
        assert_eq!(store.result(result).unwrap().unwrap().0.state, ReviewState::Applied);
    }

    #[test]
    fn an_undo_is_recorded_once_and_only_for_the_latest_write() {
        let store = Store::open_in_memory().unwrap();
        let (track, result) = reviewed(&store, "a.mp3", 0.8);
        store.set_state(result, ReviewState::Accepted).unwrap();
        let app = store.begin_application(result, &proposal().changes, None, &snapshot("T"), Some("{\"lib\":1}")).unwrap();
        let file_after = TrackKey { path: "a.mp3".into(), file_size: 2, modified_unix: 2 };
        assert!(store.record_undo(app).is_err(), "an open write cannot be undone");
        store.complete_application_with(app, &file_after, Some(&snapshot("New")), Some("{\"lib\":2}")).unwrap();
        assert_eq!(store.application_after_snapshot(app).unwrap(), Some(snapshot("New")));
        assert_eq!(store.latest_written_application(track).unwrap().map(|a| a.id), Some(app));
        let written = store.application(app).unwrap().unwrap();
        assert_eq!(written.library_after.as_deref(), Some("{\"lib\":2}"));

        store.record_undo(app).unwrap();
        assert_eq!(store.result(result).unwrap().unwrap().0.state, ReviewState::Undone);
        assert!(store.application(app).unwrap().unwrap().undone_unix.is_some());
        assert!(store.record_undo(app).is_err(), "only once");

        // Re-applied: the older write can no longer be undone.
        store.set_state(result, ReviewState::Pending).unwrap();
        let second = write(&store, result, &snapshot("T"));
        let err = store.record_undo(app).unwrap_err().to_string();
        assert!(err.contains("already been undone"), "{err}");
        store.record_undo(second).unwrap();
    }

    #[test]
    fn outcomes_and_applications_are_insert_only() {
        let store = Store::open_in_memory().unwrap();
        let (_, result) = reviewed(&store, "a.mp3", 0.8);
        let app = write(&store, result, &snapshot("T"));
        for statement in [
            "UPDATE application_outcomes SET outcome = 'abandoned', file_size_after = NULL, modified_unix_after = NULL",
            "DELETE FROM application_outcomes",
            "INSERT INTO application_outcomes (application_id, outcome, reason, completed_unix)
             SELECT application_id, 'abandoned', 'x', 0 FROM application_outcomes",
            "INSERT OR REPLACE INTO application_outcomes (application_id, outcome, reason, completed_unix)
             SELECT application_id, 'abandoned', 'x', 0 FROM application_outcomes",
            "INSERT OR IGNORE INTO application_outcomes (application_id, outcome, reason, completed_unix)
             SELECT application_id, 'abandoned', 'x', 0 FROM application_outcomes",
            "INSERT OR REPLACE INTO applications (id, result_id, changes_json, begun_unix)
             SELECT id, result_id, '[]', 0 FROM applications",
        ] {
            let err = store.conn.execute(statement, []).unwrap_err();
            assert!(err.to_string().contains("permanent record"), "{statement}: {err}");
        }
        assert!(matches!(store.application(app).unwrap().unwrap().outcome, Some(Outcome::Written { .. })));
        assert_eq!(store.applied_changes(result).unwrap(), proposal().changes);
    }

    #[test]
    fn an_auto_apply_result_becomes_auto_applied_only_once_written() {
        let store = Store::open_in_memory().unwrap();
        let track = store.upsert_track(&key("a.mp3"), 1.0, None, &tags("T")).unwrap();
        let result = store.save_result(track, Decision::AutoApply, &[scored("T", 0.95)], None, None, &[], false).unwrap();
        let state = |s: &Store| s.result(result).unwrap().unwrap().0.state;
        let applications = |s: &Store| -> i64 {
            s.conn.query_row("SELECT COUNT(*) FROM applications", [], |r| r.get(0)).unwrap()
        };

        // The write failed before it began: still Accepted,
        // nothing recorded, not in the undoable list.
        assert_eq!(state(&store), ReviewState::Accepted);
        assert_eq!(applications(&store), 0);
        assert_eq!(store.count(Filter::Applied).unwrap(), 0);
        // set_state cannot fake it.
        assert!(store.set_state(result, ReviewState::AutoApplied).is_err());

        record_applied(&store, result, &proposal().changes, None, &snapshot("T"), &key("a.mp3"), None).unwrap();
        assert_eq!(state(&store), ReviewState::AutoApplied);
        assert_eq!(applications(&store), 1);
        assert_eq!(store.count(Filter::Applied).unwrap(), 1);
        assert!(record_applied(&store, result, &proposal().changes, None, &snapshot("T"), &key("a.mp3"), None).is_err());

        // Undone and re-accepted by the user: a review, so Applied.
        store.set_state(result, ReviewState::Undone).unwrap();
        store.set_state(result, ReviewState::Pending).unwrap();
        store.set_state(result, ReviewState::Accepted).unwrap();
        record_applied(&store, result, &proposal().changes, None, &snapshot("T"), &key("a.mp3"), None).unwrap();
        assert_eq!(state(&store), ReviewState::Applied);
    }

    #[test]
    fn a_user_accepted_result_becomes_applied() {
        let store = Store::open_in_memory().unwrap();
        let (_, result) = reviewed(&store, "a.mp3", 0.95);
        apply(&store, result);
        assert_eq!(store.result(result).unwrap().unwrap().0.state, ReviewState::Applied);
    }

    #[test]
    fn applied_states_only_ever_exist_with_an_application_record() {
        let store = Store::open_in_memory().unwrap();
        let (_, r1) = reviewed(&store, "a.mp3", 0.9);
        apply(&store, r1);
        let t2 = store.upsert_track(&key("b.mp3"), 1.0, None, &tags("T")).unwrap();
        let r2 = store.save_result(t2, Decision::AutoApply, &[scored("T", 0.95)], None, None, &[], false).unwrap();
        record_applied(&store, r2, &proposal().changes, None, &snapshot("T"), &key("b.mp3"), None).unwrap();
        let t3 = store.upsert_track(&key("c.mp3"), 1.0, None, &tags("T")).unwrap();
        store.save_result(t3, Decision::AutoApply, &[], None, None, &[], false).unwrap();
        let orphans: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM results r WHERE r.state IN ('Applied', 'AutoApplied')
                 AND NOT EXISTS (SELECT 1 FROM applications a WHERE a.result_id = r.id)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(orphans, 0);
        assert_eq!(store.count(Filter::Applied).unwrap(), 2);
    }

    #[test]
    fn a_re_apply_after_undo_keeps_both_records() {
        let store = Store::open_in_memory().unwrap();
        let (_, result) = reviewed(&store, "a.mp3", 0.8);
        apply(&store, result);
        store.set_state(result, ReviewState::Undone).unwrap();
        store.set_state(result, ReviewState::Pending).unwrap();
        store.set_state(result, ReviewState::Accepted).unwrap();
        let only_label = &proposal().changes[..1];
        record_applied(&store, result, only_label, None, &snapshot("T"), &key("a.mp3"), None).unwrap();
        assert_eq!(store.applied_changes(result).unwrap(), only_label);
        let n: i64 = store.conn.query_row("SELECT COUNT(*) FROM applications", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn an_applied_result_is_not_replaced_unless_allowed() {
        let store = Store::open_in_memory().unwrap();
        let (track, result) = reviewed(&store, "a.mp3", 0.8);
        apply(&store, result);
        let err = store.save_result(track, Decision::Review, &[], None, None, &[], false).unwrap_err();
        assert!(matches!(err, IdentifyError::Store(_)));
        assert_eq!(store.list(Filter::All, 0, 10).unwrap()[0].result, result, "still current");
        let newer = store.save_result(track, Decision::Review, &[], None, None, &[], true).unwrap();
        assert_eq!(store.list(Filter::All, 0, 10).unwrap()[0].result, newer);

        // AutoApplied is applied too; an auto-apply not yet written is not.
        let t2 = store.upsert_track(&key("b.mp3"), 1.0, None, &tags("T")).unwrap();
        let r2 = store.save_result(t2, Decision::AutoApply, &[], None, None, &[], false).unwrap();
        record_applied(&store, r2, &proposal().changes, None, &snapshot("T"), &key("b.mp3"), None).unwrap();
        assert!(store.save_result(t2, Decision::Review, &[], None, None, &[], false).is_err());
        let t4 = store.upsert_track(&key("d.mp3"), 1.0, None, &tags("T")).unwrap();
        store.save_result(t4, Decision::AutoApply, &[], None, None, &[], false).unwrap();
        store.save_result(t4, Decision::Review, &[], None, None, &[], false).unwrap();
        // An undone one may be replaced.
        let (t3, r3) = reviewed(&store, "c.mp3", 0.8);
        apply(&store, r3);
        store.set_state(r3, ReviewState::Undone).unwrap();
        store.save_result(t3, Decision::Review, &[], None, None, &[], false).unwrap();
    }

    #[test]
    fn the_queue_filters_orders_and_pages() {
        let store = Store::open_in_memory().unwrap();
        reviewed(&store, "b.mp3", 0.7);
        reviewed(&store, "a.mp3", 0.7);
        reviewed(&store, "c.mp3", 0.9);
        let none = store.upsert_track(&key("d.mp3"), 1.0, None, &tags("T")).unwrap();
        store.save_result(none, Decision::Unidentified, &[], None, None, &[], false).unwrap();
        let (_, r) = reviewed(&store, "e.mp3", 0.6);
        apply(&store, r);
        let auto = store.upsert_track(&key("f.mp3"), 1.0, None, &tags("T")).unwrap();
        let r = store.save_result(auto, Decision::AutoApply, &[scored("F", 0.95)], None, None, &[], false).unwrap();
        record_applied(&store, r, &proposal().changes, None, &snapshot("T"), &key("f.mp3"), None).unwrap();
        // A track with no result is not in the queue.
        store.upsert_track(&key("g.mp3"), 1.0, None, &tags("T")).unwrap();

        let paths = |filter, offset, limit| -> Vec<String> {
            store.list(filter, offset, limit).unwrap().into_iter().map(|i| i.path).collect()
        };
        assert_eq!(paths(Filter::NeedsReview, 0, 10), ["c.mp3", "a.mp3", "b.mp3"]);
        assert_eq!(paths(Filter::Unidentified, 0, 10), ["d.mp3"]);
        assert_eq!(paths(Filter::Applied, 0, 10), ["f.mp3", "e.mp3"]);
        assert_eq!(paths(Filter::All, 0, 10), ["f.mp3", "c.mp3", "a.mp3", "b.mp3", "e.mp3", "d.mp3"]);
        assert_eq!(paths(Filter::All, 2, 2), ["a.mp3", "b.mp3"]);
        assert_eq!(paths(Filter::All, 6, 2), Vec::<String>::new());
        assert_eq!(store.count(Filter::NeedsReview).unwrap(), 3);
        assert_eq!(store.count(Filter::Unidentified).unwrap(), 1);
        assert_eq!(store.count(Filter::Applied).unwrap(), 2);
        assert_eq!(store.count(Filter::All).unwrap(), 6);

        let item = &store.list(Filter::NeedsReview, 0, 1).unwrap()[0];
        assert_eq!(item.score, Some(0.9));
        assert_eq!(item.existing, tags("T"));
        assert_eq!(item.proposal, Some(proposal()));
        assert!(item.updated_unix > 0);
    }

    #[test]
    fn bulk_accept_takes_pending_results_at_or_above_the_threshold() {
        let store = Store::open_in_memory().unwrap();
        let (_, low) = reviewed(&store, "a.mp3", 0.79);
        let (_, at) = reviewed(&store, "b.mp3", 0.8);
        let (_, high) = reviewed(&store, "c.mp3", 0.95);
        let (_, rejected) = reviewed(&store, "d.mp3", 0.99);
        store.set_state(rejected, ReviewState::Rejected).unwrap();
        // A superseded high scorer is history, not accepted.
        let (t, old) = reviewed(&store, "e.mp3", 0.99);
        store.save_result(t, Decision::Review, &[scored("E", 0.5)], None, None, &[], false).unwrap();

        assert_eq!(store.accept_at_or_above(0.8).unwrap(), [high, at]);
        let state = |id| store.result(id).unwrap().unwrap().0.state;
        assert_eq!(state(high), ReviewState::Accepted);
        assert_eq!(state(at), ReviewState::Accepted);
        assert_eq!(state(low), ReviewState::Pending);
        assert_eq!(state(rejected), ReviewState::Rejected);
        assert_eq!(state(old), ReviewState::Pending);
        assert_eq!(store.accept_at_or_above(0.8).unwrap(), Vec::<ResultId>::new(), "nothing twice");
        assert!(store.accept_at_or_above(f64::NAN).is_err());
    }

    #[test]
    fn artwork_is_stored_once_per_content() {
        let store = Store::open_in_memory().unwrap();
        let png = [0x89, b'P', b'N', b'G', 1, 2, 3];
        let hash = store.put_artwork(&png, "image/png").unwrap();
        // SHA-256 of the empty string, to pin the encoding.
        assert_eq!(
            store.put_artwork(&[], "image/jpeg").unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(store.put_artwork(&png, "image/png").unwrap(), hash);
        let n: i64 = store.conn.query_row("SELECT COUNT(*) FROM artwork", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2);
        assert_eq!(store.artwork(&hash).unwrap(), Some((png.to_vec(), "image/png".to_owned())));
        assert_eq!(store.artwork(&"0".repeat(64)).unwrap(), None);
    }

    #[test]
    fn candidates_proposal_enrichment_and_warnings_round_trip() {
        let store = Store::open_in_memory().unwrap();
        let track = store.upsert_track(&key("a.mp3"), 1.0, None, &tags("T")).unwrap();
        let candidates = vec![scored("Best (Extended Mix)", 0.93), scored("Other", 0.4)];
        let warnings = vec!["Discogs: HTTP 503".to_owned()];
        let id = store
            .save_result(track, Decision::Review, &candidates, Some(&proposal()), Some(&enrichment()), &warnings, false)
            .unwrap();
        let (item, got, enr, warn) = store.result(id).unwrap().unwrap();
        assert_eq!(got, candidates);
        assert_eq!(item.proposal, Some(proposal()));
        assert_eq!(item.score, Some(0.93));
        assert_eq!(enr, Some(enrichment()));
        assert_eq!(warn, warnings);
        assert_eq!(store.result(9_999).unwrap().map(|r| r.0), None);
    }

    #[test]
    fn a_windows_path_round_trips_exactly() {
        let store = Store::open_in_memory().unwrap();
        let path = "C:\\Music\\Röyksopp\\Track.mp3";
        let (track, _) = reviewed(&store, path, 0.8);
        assert_eq!(store.list(Filter::All, 0, 1).unwrap()[0].path, path);
        assert_eq!(store.track_id(path).unwrap(), Some(track));
        // Another spelling is another key: the store does not normalise.
        assert_eq!(store.track_id("C:/Music/Röyksopp/Track.mp3").unwrap(), None);
    }

    fn track_result(image: Option<&[u8]>, named: Option<String>) -> TrackResult {
        let art = named.map(|sha256| ArtworkRef {
            source: ArtworkSource::CoverArtArchive,
            url: "https://coverartarchive.org/release/rel/front-500".into(),
            sha256,
            mime: "image/jpeg".into(),
            bytes: 4,
        });
        TrackResult {
            path: "a.mp3".into(),
            existing: tags("T"),
            duration_secs: 360.0,
            fingerprint: crate::pipeline::FingerprintOutcome::default(),
            encoded_fingerprint: None,
            candidates: vec![scored("T", 0.8)],
            decision: Decision::Review,
            snapshot: snapshot("T"),
            proposal: Some(Proposal::new(
                vec![FieldChange { field: Field::Label, current: String::new(), proposed: "Defected".into() }],
                art,
                false,
            )),
            enrichment: Some(enrichment()),
            artwork: image.map(|bytes| crate::coverart::FetchedImage {
                bytes: bytes.to_vec(),
                mime: "image/jpeg".into(),
                url: "https://coverartarchive.org/release/rel/front-500".into(),
            }),
            warnings: vec!["Discogs: slow".into()],
        }
    }

    #[test]
    fn a_track_result_is_saved_with_its_artwork_in_one_call() {
        let store = Store::open_in_memory().unwrap();
        let bytes: &[u8] = b"\xFF\xD8\xFF\xE0";
        let result = track_result(Some(bytes), Some(sha256_hex(bytes)));
        let (track, saved) = store.save_track_result(&key("a.mp3"), &result, None, false).unwrap();
        assert_eq!(store.track_id("a.mp3").unwrap(), Some(track));
        let (item, candidates, enrichment, warnings) = store.result(saved).unwrap().unwrap();
        assert_eq!((item.state, item.proposal.as_ref()), (ReviewState::Pending, result.proposal.as_ref()));
        assert_eq!((candidates, enrichment, warnings), (result.candidates.clone(), result.enrichment.clone(), result.warnings.clone()));
        // The bytes are in the store under the hash the proposal names.
        let named = item.proposal.unwrap().artwork.unwrap().sha256;
        assert_eq!(store.artwork(&named).unwrap(), Some((bytes.to_vec(), "image/jpeg".to_owned())));
        // Saved again without the bytes: the stored copy still backs it.
        let again = track_result(None, Some(named));
        let (same_track, newer) = store.save_track_result(&key("a.mp3"), &again, None, false).unwrap();
        assert_eq!(same_track, track);
        assert_ne!(newer, saved);
        // No artwork at all is fine too.
        store.save_track_result(&key("b.mp3"), &track_result(None, None), None, false).unwrap();
    }

    #[test]
    fn a_track_result_whose_artwork_does_not_add_up_is_refused_whole() {
        let store = Store::open_in_memory().unwrap();
        let bytes: &[u8] = b"\xFF\xD8\xFF\xE0";
        let mismatched = track_result(Some(bytes), Some("00".repeat(32)));
        assert!(store.save_track_result(&key("a.mp3"), &mismatched, None, false).is_err());
        let missing = track_result(None, Some("11".repeat(32)));
        assert!(store.save_track_result(&key("a.mp3"), &missing, None, false).is_err());
        assert_eq!(store.track_id("a.mp3").unwrap(), None, "nothing was written");
        assert_eq!(store.artwork(&sha256_hex(bytes)).unwrap(), None);
    }

    /// A track with an accepted result and a write begun but not finished.
    fn interrupted(store: &Store, path: &str, before: &TagSnapshot) -> (ResultId, ApplicationId) {
        let (_, result) = reviewed(store, path, 0.8);
        store.set_state(result, ReviewState::Accepted).unwrap();
        let app = store.begin_application(result, &proposal().changes, None, before, None).unwrap();
        (result, app)
    }

    fn read(tags: TagSnapshot) -> FileNow {
        FileNow::Read { tags, key: TrackKey { path: "ignored".into(), file_size: 2_000, modified_unix: 1_800_000_000 } }
    }

    /// `before` as writing `proposal()` over it leaves it.
    fn written_over(before: &TagSnapshot) -> TagSnapshot {
        let mut after = before.clone();
        after.values.insert(Field::Label, vec![FrameValues { frame: "TPUB".into(), values: vec!["Defected".into()] }]);
        after.has_artwork = proposal().changes.iter().any(|c| c.field == Field::Artwork);
        after.raw_id3 = Some(RawId3 { v2: Some(BASE64.encode("ID3 written")), v1: None });
        after
    }

    #[test]
    fn reconciling_tells_unchanged_written_unknown_and_missing_files_apart() {
        let store = Store::open_in_memory().unwrap();
        let (untouched_result, untouched) = interrupted(&store, "untouched.mp3", &snapshot("T"));
        let (written_result, written) = interrupted(&store, "written.mp3", &snapshot("T"));
        let (partial_result, partial) = interrupted(&store, "partial.mp3", &snapshot("T"));
        let (_, unreadable) = interrupted(&store, "unreadable.mp3", &snapshot("T"));
        let (_, missing) = interrupted(&store, "missing.mp3", &snapshot("T"));
        // Same values, different raw bytes: not unchanged, and not the write.
        let mut raw_differs = snapshot("T");
        raw_differs.raw_id3 = Some(RawId3 { v2: Some(BASE64.encode("ID3 other")), v1: None });
        let (_, raw) = interrupted(&store, "raw.mp3", &snapshot("T"));
        // The write landed, and someone also retitled the file: unknown.
        let mut retitled = written_over(&snapshot("T"));
        retitled.values.insert(Field::Title, vec![FrameValues { frame: "TIT2".into(), values: vec!["Other".into()] }]);

        let outcomes = store
            .reconcile_open_applications(|app| match app.path.as_str() {
                "untouched.mp3" => read(snapshot("T")),
                "written.mp3" => read(written_over(&snapshot("T"))),
                "partial.mp3" => read(retitled.clone()),
                "raw.mp3" => read(raw_differs.clone()),
                "unreadable.mp3" => FileNow::Unreadable("bad header".into()),
                _ => FileNow::Missing,
            })
            .unwrap();

        assert_eq!(outcomes[0], Reconciliation::Abandoned { application: untouched });
        assert_eq!(outcomes[1], Reconciliation::Completed { application: written });
        assert!(matches!(outcomes[2], Reconciliation::Unknown { application, .. } if application == partial));
        for (outcome, id) in outcomes[3..5].iter().zip([unreadable, missing]) {
            assert!(matches!(outcome, Reconciliation::NeedsUser { application, .. } if application.id == id), "{outcome:?}");
        }
        assert!(matches!(outcomes[5], Reconciliation::Unknown { application, .. } if application == raw));

        // Unchanged: abandoned, the result free again.
        let abandoned = store.application(untouched).unwrap().unwrap();
        assert!(matches!(abandoned.outcome, Some(Outcome::Abandoned { ref reason, .. }) if reason == RECONCILED_UNCHANGED));
        store.set_state(untouched_result, ReviewState::Pending).unwrap();
        // Landed: recorded as written with its after-state, undoable.
        assert_eq!(store.result(written_result).unwrap().unwrap().0.state, ReviewState::Applied);
        assert_eq!(store.application_after_snapshot(written).unwrap(), Some(written_over(&snapshot("T"))));
        assert_eq!(store.last_application(written_result).unwrap().map(|a| a.id), Some(written));
        // Unknown: closed as such, its result back in review, the track free.
        let closed = store.application(partial).unwrap().unwrap();
        assert!(matches!(closed.outcome, Some(Outcome::Abandoned { ref reason, .. }) if reason.starts_with(RECONCILED_UNKNOWN)));
        assert_eq!(store.result(partial_result).unwrap().unwrap().0.state, ReviewState::Pending);
        assert_eq!(store.application_snapshot(partial).unwrap(), Some(snapshot("T")), "the before-tags are kept");
        // The unreadable and the missing file stay open, for the next start,
        // which settles them once it can read them.
        let open: Vec<ApplicationId> = store.open_applications().unwrap().iter().map(|a| a.id).collect();
        assert_eq!(open, [unreadable, missing]);
        let again = store.reconcile_open_applications(|_| read(snapshot("T"))).unwrap();
        assert_eq!(
            again,
            [Reconciliation::Abandoned { application: unreadable }, Reconciliation::Abandoned { application: missing }]
        );
        assert_eq!(store.open_applications().unwrap(), []);
    }

    #[test]
    fn an_auto_apply_closed_as_unknown_goes_back_to_review_as_a_review() {
        let store = Store::open_in_memory().unwrap();
        let track = store.upsert_track(&key("a.mp3"), 360.0, None, &tags("T")).unwrap();
        let auto = store.save_result(track, Decision::AutoApply, &[], Some(&proposal()), None, &[], false).unwrap();
        store.begin_application(auto, &proposal().changes, None, &snapshot("T"), None).unwrap();
        let outcomes = store.reconcile_open_applications(|_| read(snapshot("Someone else's"))).unwrap();
        assert!(matches!(outcomes[..], [Reconciliation::Unknown { .. }]), "{outcomes:?}");
        let (item, _, _, warnings) = store.result(auto).unwrap().unwrap();
        assert_eq!((item.state, item.decision), (ReviewState::Pending, Decision::Review));
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn reconciling_from_files_leaves_a_missing_file_open() {
        let store = Store::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.mp3");
        let (_, app) = interrupted(&store, missing.to_str().unwrap(), &snapshot("T"));
        let outcomes = store.reconcile_open_applications_from_files().unwrap();
        assert!(matches!(&outcomes[..], [Reconciliation::NeedsUser { application, .. }] if application.id == app));
        assert_eq!(store.open_applications().unwrap().len(), 1);
    }

    #[test]
    fn a_locked_file_stays_open_and_unparseable_tags_close_as_unknown() {
        let store = Store::open_in_memory().unwrap();
        let (_, locked) = interrupted(&store, "locked.mp3", &snapshot("T"));
        let (garbled_result, garbled) = interrupted(&store, "garbled.mp3", &snapshot("T"));
        let outcomes = store
            .reconcile_open_applications(|app| match app.path.as_str() {
                "locked.mp3" => FileNow::Locked("sharing violation".into()),
                _ => FileNow::Unparseable("no frames".into()),
            })
            .unwrap();
        assert!(matches!(&outcomes[0], Reconciliation::NeedsUser { application, .. } if application.id == locked));
        assert!(matches!(&outcomes[1], Reconciliation::Unknown { application, .. } if *application == garbled));
        assert_eq!(store.open_applications().unwrap().iter().map(|a| a.id).collect::<Vec<_>>(), [locked]);
        let (item, _, _, warnings) = store.result(garbled_result).unwrap().unwrap();
        assert_eq!(item.state, ReviewState::Pending);
        assert_eq!(warnings, [RECONCILED_UNPARSEABLE_WARNING]);
        assert_eq!(store.application_snapshot(garbled).unwrap(), Some(snapshot("T")), "the before-tags are kept");
    }

    #[test]
    fn reconciling_from_files_closes_a_file_whose_tags_do_not_parse() {
        let store = Store::open_in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let garbled = dir.path().join("garbled.flac");
        std::fs::write(&garbled, b"fLaC but nothing a reader can parse").unwrap();
        let (result, app) = interrupted(&store, garbled.to_str().unwrap(), &snapshot("T"));
        let outcomes = store.reconcile_open_applications_from_files().unwrap();
        assert!(matches!(&outcomes[..], [Reconciliation::Unknown { application, .. }] if *application == app), "{outcomes:?}");
        assert_eq!(store.open_applications().unwrap(), []);
        assert_eq!(store.result(result).unwrap().unwrap().0.state, ReviewState::Pending);
        assert_eq!(std::fs::read(&garbled).unwrap(), b"fLaC but nothing a reader can parse", "never written");
    }

    #[test]
    fn a_key_for_a_file_is_canonical_so_spellings_meet() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let file = dir.path().join("Track.mp3");
        std::fs::write(&file, b"12345").unwrap();
        let direct = TrackKey::for_file(&file).unwrap();
        assert_eq!(direct.file_size, 5);
        assert!(direct.modified_unix > 0);
        assert!(!direct.path.starts_with(r"\\?\"), "{}", direct.path);
        let roundabout = TrackKey::for_file(&dir.path().join("sub").join("..").join("Track.mp3")).unwrap();
        assert_eq!(roundabout, direct);
        let store = Store::open_in_memory().unwrap();
        let id = store.upsert_track(&direct, 1.0, None, &tags("T")).unwrap();
        assert_eq!(store.upsert_track(&roundabout, 1.0, None, &tags("T")).unwrap(), id, "one file, one track");
        assert!(TrackKey::for_file(&dir.path().join("missing.mp3")).is_err());
    }

    #[test]
    fn errors_never_quote_bound_values() {
        let store = Store::open_in_memory().unwrap();
        let secret_path = "C:\\Music\\SECRET-PATH.mp3";
        let (_, result) = reviewed(&store, secret_path, 0.8);
        let errors = [
            store.set_state(result, ReviewState::Undone).unwrap_err(),
            record_applied(&store, result, &[], None, &snapshot("T"), &key(secret_path), Some("SECRET-LIB")).unwrap_err(),
        ];
        for err in errors {
            let text = err.to_string();
            assert!(!text.contains("SECRET"), "{text}");
        }
        let bad = from_json::<ExistingTags>(r#"{"artist": 5, "title": "SECRET-TAG"}"#, "existing tags").unwrap_err();
        assert!(!bad.to_string().contains("SECRET"), "{bad}");
    }

    #[test]
    fn a_version_3_store_gains_waiting_writes_and_keeps_its_results() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identify.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(SCHEMA_V2).unwrap();
            conn.execute_batch(SCHEMA_V3).unwrap();
            conn.pragma_update(None, "application_id", APPLICATION_ID).unwrap();
            conn.pragma_update(None, "user_version", 3).unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), 4);
        assert!(store.has_table("waiting_writes").unwrap() && store.has_table("waiting_write_ends").unwrap());
        let (_, result) = reviewed(&store, "a.mp3", 0.8);
        assert_eq!(store.waiting_writes().unwrap(), []);
        assert_eq!(store.result(result).unwrap().unwrap().0.waiting, None);
    }

    #[test]
    fn a_deferred_write_waits_accepted_and_shows_in_the_queue() {
        let store = Store::open_in_memory().unwrap();
        let (_, first) = reviewed(&store, "a.mp3", 0.8);
        let (_, second) = reviewed(&store, "b.mp3", 0.9);
        reviewed(&store, "c.mp3", 0.95);
        let id = store.defer_write(first, &[Field::Label], "open elsewhere").unwrap();
        let (item, ..) = store.result(first).unwrap().unwrap();
        assert_eq!(item.state, ReviewState::Accepted, "a pending result is accepted");
        let waiting = item.waiting.unwrap();
        assert_eq!((waiting.id, waiting.fields.as_slice(), waiting.reason.as_str()), (id, &[Field::Label][..], "open elsewhere"));
        // The same fields again keep the waiting write; others supersede it.
        assert_eq!(store.defer_write(first, &[Field::Label], "again").unwrap(), id);
        store.set_state(second, ReviewState::Accepted).unwrap();
        let later = store.defer_write(second, &[Field::Label, Field::Artwork], "open").unwrap();
        let replaced = store.defer_write(second, &[Field::Artwork], "open").unwrap();
        assert_ne!(later, replaced);
        assert_eq!(store.waiting_writes().unwrap().iter().map(|w| w.id).collect::<Vec<_>>(), [id, replaced]);
        // Newest first under its own filter; the others are not listed there.
        let listed: Vec<ResultId> = store.list(Filter::Waiting, 0, 10).unwrap().iter().map(|i| i.result).collect();
        assert_eq!(listed.len(), 2);
        assert!(listed.contains(&first) && listed.contains(&second));
        assert_eq!(store.count(Filter::Waiting).unwrap(), 2);
        assert_eq!(store.count(Filter::All).unwrap(), 3, "no row twice");
        assert_eq!(store.count(Filter::NeedsReview).unwrap(), 1);
        // Waiting and its end are permanent records.
        for statement in ["DELETE FROM waiting_writes", "UPDATE waiting_writes SET reason = ''", "DELETE FROM waiting_write_ends"] {
            assert!(store.conn.execute(statement, []).is_err(), "{statement}");
        }
    }

    #[test]
    fn the_waiting_filter_lists_newest_first() {
        let store = Store::open_in_memory().unwrap();
        let (_, old) = reviewed(&store, "a.mp3", 0.95);
        let (_, new) = reviewed(&store, "b.mp3", 0.6);
        store.defer_write(old, &[Field::Label], "x").unwrap();
        store.conn.execute("DROP TRIGGER waiting_writes_never_update", []).unwrap();
        store.conn.execute("UPDATE waiting_writes SET since_unix = 1", []).unwrap();
        store.defer_write(new, &[Field::Label], "x").unwrap();
        let listed: Vec<ResultId> = store.list(Filter::Waiting, 0, 10).unwrap().iter().map(|i| i.result).collect();
        assert_eq!(listed, [new, old]);
    }

    #[test]
    fn a_waiting_result_refuses_review_changes_and_reidentifying_until_cancelled() {
        let store = Store::open_in_memory().unwrap();
        let (track, result) = reviewed(&store, "a.mp3", 0.8);
        store.defer_write(result, &[Field::Label], "open").unwrap();
        for to in [ReviewState::Pending, ReviewState::Rejected] {
            let error = store.set_state(result, to).unwrap_err().to_string();
            assert!(error.contains("Cancel the waiting write"), "{error}");
        }
        assert!(store.edit_proposal(result, &[(Field::Label, "X".into())]).is_err());
        assert!(store.save_result(track, Decision::Review, &[], None, None, &[], true).is_err());
        store.cancel_waiting(result).unwrap();
        let (item, _, _, warnings) = store.result(result).unwrap().unwrap();
        assert_eq!((item.state, item.waiting, warnings.len()), (ReviewState::Pending, None, 0));
        assert!(store.cancel_waiting(result).is_err(), "nothing waits now");
        store.set_state(result, ReviewState::Rejected).unwrap();
    }

    #[test]
    fn a_dropped_waiting_write_returns_its_result_to_review_with_the_reason() {
        let store = Store::open_in_memory().unwrap();
        let track = store.upsert_track(&key("a.mp3"), 360.0, None, &tags("T")).unwrap();
        let auto = store.save_result(track, Decision::AutoApply, &[], Some(&proposal()), None, &[], false).unwrap();
        store.defer_write(auto, &[Field::Label], "open").unwrap();
        assert!(store.drop_waiting(auto, "the label changed").unwrap());
        let (item, _, _, warnings) = store.result(auto).unwrap().unwrap();
        assert_eq!((item.state, item.decision, item.waiting), (ReviewState::Pending, Decision::Review, None));
        assert_eq!(warnings, ["the label changed"]);
        assert!(store.drop_waiting(auto, "again").is_err());
    }

    #[test]
    fn a_write_that_lands_ends_its_waiting_write() {
        let store = Store::open_in_memory().unwrap();
        let track = store.upsert_track(&key("a.mp3"), 360.0, None, &tags("T")).unwrap();
        let auto = store.save_result(track, Decision::AutoApply, &[], Some(&proposal()), None, &[], false).unwrap();
        store.defer_write(auto, &[Field::Label], "open").unwrap();
        record_applied(&store, auto, &proposal().changes[..1], None, &snapshot("T"), &key("a.mp3"), None).unwrap();
        let (item, ..) = store.result(auto).unwrap().unwrap();
        assert_eq!((item.state, item.waiting), (ReviewState::AutoApplied, None), "still the auto-apply's write");
        assert_eq!(store.waiting_writes().unwrap(), []);
        let ended: String = store.conn.query_row("SELECT outcome FROM waiting_write_ends", [], |r| r.get(0)).unwrap();
        assert_eq!(ended, "written");
    }
}
