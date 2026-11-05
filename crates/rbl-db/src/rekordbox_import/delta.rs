//! Importing from rekordbox again, into a library rbxport already keeps.
//!
//! Each rekordbox track is matched to an rbxport track by the mapping an
//! earlier import recorded, else by its audio file's path, else it is new:
//! imported in full under a fresh rbxport ID, since rbxport may have given
//! rekordbox's ID to a track of its own meanwhile.
//!
//! A track that is already in rbxport takes rekordbox's tag fields only when
//! rbxport's still equal the baseline the last import recorded, so a track
//! edited in rbxport (by hand, by Identify, over LINK, any way at all) keeps
//! its tags, unless you chose to take rekordbox's for it from the report
//! (a [`Take`]). Everything else is filled, never replaced: memory cues, hot
//! cues, the analysis files (beat grid and waveforms), the key and the
//! artwork are taken only where rbxport has none of that kind. Playlists and
//! history sessions rekordbox has made since are added; tracks added to a
//! rekordbox playlist are appended to rbxport's unless that playlist was
//! changed in rbxport. Nothing is ever removed from rbxport.
//!
//! The files a track takes are copied before the database is written, each
//! to a path where nothing is yet; every row is written in one transaction,
//! and if that fails the copies are removed again.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rbl_core::ids::{Rng, MAX_CONTENT_ID, MAX_CUE_ID, MAX_PLAYLIST_ID};
use rusqlite::types::Value;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

use super::{copy_file, playlist_members, tag_fields, Progress, Stage};
use crate::write::{Writer, ROOT};
use crate::{Library, Result};

/// A track, as the report names it: its rbxport ID and "Artist — Title".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackLine {
    pub id: String,
    pub name: String,
}

/// One tag field where rbxport and rekordbox disagree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldDiff {
    /// As [`tag_fields`] names it: `title`, `album_artist`, `my_tags`, …
    pub field: String,
    pub rbxport: String,
    pub rekordbox: String,
}

/// A track whose tags were not taken.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Skipped {
    pub track: TrackLine,
    /// Where rekordbox's tags differ from rbxport's; shown, not applied.
    pub fields: Vec<FieldDiff>,
    /// Linked by its file's path, with no baseline to tell whether rbxport
    /// changed it, so rbxport's tags are kept.
    pub linked_by_path: bool,
}

/// Tracks a report kept as edited whose rekordbox tags you chose to take:
/// rbxport ID → the differences the report showed. A track's tags are taken
/// only while the differences are still those, so what you saw is what you
/// get.
pub type Take = HashMap<String, Vec<FieldDiff>>;

/// What an existing track was given because rbxport had none of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FillKind {
    MemoryCues,
    HotCues,
    /// The analysis files: beat grid, waveforms, and the tempo with them.
    Analysis,
    Key,
    Artwork,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Filled {
    pub track: TrackLine,
    pub what: Vec<FillKind>,
}

/// A playlist, and how many tracks the line is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistLine {
    pub name: String,
    pub tracks: u64,
}

/// What a repeat import did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeltaReport {
    /// Imported in full, under fresh rbxport IDs.
    pub new_tracks: Vec<TrackLine>,
    /// The new tracks that came with no analysis files, to analyse here.
    pub to_analyse: Vec<TrackLine>,
    /// Never edited in rbxport, and given rekordbox's current tags.
    pub updated: Vec<TrackLine>,
    /// Edited in rbxport, so no tag data was taken.
    pub skipped: Vec<Skipped>,
    /// Matched by their file's path rather than an earlier import's record.
    pub path_matches: Vec<TrackLine>,
    pub filled: Vec<Filled>,
    /// Already in rbxport with the same tags as rekordbox's, and nothing to fill.
    pub unchanged: u64,
    /// Imported before and deleted in rbxport since: left deleted.
    pub kept_deleted: u64,
    /// Whose file another rekordbox track already brought, so it was left out.
    pub duplicates: Vec<String>,
    pub playlists_created: Vec<PlaylistLine>,
    /// Tracks added in rekordbox, appended to rbxport's playlist.
    pub playlists_appended: Vec<PlaylistLine>,
    /// Changed in rbxport since the last import, so nothing was appended;
    /// `tracks` is how many rekordbox has that rbxport's doesn't.
    pub playlists_skipped: Vec<PlaylistLine>,
    pub history_sessions: u64,
    /// Plays in those sessions, added to the play count of tracks rbxport
    /// already had.
    pub plays_added: u64,
    /// Cues and loops copied, onto new tracks and into empty ones.
    pub cues: u64,
    pub files_copied: u64,
    pub bytes_copied: u64,
    pub problems: Vec<String>,
}

/// What `import.db` has to record once the library is written.
#[derive(Debug, Default)]
pub(super) struct Bookkeeping {
    /// `(rb_id, rb_uuid, rbx_id)`.
    pub tracks: Vec<(String, Option<String>, String)>,
    /// rbxport tracks whose baseline becomes their tags as they are now.
    pub baselines: Vec<String>,
    pub playlists: Vec<(String, Option<String>, String)>,
    /// rbxport playlists whose baseline becomes their members as they are now.
    pub playlist_baselines: Vec<String>,
    pub histories: Vec<(String, Option<String>, String)>,
}

/// What `import.db` holds from earlier imports.
#[derive(Debug, Default)]
pub(super) struct Recorded {
    /// `rb_id` → `(rb_uuid, rbx_id)`.
    pub tracks: HashMap<String, (Option<String>, String)>,
    /// `rbx_id` → tag fields JSON.
    pub baselines: HashMap<String, String>,
    pub playlists: HashMap<String, (Option<String>, String)>,
    /// `rbx_id` → members JSON.
    pub playlist_baselines: HashMap<String, String>,
    pub histories: HashMap<String, (Option<String>, String)>,
}

impl Recorded {
    pub(super) fn read(db: &Connection) -> Result<Self> {
        let map = |table: &str| -> Result<HashMap<String, (Option<String>, String)>> {
            let mut statement = db.prepare(&format!("SELECT rb_id, rb_uuid, rbx_id FROM {table}"))?;
            let rows = statement.query_map([], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?))))?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        };
        let values = |sql: &str| -> Result<HashMap<String, String>> {
            let mut statement = db.prepare(sql)?;
            let rows = statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        };
        Ok(Self {
            tracks: map("track_map")?,
            baselines: values("SELECT rbx_id, fields FROM track_baseline")?,
            playlists: map("playlist_map")?,
            playlist_baselines: values("SELECT rbx_id, members FROM playlist_baseline")?,
            histories: map("history_map")?,
        })
    }
}

/// A row by column name.
type Row = HashMap<String, Value>;

fn text(row: &Row, column: &str) -> String {
    match row.get(column) {
        Some(Value::Text(t)) => t.clone(),
        Some(Value::Integer(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn rows(conn: &Connection, sql: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<Row>> {
    let mut statement = conn.prepare(sql)?;
    let names: Vec<String> = statement.column_names().into_iter().map(str::to_owned).collect();
    let found = statement.query_map(args, |r| {
        let mut row = Row::with_capacity(names.len());
        for (index, name) in names.iter().enumerate() {
            row.insert(name.clone(), r.get::<_, Value>(index)?);
        }
        Ok(row)
    })?;
    Ok(found.collect::<std::result::Result<_, _>>()?)
}

/// The columns a table has in rbxport's library, so a row from rekordbox's
/// is copied only into columns that are there.
fn columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut statement = conn.prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))?;
    let names = statement.query_map([], |r| r.get::<_, String>(0))?;
    Ok(names.collect::<std::result::Result<_, _>>()?)
}

fn insert(conn: &Connection, table: &str, columns: &[String], row: &Row) -> Result<()> {
    let present: Vec<&String> = columns.iter().filter(|c| row.contains_key(c.as_str())).collect();
    let names: Vec<String> = present.iter().map(|c| format!("\"{c}\"")).collect();
    let marks: Vec<String> = (1..=present.len()).map(|i| format!("?{i}")).collect();
    let sql = format!("INSERT INTO {table} ({}) VALUES ({})", names.join(", "), marks.join(", "));
    let values: Vec<&Value> = present.iter().filter_map(|c| row.get(c.as_str())).collect();
    conn.execute(&sql, rusqlite::params_from_iter(values))?;
    Ok(())
}

/// The path as matching compares it: `\` and `/` the same, and on Windows
/// without regard to case.
fn path_key(path: &str) -> String {
    let unified = path.replace('\\', "/");
    if cfg!(windows) {
        unified.to_lowercase()
    } else {
        unified
    }
}

/// A JSON tag value as the report shows it.
fn shown(value: Option<&serde_json::Value>) -> String {
    match value {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(items)) => items.iter().map(|v| shown(Some(v))).collect::<Vec<_>>().join(", "),
        Some(other) => other.to_string(),
    }
}

/// The fields of [`tag_fields`], in its order.
const TAG_FIELDS: [&str; 19] = [
    "title", "artist", "album", "album_artist", "remixer", "original_artist", "composer", "lyricist", "genre", "label",
    "release_year", "release_date", "track_no", "disc_no", "comment", "rating", "color", "isrc", "my_tags",
];

/// The tag fields where two [`tag_fields`] objects differ, in the order the
/// fields are listed.
fn differences(rbxport: &str, rekordbox: &str) -> Vec<FieldDiff> {
    let parse = |json: &str| serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(json).unwrap_or_default();
    let (ours, theirs) = (parse(rbxport), parse(rekordbox));
    let null = serde_json::Value::Null;
    // In the order the fields are listed, whatever order the map keeps.
    let mut fields: Vec<&String> = ours.keys().chain(theirs.keys().filter(|k| !ours.contains_key(*k))).collect();
    fields.sort_by_key(|field| (TAG_FIELDS.iter().position(|f| f == field).unwrap_or(usize::MAX), (*field).clone()));
    fields
        .into_iter()
        .filter(|field| ours.get(*field).unwrap_or(&null) != theirs.get(*field).unwrap_or(&null))
        .map(|field| FieldDiff {
            field: field.clone(),
            rbxport: shown(ours.get(field)),
            rekordbox: shown(theirs.get(field)),
        })
        .collect()
}

/// Writes inside the import's transaction: ids, the update counter, the
/// lookup rows made so far.
struct Session<'a> {
    conn: &'a Connection,
    rng: &'a mut Rng,
    /// The last `rb_local_usn` handed out.
    usn: i64,
    stamp: String,
    taken: HashSet<String>,
    lookups: HashMap<(&'static str, String), Option<String>>,
    albums: HashMap<(String, Option<String>), Option<String>>,
    my_tags: HashMap<String, String>,
}

impl Session<'_> {
    fn next_usn(&mut self) -> i64 {
        self.usn += 1;
        self.usn
    }

    /// An ID no row of `table` has, nor any this import has handed out.
    fn fresh_id(&mut self, table: &str, limit: u64) -> Result<String> {
        let sql = format!("SELECT COUNT(*) FROM {table} WHERE ID = ?1");
        for _ in 0..64 {
            let candidate = self.rng.numeric_id(limit);
            let used: i64 = self.conn.query_row(&sql, [&candidate], |r| r.get(0))?;
            if used == 0 && self.taken.insert(format!("{table}:{candidate}")) {
                return Ok(candidate);
            }
        }
        Err(crate::DbError::WriteRefused(format!("could not find an unused id for {table}")))
    }

    /// The standard columns of a row made here.
    fn stamp_row(&mut self, row: &mut Row) {
        let usn = self.next_usn();
        for (column, value) in [
            ("rb_data_status", Value::Integer(0)),
            ("rb_local_data_status", Value::Integer(0)),
            ("rb_local_deleted", Value::Integer(0)),
            ("rb_local_synced", Value::Integer(0)),
            ("usn", Value::Null),
            ("rb_local_usn", Value::Integer(usn)),
            ("updated_at", Value::Text(self.stamp.clone())),
        ] {
            row.insert(column.to_owned(), value);
        }
        row.entry("created_at".to_owned()).or_insert_with(|| Value::Text(self.stamp.clone()));
        if matches!(row.get("created_at"), Some(Value::Null)) {
            row.insert("created_at".to_owned(), Value::Text(self.stamp.clone()));
        }
    }

    /// The ID of rbxport's lookup row named `name` (`djmdArtist`, `djmdGenre`,
    /// `djmdLabel`), made when there is none. No name is no row.
    fn lookup(&mut self, table: &'static str, name: &str) -> Result<Option<String>> {
        if name.is_empty() {
            return Ok(None);
        }
        let key = (table, name.to_owned());
        if let Some(found) = self.lookups.get(&key) {
            return Ok(found.clone());
        }
        let found: Option<String> = self
            .conn
            .query_row(
                &format!("SELECT ID FROM {table} WHERE Name = ?1 AND rb_local_deleted = 0 ORDER BY ID LIMIT 1"),
                [name],
                |r| r.get(0),
            )
            .optional()?;
        let id = if let Some(id) = found {
            id
        } else {
            let id = self.fresh_id(table, MAX_PLAYLIST_ID)?;
            let mut row = Row::new();
            row.insert("ID".into(), Value::Text(id.clone()));
            row.insert("Name".into(), Value::Text(name.to_owned()));
            row.insert("UUID".into(), Value::Text(self.rng.uuid4()));
            self.stamp_row(&mut row);
            insert(self.conn, table, &columns(self.conn, table)?, &row)?;
            id
        };
        self.lookups.insert(key, Some(id.clone()));
        Ok(Some(id))
    }

    /// rbxport's album named `name` with that album artist, made when there
    /// is none: two albums can share a name.
    fn album(&mut self, name: &str, artist: Option<String>) -> Result<Option<String>> {
        if name.is_empty() {
            return Ok(None);
        }
        let key = (name.to_owned(), artist.clone());
        if let Some(found) = self.albums.get(&key) {
            return Ok(found.clone());
        }
        let found: Option<String> = self
            .conn
            .query_row(
                "SELECT ID FROM djmdAlbum WHERE Name = ?1 AND COALESCE(AlbumArtistID, '') = COALESCE(?2, '')
                     AND rb_local_deleted = 0 ORDER BY ID LIMIT 1",
                params![name, artist],
                |r| r.get(0),
            )
            .optional()?;
        let id = if let Some(id) = found {
            id
        } else {
            let id = self.fresh_id("djmdAlbum", MAX_PLAYLIST_ID)?;
            let mut row = Row::new();
            row.insert("ID".into(), Value::Text(id.clone()));
            row.insert("Name".into(), Value::Text(name.to_owned()));
            row.insert("AlbumArtistID".into(), artist.map_or(Value::Null, Value::Text));
            row.insert("UUID".into(), Value::Text(self.rng.uuid4()));
            self.stamp_row(&mut row);
            insert(self.conn, "djmdAlbum", &columns(self.conn, "djmdAlbum")?, &row)?;
            id
        };
        self.albums.insert(key, Some(id.clone()));
        Ok(Some(id))
    }
}

/// rekordbox's library, as read from the copy.
struct Source {
    tracks: Vec<Row>,
    /// Lookup ID → name, by table.
    names: HashMap<&'static str, HashMap<String, String>>,
    /// Album ID → (name, album artist ID).
    albums: HashMap<String, (String, String)>,
    /// Key ID → name.
    keys: HashMap<String, String>,
    tags: HashMap<String, String>,
    /// My Tag ID → (name, its category's name).
    my_tags: HashMap<String, (String, String)>,
    /// Track ID → My Tag IDs.
    track_tags: HashMap<String, Vec<String>>,
    playlists: Vec<Row>,
    histories: Vec<Row>,
}

impl Source {
    fn read(conn: &Connection) -> Result<Self> {
        let pairs = |sql: &str| -> Result<HashMap<String, String>> {
            let mut statement = conn.prepare(sql)?;
            let found = statement.query_map([], |r| {
                Ok((r.get::<_, Option<String>>(0)?.unwrap_or_default(), r.get::<_, Option<String>>(1)?.unwrap_or_default()))
            })?;
            Ok(found.collect::<std::result::Result<_, _>>()?)
        };
        let mut names = HashMap::new();
        for table in ["djmdArtist", "djmdGenre", "djmdLabel"] {
            names.insert(table, pairs(&format!("SELECT CAST(ID AS TEXT), Name FROM {table}"))?);
        }
        let mut albums = HashMap::new();
        for row in rows(conn, "SELECT CAST(ID AS TEXT) AS ID, Name, CAST(AlbumArtistID AS TEXT) AS Artist FROM djmdAlbum", &[])? {
            albums.insert(text(&row, "ID"), (text(&row, "Name"), text(&row, "Artist")));
        }
        let categories = pairs("SELECT CAST(ID AS TEXT), Name FROM djmdMyTag WHERE Attribute = 1")?;
        let mut my_tags = HashMap::new();
        for row in rows(conn, "SELECT CAST(ID AS TEXT) AS ID, Name, CAST(ParentID AS TEXT) AS Parent FROM djmdMyTag WHERE Attribute = 0", &[])? {
            let category = categories.get(&text(&row, "Parent")).cloned().unwrap_or_default();
            my_tags.insert(text(&row, "ID"), (text(&row, "Name"), category));
        }
        let mut track_tags: HashMap<String, Vec<String>> = HashMap::new();
        for row in rows(
            conn,
            "SELECT CAST(ContentID AS TEXT) AS C, CAST(MyTagID AS TEXT) AS T FROM djmdSongMyTag WHERE rb_local_deleted = 0",
            &[],
        )? {
            track_tags.entry(text(&row, "C")).or_default().push(text(&row, "T"));
        }
        Ok(Self {
            tracks: rows(conn, "SELECT * FROM djmdContent WHERE rb_local_deleted = 0 ORDER BY ID", &[])?,
            names,
            albums,
            keys: pairs("SELECT CAST(ID AS TEXT), ScaleName FROM djmdKey")?,
            tags: tag_fields(conn)?.into_iter().collect(),
            my_tags,
            track_tags,
            playlists: rows(conn, "SELECT * FROM djmdPlaylist WHERE rb_local_deleted = 0", &[])?,
            histories: rows(conn, "SELECT * FROM djmdHistory WHERE rb_local_deleted = 0", &[])?,
        })
    }

    fn name(&self, table: &str, id: &str) -> String {
        self.names.get(table).and_then(|m| m.get(id)).cloned().unwrap_or_default()
    }

    fn cues(conn: &Connection, track: &str) -> Result<Vec<Row>> {
        rows(conn, "SELECT * FROM djmdCue WHERE ContentID = ?1 AND rb_local_deleted = 0 ORDER BY InMsec, ID", &[&track])
    }
}

fn label(row: &Row, source: &Source) -> String {
    let title = text(row, "Title");
    let artist = source.name("djmdArtist", &text(row, "ArtistID"));
    if artist.is_empty() { title } else { format!("{artist} — {title}") }
}

/// How a rekordbox track lands.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Match {
    /// Mapped by an earlier import, `rbx` live.
    Mapped(String),
    /// Linked by its file's path.
    Path(String),
    New,
    /// Mapped, and deleted in rbxport since.
    Deleted,
    /// Its file is the one another rekordbox track has already brought,
    /// as rbxport's `0`.
    Duplicate(String),
}

/// rbxport's track as matching and filling need it.
#[derive(Debug, Default, Clone)]
struct Ours {
    live: bool,
    uuid: String,
    analysis: String,
    image: String,
    key: String,
}

fn match_tracks(source: &Source, recorded: &Recorded, ours: &HashMap<String, Ours>, by_path: &HashMap<String, String>) -> Vec<Match> {
    let mut claimed: HashSet<String> = HashSet::new();
    let mut matches: Vec<Option<Match>> = vec![None; source.tracks.len()];
    // Mappings first, so a path is never matched to a track a mapping holds.
    for (index, row) in source.tracks.iter().enumerate() {
        let Some((uuid, rbx)) = recorded.tracks.get(&text(row, "ID")) else { continue };
        let theirs = text(row, "UUID");
        if uuid.as_deref().is_some_and(|u| !u.is_empty() && !theirs.is_empty() && *u != theirs) {
            // rekordbox gave that ID to another track since.
            continue;
        }
        claimed.insert(rbx.clone());
        matches[index] = Some(if ours.get(rbx).is_some_and(|t| t.live) { Match::Mapped(rbx.clone()) } else { Match::Deleted });
    }
    for (index, row) in source.tracks.iter().enumerate() {
        if matches[index].is_some() {
            continue;
        }
        let found = by_path.get(&path_key(&text(row, "FolderPath")));
        matches[index] = Some(match found {
            Some(rbx) if claimed.insert(rbx.clone()) => Match::Path(rbx.clone()),
            Some(rbx) => Match::Duplicate(rbx.clone()),
            None => Match::New,
        });
    }
    matches.into_iter().map(|m| m.unwrap_or(Match::New)).collect()
}

/// A set of files copied into rbxport's `share/`.
#[derive(Debug, Default)]
struct Copies {
    made: Vec<PathBuf>,
    files: u64,
    bytes: u64,
}

impl Copies {
    fn undo(&self) {
        for path in &self.made {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Copies the file at `relative` in rekordbox's `share/` into rbxport's,
/// with the files beside it that share its stem (an analysis's `.DAT`,
/// `.EXT`, `.2EX`, `.3EX`; artwork's three sizes). Lands at the same path
/// when nothing is there yet, else in a fresh folder; never over a file.
/// `None` when rekordbox has no such file.
fn copy_set(
    from_share: &Path,
    relative: &str,
    to_share: &Path,
    fresh_folder: &mut dyn FnMut() -> String,
    copies: &mut Copies,
) -> std::result::Result<Option<String>, String> {
    let source = rbl_anlz::resolve(from_share, relative);
    if relative.is_empty() || !source.is_file() {
        return Ok(None);
    }
    let (Some(folder), Some(stem), Some(file_name)) = (
        source.parent(),
        source.file_stem().map(|s| s.to_string_lossy().into_owned()),
        Path::new(relative).file_name().map(|s| s.to_string_lossy().into_owned()),
    ) else {
        return Ok(None);
    };
    let mut set: Vec<PathBuf> = std::fs::read_dir(folder)
        .map_err(|e| format!("{relative}: {e}"))?
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()) && e.file_name().to_string_lossy().starts_with(&stem))
        .map(|e| e.path())
        .collect();
    set.sort();
    let free = |rel: &str| {
        let at = rbl_anlz::resolve(to_share, rel);
        let dir = at.parent().map(Path::to_path_buf).unwrap_or_default();
        set.iter().all(|file| file.file_name().is_none_or(|name| !dir.join(name).exists()))
    };
    let mut target = relative.to_owned();
    let mut attempts = 0;
    while !free(&target) {
        attempts += 1;
        if attempts > 8 {
            return Err(format!("{relative}: no free place for it in rbxport's library"));
        }
        target = format!("{}/{file_name}", fresh_folder());
    }
    let dir = rbl_anlz::resolve(to_share, &target).parent().map(Path::to_path_buf).unwrap_or_default();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{target}: {e}"))?;
    for file in &set {
        let Some(name) = file.file_name() else { continue };
        let dest = dir.join(name);
        let written = copy_file(file, &dest).map_err(|e| format!("{}: {e}", dest.display()))?;
        copies.made.push(dest.clone());
        let (len, expected) = (
            written.metadata().map_err(|e| e.to_string())?.len(),
            std::fs::metadata(file).map_err(|e| e.to_string())?.len(),
        );
        if len != expected {
            return Err(format!("{}: copied {len} of {expected} bytes", dest.display()));
        }
        copies.files += 1;
        copies.bytes += len;
    }
    Ok(Some(target))
}

/// What one existing track takes besides its tags.
#[derive(Debug, Default, Clone)]
struct Fill {
    memory: bool,
    hot: bool,
    analysis: Option<String>,
    key: Option<String>,
    artwork: Option<String>,
}

impl Fill {
    fn kinds(&self) -> Vec<FillKind> {
        let mut kinds = Vec::new();
        if self.memory {
            kinds.push(FillKind::MemoryCues);
        }
        if self.hot {
            kinds.push(FillKind::HotCues);
        }
        if self.analysis.is_some() {
            kinds.push(FillKind::Analysis);
        }
        if self.key.is_some() {
            kinds.push(FillKind::Key);
        }
        if self.artwork.is_some() {
            kinds.push(FillKind::Artwork);
        }
        kinds
    }
}

/// The plan for one rekordbox track, made before anything is written.
#[derive(Debug, Clone)]
enum Plan {
    New { id: String, uuid: String, analysis: Option<String>, artwork: Option<String> },
    Existing { rbx: String, take_tags: bool, fill: Fill },
    Nothing,
}

/// Merges rekordbox's library, read from `source` (a verified copy), into
/// the library `writer` holds, in one transaction. rekordbox's `share/` is
/// read from `source_share`. Returns the report and what `import.db` has to
/// record.
pub(super) fn merge(
    writer: &mut Writer,
    source: &Library,
    source_share: &Path,
    recorded: &Recorded,
    take: &Take,
    progress: &mut dyn FnMut(Progress),
) -> Result<(DeltaReport, Bookkeeping)> {
    progress(Progress { stage: Stage::Comparing, done: 0, total: 1 });
    let theirs = Source::read(source.connection())?;
    let (library, rng) = writer.import_session()?;
    let share = library.location().share_root.clone();
    let conn = library.connection();
    let ours = read_ours(conn)?;
    let our_tags: HashMap<String, String> = tag_fields(conn)?.into_iter().collect();
    let by_path = paths(conn)?;
    let matches = match_tracks(&theirs, recorded, &ours, &by_path);

    let mut report = DeltaReport::default();
    let mut book = Bookkeeping::default();
    let mut copies = Copies::default();
    let mut plans = Vec::with_capacity(matches.len());
    let taken_uuids: HashSet<String> = ours.values().map(|t| t.uuid.clone()).filter(|u| !u.is_empty()).collect();
    let mut fresh_analysis = || {
        let uuid = rng.uuid4();
        format!("/PIONEER/USBANLZ/{}/{}", uuid.get(..3).unwrap_or("000"), uuid.get(3..).unwrap_or("0"))
    };
    let total = u64::try_from(matches.len()).unwrap_or(u64::MAX);
    for (index, (row, matched)) in theirs.tracks.iter().zip(&matches).enumerate() {
        let rb_id = text(row, "ID");
        let plan = match matched {
            Match::Deleted => {
                report.kept_deleted += 1;
                Plan::Nothing
            }
            Match::Duplicate(_) => {
                report.duplicates.push(format!("{}: {}", label(row, &theirs), text(row, "FolderPath")));
                Plan::Nothing
            }
            Match::New => {
                let analysis = copy_set(source_share, &text(row, "AnalysisDataPath"), &share, &mut fresh_analysis, &mut copies);
                let artwork = copy_set(source_share, &text(row, "ImagePath"), &share, &mut fresh_analysis_art(&mut fresh_analysis), &mut copies);
                let (analysis, artwork) = (keep_problem(analysis, &mut report), keep_problem(artwork, &mut report));
                Plan::New { id: String::new(), uuid: text(row, "UUID"), analysis, artwork }
            }
            Match::Mapped(rbx) | Match::Path(rbx) => {
                let mine = ours.get(rbx).cloned().unwrap_or_default();
                let linked = matches!(matched, Match::Path(_));
                let now = our_tags.get(rbx).map_or("", String::as_str);
                let theirs_now = theirs.tags.get(&rb_id).map_or("", String::as_str);
                let diffs = differences(now, theirs_now);
                let same_now = diffs.is_empty();
                let untouched = recorded.baselines.get(rbx).is_some_and(|b| differences(now, b).is_empty());
                let chosen = take.get(rbx).map(|shown| *shown == diffs);
                if chosen == Some(false) && !same_now {
                    report.problems.push(format!(
                        "{}: its tags changed after the report you chose from, so rekordbox's were not taken; it is listed again with the differences as they are now",
                        label(row, &theirs)
                    ));
                }
                let take_tags = (untouched || chosen == Some(true)) && !same_now;
                if same_now && (linked || !untouched) {
                    // Equal now: record that as the baseline.
                    book.baselines.push(rbx.clone());
                } else if take_tags {
                    book.baselines.push(rbx.clone());
                } else if !same_now {
                    report.skipped.push(Skipped {
                        track: TrackLine { id: rbx.clone(), name: label(row, &theirs) },
                        fields: diffs,
                        linked_by_path: linked || !recorded.baselines.contains_key(rbx),
                    });
                }
                let fill = plan_fill(conn, row, &mine, rbx, source, source_share, &share, &mut fresh_analysis, &mut copies, &mut report)?;
                Plan::Existing { rbx: rbx.clone(), take_tags, fill }
            }
        };
        plans.push(plan);
        let done = u64::try_from(index + 1).unwrap_or(u64::MAX);
        if done % 64 == 0 || done == total {
            progress(Progress { stage: Stage::CopyingFiles, done, total });
        }
    }
    report.files_copied = copies.files;
    report.bytes_copied = copies.bytes;

    progress(Progress { stage: Stage::Merging, done: 0, total: 1 });
    let parts = Parts { theirs: &theirs, source: source.connection(), recorded, matches: &matches };
    let written = write_all(library, rng, &parts, &mut plans, taken_uuids, &mut report, &mut book);
    if let Err(e) = written {
        copies.undo();
        return Err(e);
    }
    progress(Progress { stage: Stage::Merging, done: 1, total: 1 });
    Ok((report, book))
}

/// Artwork goes in a folder of its own, as rbxport files it.
fn fresh_analysis_art(fresh: &mut dyn FnMut() -> String) -> impl FnMut() -> String + '_ {
    move || fresh().replacen("/PIONEER/USBANLZ/", "/PIONEER/Artwork/", 1)
}

fn keep_problem(result: std::result::Result<Option<String>, String>, report: &mut DeltaReport) -> Option<String> {
    result.unwrap_or_else(|problem| {
        report.problems.push(problem);
        None
    })
}

/// What an existing track lacks that rekordbox has, with the files copied
/// for it.
#[allow(clippy::too_many_arguments, reason = "the parts of one merge, read once")]
fn plan_fill(
    conn: &Connection,
    row: &Row,
    mine: &Ours,
    rbx: &str,
    source: &Library,
    source_share: &Path,
    share: &Path,
    fresh: &mut dyn FnMut() -> String,
    copies: &mut Copies,
    report: &mut DeltaReport,
) -> Result<Fill> {
    let rb_id = text(row, "ID");
    let mut fill = Fill::default();
    let count = |kind: &str| -> Result<(i64, i64)> {
        let sql = format!("SELECT COUNT(*) FROM djmdCue WHERE ContentID = ?1 AND rb_local_deleted = 0 AND {kind}");
        let ours: i64 = conn.query_row(&sql, [rbx], |r| r.get(0))?;
        let theirs: i64 = source.connection().query_row(&sql, [&rb_id], |r| r.get(0))?;
        Ok((ours, theirs))
    };
    let (ours_memory, theirs_memory) = count("Kind = 0")?;
    fill.memory = ours_memory == 0 && theirs_memory > 0;
    let (ours_hot, theirs_hot) = count("Kind > 0")?;
    fill.hot = ours_hot == 0 && theirs_hot > 0;
    let has_analysis = !mine.analysis.is_empty() && rbl_anlz::resolve(share, &mine.analysis).is_file();
    if !has_analysis {
        let analysis_copy = copy_set(source_share, &text(row, "AnalysisDataPath"), share, fresh, copies);
        fill.analysis = keep_problem(analysis_copy, report);
    }
    if mine.key.is_empty() && !text(row, "KeyID").is_empty() {
        fill.key = Some(text(row, "KeyID"));
    }
    if mine.image.is_empty() {
        let artwork_copy = copy_set(source_share, &text(row, "ImagePath"), share, &mut fresh_analysis_art(fresh), copies);
        fill.artwork = keep_problem(artwork_copy, report);
    }
    Ok(fill)
}

fn read_ours(conn: &Connection) -> Result<HashMap<String, Ours>> {
    let mut statement = conn.prepare(
        "SELECT CAST(ID AS TEXT), rb_local_deleted, UUID, AnalysisDataPath, ImagePath, CAST(KeyID AS TEXT) FROM djmdContent",
    )?;
    let found = statement.query_map([], |r| {
        let s = |i: usize| -> rusqlite::Result<String> { Ok(r.get::<_, Option<String>>(i)?.unwrap_or_default()) };
        Ok((
            r.get::<_, String>(0)?,
            Ours {
                live: r.get::<_, Option<i64>>(1)?.unwrap_or(0) == 0,
                uuid: s(2)?,
                analysis: s(3)?,
                image: s(4)?,
                key: s(5)?,
            },
        ))
    })?;
    Ok(found.collect::<std::result::Result<_, _>>()?)
}

fn paths(conn: &Connection) -> Result<HashMap<String, String>> {
    let mut statement =
        conn.prepare("SELECT CAST(ID AS TEXT), FolderPath FROM djmdContent WHERE rb_local_deleted = 0 ORDER BY ID")?;
    let found = statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?.unwrap_or_default())))?;
    let mut by_path = HashMap::new();
    for pair in found {
        let (id, path) = pair?;
        if !path.is_empty() {
            by_path.entry(path_key(&path)).or_insert(id);
        }
    }
    Ok(by_path)
}

/// What every part of the write reads.
struct Parts<'a> {
    theirs: &'a Source,
    source: &'a Connection,
    recorded: &'a Recorded,
    matches: &'a [Match],
}

/// Every row, in one transaction.
fn write_all(
    library: &mut Library,
    rng: &mut Rng,
    parts: &Parts<'_>,
    plans: &mut [Plan],
    mut uuids: HashSet<String>,
    report: &mut DeltaReport,
    book: &mut Bookkeeping,
) -> Result<()> {
    let theirs = parts.theirs;
    let tx = library.connection_mut().transaction_with_behavior(TransactionBehavior::Immediate)?;
    let start = crate::write::next_usn(&tx)? - 1;
    let mut s = Session {
        conn: &tx,
        rng,
        usn: start,
        stamp: rbl_core::time::now(),
        taken: HashSet::new(),
        lookups: HashMap::new(),
        albums: HashMap::new(),
        my_tags: HashMap::new(),
    };
    let content_columns = columns(&tx, "djmdContent")?;
    let cue_columns = columns(&tx, "djmdCue")?;
    let property: (Option<String>, Option<String>) = tx
        .query_row("SELECT DBID, DeviceID FROM djmdProperty LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap_or((None, None));
    // rekordbox ID → rbxport ID, for playlists and history.
    let mut ids: HashMap<String, String> = HashMap::new();
    // rbxport tracks that were there before this import.
    let mut existing: HashSet<String> = HashSet::new();

    for ((row, matched), plan) in theirs.tracks.iter().zip(parts.matches).zip(plans.iter_mut()) {
        let rb_id = text(row, "ID");
        let rb_uuid = Some(text(row, "UUID")).filter(|u| !u.is_empty());
        match plan {
            Plan::Nothing => {
                if let Match::Duplicate(rbx) = matched {
                    ids.insert(rb_id, rbx.clone());
                }
            }
            Plan::New { id, uuid, analysis, artwork } => {
                *id = s.fresh_id("djmdContent", MAX_CONTENT_ID)?;
                // rekordbox's UUID stays, unless rbxport already has it.
                if uuid.is_empty() || !uuids.insert(uuid.clone()) {
                    *uuid = s.rng.uuid4();
                    uuids.insert(uuid.clone());
                }
                let files = (analysis.as_deref(), artwork.as_deref());
                new_track(&mut s, theirs, row, (id, uuid), files, &property, &content_columns)?;
                report.cues += copy_cues(&mut s, &Source::cues(parts.source, &rb_id)?, (id, uuid), |_| true, &cue_columns)?;
                set_my_tags(&mut s, theirs, &rb_id, id)?;
                report.new_tracks.push(TrackLine { id: id.clone(), name: label(row, theirs) });
                if analysis.is_none() {
                    report.to_analyse.push(TrackLine { id: id.clone(), name: text(row, "Title") });
                }
                book.tracks.push((rb_id.clone(), rb_uuid, id.clone()));
                book.baselines.push(id.clone());
                ids.insert(rb_id, id.clone());
            }
            Plan::Existing { rbx, take_tags, fill } => {
                if *take_tags {
                    take_tag_fields(&mut s, theirs, row, rbx)?;
                    set_my_tags(&mut s, theirs, &rb_id, rbx)?;
                    report.updated.push(TrackLine { id: rbx.clone(), name: label(row, theirs) });
                }
                report.cues += fill_track(&mut s, theirs, parts.source, row, rbx, fill, &cue_columns)?;
                let kinds = fill.kinds();
                let skipped = report.skipped.iter().any(|k| k.track.id == *rbx);
                if !kinds.is_empty() {
                    report.filled.push(Filled { track: TrackLine { id: rbx.clone(), name: label(row, theirs) }, what: kinds });
                } else if !*take_tags && !skipped {
                    report.unchanged += 1;
                }
                if let Match::Path(_) = matched {
                    report.path_matches.push(TrackLine { id: rbx.clone(), name: label(row, theirs) });
                    book.tracks.push((rb_id.clone(), rb_uuid, rbx.clone()));
                }
                existing.insert(rbx.clone());
                ids.insert(rb_id, rbx.clone());
            }
        }
    }

    playlists(&mut s, parts, &ids, report, book)?;
    let history = histories(&mut s, parts, &ids, &existing, book)?;
    (report.history_sessions, report.plays_added) = history;

    if s.usn > start {
        crate::write::set_counter(&tx, s.usn)?;
    }
    drop(s);
    tx.commit()?;
    Ok(())
}

/// A rekordbox track as a new rbxport row: every column rekordbox has that
/// rbxport's table has, with its lookups found again by name, its files
/// where they were copied, and this library's own device.
fn new_track(
    s: &mut Session<'_>,
    theirs: &Source,
    row: &Row,
    (id, uuid): (&str, &str),
    (analysis, artwork): (Option<&str>, Option<&str>),
    property: &(Option<String>, Option<String>),
    columns: &[String],
) -> Result<()> {
    let mut new = row.clone();
    new.insert("ID".into(), Value::Text(id.to_owned()));
    new.insert("MasterSongID".into(), Value::Text(id.to_owned()));
    new.insert("UUID".into(), Value::Text(uuid.to_owned()));
    new.insert("MasterDBID".into(), property.0.clone().map_or(Value::Null, Value::Text));
    new.insert("DeviceID".into(), property.1.clone().map_or(Value::Null, Value::Text));
    // A counter whose owner is [UNKNOWN]; a new row of rbxport's leaves it unset.
    new.insert("rb_file_id".into(), Value::Null);
    for (column, value) in tag_columns(s, theirs, row)? {
        new.insert(column.to_owned(), value);
    }
    let key = theirs.keys.get(&text(row, "KeyID")).and_then(|name| crate::write::key_id_for(s.conn, name).ok());
    new.insert("KeyID".into(), key.map_or(Value::Null, Value::Text));
    if let Some(path) = analysis {
        new.insert("AnalysisDataPath".into(), Value::Text(path.to_owned()));
    } else {
        // No files: the track is analysed again, like a file just added.
        new.insert("AnalysisDataPath".into(), Value::Text(String::new()));
        new.insert("Analysed".into(), Value::Null);
    }
    new.insert("ImagePath".into(), Value::Text(artwork.unwrap_or_default().to_owned()));
    s.stamp_row(&mut new);
    insert(s.conn, "djmdContent", columns, &new)
}

/// The tag columns of a rekordbox track, with each lookup found (or made)
/// in rbxport's library by name.
fn tag_columns(s: &mut Session<'_>, theirs: &Source, row: &Row) -> Result<Vec<(&'static str, Value)>> {
    let mut values = Vec::new();
    for column in [
        "Title", "Lyricist", "ReleaseYear", "ReleaseDate", "TrackNo", "DiscNo", "Commnt", "Rating", "ColorID", "ISRC",
    ] {
        values.push((column, row.get(column).cloned().unwrap_or(Value::Null)));
    }
    for (column, table) in [
        ("ArtistID", "djmdArtist"),
        ("RemixerID", "djmdArtist"),
        ("OrgArtistID", "djmdArtist"),
        ("ComposerID", "djmdArtist"),
        ("GenreID", "djmdGenre"),
        ("LabelID", "djmdLabel"),
    ] {
        let name = theirs.name(table, &text(row, column));
        values.push((column, s.lookup(table, &name)?.map_or(Value::Null, Value::Text)));
    }
    let (album, album_artist) = theirs.albums.get(&text(row, "AlbumID")).cloned().unwrap_or_default();
    let album_artist = s.lookup("djmdArtist", &theirs.name("djmdArtist", &album_artist))?;
    values.push(("AlbumID", s.album(&album, album_artist)?.map_or(Value::Null, Value::Text)));
    Ok(values)
}

fn take_tag_fields(s: &mut Session<'_>, theirs: &Source, row: &Row, rbx: &str) -> Result<()> {
    let values = tag_columns(s, theirs, row)?;
    let usn = s.next_usn();
    let sets: Vec<String> = values.iter().enumerate().map(|(i, (c, _))| format!("{c} = ?{}", i + 1)).collect();
    let n = values.len();
    let sql = format!(
        "UPDATE djmdContent SET {}, rb_local_usn = ?{}, updated_at = ?{} WHERE ID = ?{} AND rb_local_deleted = 0",
        sets.join(", "),
        n + 1,
        n + 2,
        n + 3
    );
    let mut args: Vec<Value> = values.into_iter().map(|(_, v)| v).collect();
    args.extend([Value::Integer(usn), Value::Text(s.stamp.clone()), Value::Text(rbx.to_owned())]);
    s.conn.execute(&sql, rusqlite::params_from_iter(args))?;
    Ok(())
}

/// Gives an rbxport track exactly rekordbox's My Tags, made by name (in a
/// category of the same name) where rbxport has none so called.
fn set_my_tags(s: &mut Session<'_>, theirs: &Source, rb_id: &str, rbx: &str) -> Result<()> {
    let mut wanted: Vec<String> = Vec::new();
    for tag in theirs.track_tags.get(rb_id).into_iter().flatten() {
        let Some((name, category)) = theirs.my_tags.get(tag) else { continue };
        let id = my_tag(s, name, category)?;
        if !wanted.contains(&id) {
            wanted.push(id);
        }
    }
    let current: Vec<String> = {
        let mut statement = s.conn.prepare(
            "SELECT CAST(MyTagID AS TEXT) FROM djmdSongMyTag WHERE ContentID = ?1 AND rb_local_deleted = 0",
        )?;
        let found = statement.query_map([rbx], |r| r.get::<_, String>(0))?;
        found.collect::<std::result::Result<_, _>>()?
    };
    for gone in current.iter().filter(|t| !wanted.contains(t)) {
        let usn = s.next_usn();
        s.conn.execute(
            "UPDATE djmdSongMyTag SET rb_local_deleted = 1, rb_local_usn = ?1, updated_at = ?2
             WHERE ContentID = ?3 AND MyTagID = ?4 AND rb_local_deleted = 0",
            params![usn, s.stamp, rbx, gone],
        )?;
    }
    for tag in wanted.iter().filter(|t| !current.contains(t)) {
        let track_no: i64 = s.conn.query_row(
            "SELECT COALESCE(MAX(TrackNo), 0) + 1 FROM djmdSongMyTag WHERE MyTagID = ?1 AND rb_local_deleted = 0",
            [tag],
            |r| r.get(0),
        )?;
        let mut row = Row::new();
        row.insert("ID".into(), Value::Text(s.rng.uuid4()));
        row.insert("MyTagID".into(), Value::Text(tag.clone()));
        row.insert("ContentID".into(), Value::Text(rbx.to_owned()));
        row.insert("TrackNo".into(), Value::Integer(track_no));
        row.insert("UUID".into(), Value::Text(s.rng.uuid4()));
        s.stamp_row(&mut row);
        insert(s.conn, "djmdSongMyTag", &columns(s.conn, "djmdSongMyTag")?, &row)?;
    }
    Ok(())
}

/// rbxport's My Tag called `name`, made under a category called `category`
/// (made too, when rbxport has none) when there is none.
fn my_tag(s: &mut Session<'_>, name: &str, category: &str) -> Result<String> {
    if let Some(id) = s.my_tags.get(name) {
        return Ok(id.clone());
    }
    let found: Option<String> = s
        .conn
        .query_row(
            "SELECT CAST(ID AS TEXT) FROM djmdMyTag WHERE Name = ?1 AND Attribute = 0 AND rb_local_deleted = 0 ORDER BY ID LIMIT 1",
            [name],
            |r| r.get(0),
        )
        .optional()?;
    let id = if let Some(id) = found {
        id
    } else {
        let parent = node_with(s, "djmdMyTag", category, ROOT, 1, Vec::new())?;
        node_with(s, "djmdMyTag", name, &parent, 0, Vec::new())?
    };
    s.my_tags.insert(name.to_owned(), id.clone());
    Ok(id)
}

/// A node of a tree table (`djmdMyTag`, `djmdHistory` folders) called
/// `name` under `parent`, made at the end of it when there is none.
fn node_with(
    s: &mut Session<'_>,
    table: &str,
    name: &str,
    parent: &str,
    attribute: i64,
    extra: Vec<(&'static str, Value)>,
) -> Result<String> {
    let found: Option<String> = s
        .conn
        .query_row(
            &format!(
                "SELECT CAST(ID AS TEXT) FROM {table} WHERE Name = ?1 AND ParentID = ?2 AND Attribute = ?3
                     AND rb_local_deleted = 0 ORDER BY Seq LIMIT 1"
            ),
            params![name, parent, attribute],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = found {
        return Ok(id);
    }
    let id = s.fresh_id(table, MAX_PLAYLIST_ID)?;
    let seq: i64 = s.conn.query_row(
        &format!("SELECT COALESCE(MAX(Seq), 0) + 1 FROM {table} WHERE ParentID = ?1 AND rb_local_deleted = 0"),
        [parent],
        |r| r.get(0),
    )?;
    let mut row = Row::new();
    row.insert("ID".into(), Value::Text(id.clone()));
    row.insert("Seq".into(), Value::Integer(seq));
    row.insert("Name".into(), Value::Text(name.to_owned()));
    row.insert("Attribute".into(), Value::Integer(attribute));
    row.insert("ParentID".into(), Value::Text(parent.to_owned()));
    row.insert("UUID".into(), Value::Text(s.rng.uuid4()));
    for (column, value) in extra {
        row.insert(column.to_owned(), value);
    }
    s.stamp_row(&mut row);
    insert(s.conn, table, &columns(s.conn, table)?, &row)?;
    Ok(id)
}

/// Copies rekordbox's cues for which `keep` holds onto an rbxport track.
fn copy_cues(
    s: &mut Session<'_>,
    cues: &[Row],
    (rbx, rbx_uuid): (&str, &str),
    keep: impl Fn(i64) -> bool,
    columns: &[String],
) -> Result<u64> {
    let mut copied = 0;
    for cue in cues {
        let kind = match cue.get("Kind") {
            Some(Value::Integer(k)) => *k,
            _ => 0,
        };
        if !keep(kind) {
            continue;
        }
        let mut row = cue.clone();
        row.insert("ID".into(), Value::Text(s.fresh_id("djmdCue", MAX_CUE_ID)?));
        row.insert("ContentID".into(), Value::Text(rbx.to_owned()));
        row.insert("ContentUUID".into(), Value::Text(rbx_uuid.to_owned()));
        row.insert("UUID".into(), Value::Text(s.rng.uuid4()));
        s.stamp_row(&mut row);
        insert(s.conn, "djmdCue", columns, &row)?;
        copied += 1;
    }
    Ok(copied)
}

/// Gives an existing rbxport track what it lacks of rekordbox's.
fn fill_track(
    s: &mut Session<'_>,
    theirs: &Source,
    source: &Connection,
    row: &Row,
    rbx: &str,
    fill: &Fill,
    cue_columns: &[String],
) -> Result<u64> {
    let mut cues_copied = 0;
    if fill.memory || fill.hot {
        let uuid: String = s
            .conn
            .query_row("SELECT COALESCE(UUID, '') FROM djmdContent WHERE ID = ?1", [rbx], |r| r.get(0))?;
        let (memory, hot) = (fill.memory, fill.hot);
        let cues = Source::cues(source, &text(row, "ID"))?;
        cues_copied = copy_cues(s, &cues, (rbx, &uuid), |kind| if kind == 0 { memory } else { hot }, cue_columns)?;
    }
    if let Some(path) = &fill.analysis {
        let usn = s.next_usn();
        s.conn.execute(
            "UPDATE djmdContent SET AnalysisDataPath = ?1, BPM = ?2, Analysed = ?3, rb_local_usn = ?4, updated_at = ?5
             WHERE ID = ?6",
            params![
                path,
                row.get("BPM").cloned().unwrap_or(Value::Null),
                row.get("Analysed").cloned().unwrap_or(Value::Null),
                usn,
                s.stamp,
                rbx
            ],
        )?;
    }
    if let Some(key) = &fill.key {
        if let Some(id) = theirs.keys.get(key).and_then(|name| crate::write::key_id_for(s.conn, name).ok()) {
            let usn = s.next_usn();
            s.conn.execute(
                "UPDATE djmdContent SET KeyID = ?1, rb_local_usn = ?2, updated_at = ?3 WHERE ID = ?4",
                params![id, usn, s.stamp, rbx],
            )?;
        }
    }
    if let Some(path) = &fill.artwork {
        let usn = s.next_usn();
        s.conn.execute(
            "UPDATE djmdContent SET ImagePath = ?1, rb_local_usn = ?2, updated_at = ?3 WHERE ID = ?4",
            params![path, usn, s.stamp, rbx],
        )?;
    }
    Ok(cues_copied)
}

fn integer(row: &Row, column: &str) -> i64 {
    match row.get(column) {
        Some(Value::Integer(n)) => *n,
        Some(Value::Text(t)) => t.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// A tree table's live rows, parents before children and siblings by
/// `Seq`. A row whose parent is not there comes last, as if at the top.
fn tree_order(nodes: &[Row]) -> Vec<&Row> {
    let mut children: HashMap<String, Vec<&Row>> = HashMap::new();
    for node in nodes {
        children.entry(text(node, "ParentID")).or_default().push(node);
    }
    for list in children.values_mut() {
        list.sort_by_key(|r| (integer(r, "Seq"), text(r, "ID")));
    }
    let mut order: Vec<&Row> = Vec::with_capacity(nodes.len());
    let mut seen: HashSet<String> = HashSet::new();
    let mut stack: Vec<&Row> = children.get(ROOT).map(|l| l.iter().rev().copied().collect()).unwrap_or_default();
    loop {
        while let Some(node) = stack.pop() {
            if !seen.insert(text(node, "ID")) {
                continue;
            }
            order.push(node);
            if let Some(list) = children.get(&text(node, "ID")) {
                stack.extend(list.iter().rev().copied());
            }
        }
        // Orphans, and whatever hangs below them.
        let Some(orphan) = nodes.iter().find(|n| !seen.contains(&text(n, "ID"))) else { break };
        stack.push(orphan);
    }
    order
}

/// The rbxport row an earlier import mapped a rekordbox row to, unless
/// rekordbox has given that ID to another row since.
fn mapped<'r>(map: &'r HashMap<String, (Option<String>, String)>, row: &Row) -> Option<&'r String> {
    let (uuid, rbx) = map.get(&text(row, "ID"))?;
    let theirs = text(row, "UUID");
    let same = uuid.as_deref().is_none_or(|u| u.is_empty() || theirs.is_empty() || u == theirs);
    same.then_some(rbx)
}

fn live(conn: &Connection, table: &str, id: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE ID = ?1 AND rb_local_deleted = 0"),
        [id],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// A playlist's or session's members in order, by rekordbox ID.
fn members(conn: &Connection, table: &str, parent_column: &str, parent: &str) -> Result<Vec<String>> {
    let mut statement = conn.prepare(&format!(
        "SELECT CAST(ContentID AS TEXT) FROM {table} WHERE {parent_column} = ?1 AND rb_local_deleted = 0 ORDER BY TrackNo, ID"
    ))?;
    let found = statement.query_map([parent], |r| r.get::<_, String>(0))?;
    Ok(found.collect::<std::result::Result<_, _>>()?)
}

/// Appends tracks to an rbxport playlist.
fn append(s: &mut Session<'_>, playlist: &str, tracks: &[String]) -> Result<()> {
    let columns = columns(s.conn, "djmdSongPlaylist")?;
    let mut track_no: i64 = s.conn.query_row(
        "SELECT COALESCE(MAX(TrackNo), 0) FROM djmdSongPlaylist WHERE PlaylistID = ?1 AND rb_local_deleted = 0",
        [playlist],
        |r| r.get(0),
    )?;
    for track in tracks {
        track_no += 1;
        let mut row = Row::new();
        row.insert("ID".into(), Value::Text(s.rng.uuid4()));
        row.insert("PlaylistID".into(), Value::Text(playlist.to_owned()));
        row.insert("ContentID".into(), Value::Text(track.clone()));
        row.insert("TrackNo".into(), Value::Integer(track_no));
        row.insert("UUID".into(), Value::Text(s.rng.uuid4()));
        s.stamp_row(&mut row);
        insert(s.conn, "djmdSongPlaylist", &columns, &row)?;
    }
    Ok(())
}

/// rekordbox's playlist tree into rbxport's: playlists and folders it made
/// since are made, and tracks added to a playlist rbxport hasn't changed
/// since are appended. A playlist deleted in rbxport stays deleted.
fn playlists(
    s: &mut Session<'_>,
    parts: &Parts<'_>,
    ids: &HashMap<String, String>,
    report: &mut DeltaReport,
    book: &mut Bookkeeping,
) -> Result<()> {
    // rekordbox ID → where its children go in rbxport.
    let mut placed: HashMap<String, String> = HashMap::new();
    for node in tree_order(&parts.theirs.playlists) {
        let rb = text(node, "ID");
        let attribute = integer(node, "Attribute");
        let parent = placed.get(&text(node, "ParentID")).cloned().unwrap_or_else(|| ROOT.to_owned());
        let wanted: Vec<String> = members(parts.source, "djmdSongPlaylist", "PlaylistID", &rb)?
            .iter()
            .filter_map(|track| ids.get(track).cloned())
            .collect();
        let name = text(node, "Name");
        match mapped(&parts.recorded.playlists, node) {
            Some(rbx) if live(s.conn, "djmdPlaylist", rbx)? => {
                placed.insert(rb, rbx.clone());
                if attribute != crate::write::ATTRIBUTE_PLAYLIST {
                    continue;
                }
                let now = members(s.conn, "djmdSongPlaylist", "PlaylistID", rbx)?;
                let mut adding: Vec<String> = Vec::new();
                for track in wanted {
                    if !now.contains(&track) && !adding.contains(&track) {
                        adding.push(track);
                    }
                }
                if adding.is_empty() {
                    continue;
                }
                let baseline: Option<Vec<String>> =
                    parts.recorded.playlist_baselines.get(rbx).and_then(|b| serde_json::from_str(b).ok());
                let tracks = u64::try_from(adding.len()).unwrap_or(u64::MAX);
                if baseline.as_ref() == Some(&now) {
                    append(s, rbx, &adding)?;
                    report.playlists_appended.push(PlaylistLine { name, tracks });
                    book.playlist_baselines.push(rbx.clone());
                } else {
                    report.playlists_skipped.push(PlaylistLine { name, tracks });
                }
            }
            Some(_) => {
                // Deleted in rbxport: left deleted, and anything new inside
                // it goes at the top.
            }
            None => {
                let id = s.fresh_id("djmdPlaylist", MAX_PLAYLIST_ID)?;
                let seq: i64 = s.conn.query_row(
                    "SELECT COALESCE(MAX(Seq), -1) + 1 FROM djmdPlaylist WHERE ParentID = ?1 AND rb_local_deleted = 0",
                    [&parent],
                    |r| r.get(0),
                )?;
                // An intelligent playlist's rule names the playlist's own ID.
                let smart = node.get("SmartList").and_then(|v| match v {
                    Value::Text(rule) if !rule.is_empty() => {
                        Some(Value::Text(rule.replace(&format!("Id=\"{rb}\""), &format!("Id=\"{id}\""))))
                    }
                    _ => None,
                });
                let mut row = Row::new();
                row.insert("ID".into(), Value::Text(id.clone()));
                row.insert("Seq".into(), Value::Integer(seq));
                row.insert("Name".into(), Value::Text(name.clone()));
                row.insert("Attribute".into(), Value::Integer(attribute));
                row.insert("ParentID".into(), Value::Text(parent));
                row.insert("SmartList".into(), smart.unwrap_or(Value::Null));
                row.insert("UUID".into(), Value::Text(s.rng.uuid4()));
                s.stamp_row(&mut row);
                insert(s.conn, "djmdPlaylist", &columns(s.conn, "djmdPlaylist")?, &row)?;
                let tracks = if attribute == crate::write::ATTRIBUTE_PLAYLIST {
                    let mut unique: Vec<String> = Vec::new();
                    for track in wanted {
                        if !unique.contains(&track) {
                            unique.push(track);
                        }
                    }
                    append(s, &id, &unique)?;
                    book.playlist_baselines.push(id.clone());
                    u64::try_from(unique.len()).unwrap_or(u64::MAX)
                } else {
                    0
                };
                report.playlists_created.push(PlaylistLine { name, tracks });
                book.playlists.push((rb.clone(), Some(text(node, "UUID")).filter(|u| !u.is_empty()), id.clone()));
                placed.insert(rb, id);
            }
        }
    }
    Ok(())
}

/// rekordbox's play history into rbxport's: sessions it recorded since are
/// added, filed in folders of the same names; sessions already imported are
/// kept as they are. Returns the sessions and the plays added; each play of
/// a track rbxport already had adds one to its play count, since rekordbox's
/// count for it was taken before the play.
fn histories(
    s: &mut Session<'_>,
    parts: &Parts<'_>,
    ids: &HashMap<String, String>,
    existing: &HashSet<String>,
    book: &mut Bookkeeping,
) -> Result<(u64, u64)> {
    let entry_columns = columns(s.conn, "djmdSongHistory")?;
    let mut placed: HashMap<String, String> = HashMap::new();
    let mut plays: HashMap<String, i64> = HashMap::new();
    let (mut sessions, mut added) = (0_u64, 0_u64);
    for node in tree_order(&parts.theirs.histories) {
        let rb = text(node, "ID");
        let parent = placed.get(&text(node, "ParentID")).cloned().unwrap_or_else(|| ROOT.to_owned());
        let uuid = Some(text(node, "UUID")).filter(|u| !u.is_empty());
        if let Some(rbx) = mapped(&parts.recorded.histories, node) {
            if live(s.conn, "djmdHistory", rbx)? {
                placed.insert(rb, rbx.clone());
            }
            continue;
        }
        let created = node.get("DateCreated").cloned().unwrap_or(Value::Null);
        if integer(node, "Attribute") != 0 {
            let id = node_with(s, "djmdHistory", &text(node, "Name"), &parent, 1, vec![("DateCreated", created)])?;
            book.histories.push((rb.clone(), uuid, id.clone()));
            placed.insert(rb, id);
            continue;
        }
        let id = s.fresh_id("djmdHistory", MAX_PLAYLIST_ID)?;
        let seq: i64 = s.conn.query_row(
            "SELECT COALESCE(MAX(Seq), 0) + 1 FROM djmdHistory WHERE ParentID = ?1 AND rb_local_deleted = 0",
            [&parent],
            |r| r.get(0),
        )?;
        let mut row = Row::new();
        row.insert("ID".into(), Value::Text(id.clone()));
        row.insert("Seq".into(), Value::Integer(seq));
        row.insert("Name".into(), node.get("Name").cloned().unwrap_or(Value::Null));
        row.insert("Attribute".into(), Value::Integer(0));
        row.insert("ParentID".into(), Value::Text(parent));
        row.insert("DateCreated".into(), created);
        row.insert("UUID".into(), Value::Text(s.rng.uuid4()));
        s.stamp_row(&mut row);
        insert(s.conn, "djmdHistory", &columns(s.conn, "djmdHistory")?, &row)?;
        let mut track_no = 0;
        for track in members(parts.source, "djmdSongHistory", "HistoryID", &rb)? {
            let Some(rbx) = ids.get(&track) else { continue };
            track_no += 1;
            let mut entry = Row::new();
            entry.insert("ID".into(), Value::Text(s.rng.uuid4()));
            entry.insert("HistoryID".into(), Value::Text(id.clone()));
            entry.insert("ContentID".into(), Value::Text(rbx.clone()));
            entry.insert("TrackNo".into(), Value::Integer(track_no));
            entry.insert("UUID".into(), Value::Text(s.rng.uuid4()));
            s.stamp_row(&mut entry);
            insert(s.conn, "djmdSongHistory", &entry_columns, &entry)?;
            if existing.contains(rbx) {
                *plays.entry(rbx.clone()).or_default() += 1;
                added += 1;
            }
        }
        sessions += 1;
        book.histories.push((rb.clone(), uuid, id.clone()));
        placed.insert(rb, id);
    }
    for (track, count) in plays {
        let usn = s.next_usn();
        s.conn.execute(
            "UPDATE djmdContent SET DJPlayCount = COALESCE(DJPlayCount, 0) + ?1, rb_local_usn = ?2, updated_at = ?3
             WHERE ID = ?4",
            params![count, usn, s.stamp, track],
        )?;
    }
    Ok((sessions, added))
}

/// The members JSON of each of rbxport's playlists, as a baseline records it.
pub(super) fn members_now(conn: &Connection) -> Result<HashMap<String, String>> {
    Ok(playlist_members(conn)?.into_iter().collect())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]
mod tests {
    use super::super::{first_import, import_again, open_import_db, DeltaSummary};
    use super::Take;
    use crate::fixture::{history_id, playlist_id, track_id};
    use crate::write::{TrackField, Writer};
    use crate::{Library, LibraryLocation, OpenMode};
    use std::path::{Path, PathBuf};

    /// rekordbox's library (a fixture), and rbxport's, imported from it once.
    struct World {
        root: tempfile::TempDir,
        rekordbox: LibraryLocation,
        rbxport: LibraryLocation,
    }

    fn world() -> World {
        let root = tempfile::tempdir().unwrap();
        let rb = root.path().join("rekordbox");
        std::fs::create_dir_all(&rb).unwrap();
        let rekordbox = crate::fixture::build(&rb, crate::fixture::Shape::default()).unwrap();
        let summary = first_import(&rekordbox, &root.path().join("rbxport").join("library"), &mut |_| {}).unwrap();
        let mut rbxport = summary.location;
        rbxport.is_user_library = false; // a temp folder; the test gate would refuse the real one
        World { root, rekordbox, rbxport }
    }

    impl World {
        fn rb(&self, sql: &str) {
            Library::open(self.rekordbox.clone(), OpenMode::ReadWrite).unwrap().connection().execute_batch(sql).unwrap();
        }

        fn ours(&self, sql: &str) {
            Library::open(self.rbxport.clone(), OpenMode::ReadWrite).unwrap().connection().execute_batch(sql).unwrap();
        }

        fn writer(&self) -> Writer {
            Writer::open(self.rbxport.clone(), self.root.path().join("backups")).unwrap()
        }

        fn import(&self) -> DeltaSummary {
            import_again(&self.rekordbox, &mut self.writer(), &Take::new(), &mut |_| {}).unwrap()
        }

        fn one<T: rusqlite::types::FromSql>(&self, sql: &str, args: &[&dyn rusqlite::ToSql]) -> T {
            let library = Library::open(self.rbxport.clone(), OpenMode::ReadOnly).unwrap();
            library.connection().query_row(sql, args, |r| r.get(0)).unwrap()
        }

        fn id_at(&self, path: &str) -> String {
            self.one("SELECT ID FROM djmdContent WHERE FolderPath = ?1 AND rb_local_deleted = 0", &[&path])
        }

        fn members(&self, playlist: &str) -> Vec<String> {
            let library = Library::open(self.rbxport.clone(), OpenMode::ReadOnly).unwrap();
            super::members(library.connection(), "djmdSongPlaylist", "PlaylistID", playlist).unwrap()
        }
    }

    fn put_file(share: &Path, relative: &str, bytes: &[u8]) {
        let at = rbl_anlz::resolve(share, relative);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(at, bytes).unwrap();
    }

    /// A row of `djmdContent`, as the fixture makes them.
    fn track(id: &str, title: &str, path: &str, uuid: &str) -> String {
        format!(
            "INSERT INTO djmdContent (ID, Title, FolderPath, FileNameL, BPM, Length, Rating, Analysed, UUID,
                 rb_data_status, rb_local_data_status, rb_local_deleted, rb_local_synced, created_at, updated_at)
             VALUES ('{id}', '{title}', '{path}', 'x.mp3', 12400, 300, 0, 105, '{uuid}', 0, 0, 0, 0, 'x', 'x');"
        )
    }

    fn cue(id: &str, track: &str, kind: i64, at: i64) -> String {
        format!(
            "INSERT INTO djmdCue (ID, ContentID, InMsec, Kind, Color, ColorTableIndex, created_at, updated_at)
             VALUES ('{id}', '{track}', {at}, {kind}, -1, 21, 'x', 'x');"
        )
    }

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

    #[test]
    fn a_repeat_import_takes_only_what_rbxport_does_not_have() {
        let w = world();
        let (t1, t2, t3, t4, t6, t7, t10, t11) =
            (track_id(1), track_id(2), track_id(3), track_id(4), track_id(6), track_id(7), track_id(10), track_id(11));

        // In rbxport since the first import: a title edited, a playlist
        // changed, a track deleted.
        let mut writer = w.writer();
        writer.set_field(&t2, TrackField::Title, "Mine").unwrap();
        writer.remove_tracks(&playlist_id(1), std::slice::from_ref(&track_id(5))).unwrap();
        writer.delete_track(&t7).unwrap();
        drop(writer);

        // In rekordbox since: tags changed on an untouched track and on the
        // edited one, a cue and an analysis on tracks rbxport has none of,
        // a new track with its cue and files, playlist additions, a new
        // playlist and a new history session.
        let new_dat = "/PIONEER/USBANLZ/P010/0001/ANLZ0000.DAT";
        let fill_dat = "/PIONEER/USBANLZ/P009/0001/ANLZ0000.DAT";
        w.rb(&format!(
            "INSERT INTO djmdGenre (ID, Name, created_at, updated_at) VALUES ('g1', 'Techno', 'x', 'x');
             UPDATE djmdContent SET Title = 'Renamed', GenreID = 'g1' WHERE ID = '{t1}';
             UPDATE djmdContent SET Commnt = 'from rekordbox' WHERE ID = '{t2}';
             UPDATE djmdContent SET Title = 'Back from the dead' WHERE ID = '{t7}';
             {}
             UPDATE djmdContent SET AnalysisDataPath = '{fill_dat}', BPM = 13000 WHERE ID = '{t6}';
             {}
             UPDATE djmdContent SET AnalysisDataPath = '{new_dat}' WHERE ID = '990001';
             {}
             INSERT INTO djmdSongPlaylist (ID, PlaylistID, ContentID, TrackNo, created_at, updated_at) VALUES
                 ('sp1', '{p0}', '990001', 6, 'x', 'x'), ('sp2', '{p0}', '{t10}', 7, 'x', 'x'),
                 ('sp3', '{p1}', '{t11}', 6, 'x', 'x'), ('sp4', 'p-new', '{t3}', 1, 'x', 'x'),
                 ('sp5', 'p-new', '990001', 2, 'x', 'x');
             INSERT INTO djmdPlaylist (ID, Seq, Name, Attribute, ParentID, UUID, created_at, updated_at)
                 VALUES ('p-new', 9, 'Fresh', 0, 'root', 'p-new-uuid', 'x', 'x');
             INSERT INTO djmdHistory (ID, Seq, Name, Attribute, ParentID, DateCreated, UUID, created_at, updated_at)
                 VALUES ('900001', 3, 'HISTORY 2026-11-04', 0, '202609', '2026-11-04 21:00:00', 'h-new', 'x', 'x');
             INSERT INTO djmdSongHistory (ID, HistoryID, ContentID, TrackNo, created_at, updated_at) VALUES
                 ('sh1', '900001', '{t3}', 1, 'x', 'x'), ('sh2', '900001', '990001', 2, 'x', 'x');",
            cue("c-fill", &t4, 0, 5000),
            track("990001", "Brand New", "/fixture/audio/new.mp3", "new-track-uuid"),
            cue("c-new", "990001", 1, 1234),
            p0 = playlist_id(0),
            p1 = playlist_id(1),
        ));
        put_file(&w.rekordbox.share_root, fill_dat, b"fill-dat");
        put_file(&w.rekordbox.share_root, &fill_dat.replace(".DAT", ".EXT"), b"fill-ext");
        put_file(&w.rekordbox.share_root, new_dat, b"new-dat");

        let source_dir = w.rekordbox.master_db.parent().unwrap().to_path_buf();
        let before = tree(&source_dir);
        let summary = w.import();
        let r = &summary.report;
        assert_eq!(tree(&source_dir), before, "nothing of rekordbox's is changed, added or removed");
        assert!(summary.source_unchanged);

        // The new track: in full, under a fresh ID, with its cue and files.
        let new = w.id_at("/fixture/audio/new.mp3");
        assert_ne!(new, "990001");
        assert_eq!(r.new_tracks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), [new.as_str()]);
        assert_eq!(w.one::<String>("SELECT Title FROM djmdContent WHERE ID = ?1", &[&new]), "Brand New");
        assert_eq!(w.one::<String>("SELECT UUID FROM djmdContent WHERE ID = ?1", &[&new]), "new-track-uuid");
        assert_eq!(w.one::<i64>("SELECT COUNT(*) FROM djmdCue WHERE ContentID = ?1 AND Kind = 1 AND InMsec = 1234", &[&new]), 1);
        assert_eq!(w.one::<String>("SELECT ContentUUID FROM djmdCue WHERE ContentID = ?1", &[&new]), "new-track-uuid");
        assert_eq!(w.one::<String>("SELECT AnalysisDataPath FROM djmdContent WHERE ID = ?1", &[&new]), new_dat);
        assert_eq!(std::fs::read(rbl_anlz::resolve(&w.rbxport.share_root, new_dat)).unwrap(), b"new-dat");

        // The untouched track takes rekordbox's tags; the edited one keeps its own.
        assert_eq!(r.updated.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), [t1.as_str()]);
        assert_eq!(w.one::<String>("SELECT Title FROM djmdContent WHERE ID = ?1", &[&t1]), "Renamed");
        assert_eq!(
            w.one::<String>("SELECT g.Name FROM djmdContent c JOIN djmdGenre g ON g.ID = c.GenreID WHERE c.ID = ?1", &[&t1]),
            "Techno"
        );
        assert_eq!(w.one::<String>("SELECT Title FROM djmdContent WHERE ID = ?1", &[&t2]), "Mine");
        assert_eq!(w.one::<Option<String>>("SELECT Commnt FROM djmdContent WHERE ID = ?1", &[&t2]), None);
        let skipped = r.skipped.iter().find(|s| s.track.id == t2).expect("t2 skipped");
        let fields: Vec<&str> = skipped.fields.iter().map(|f| f.field.as_str()).collect();
        assert_eq!(fields, ["title", "comment"]);
        assert_eq!((skipped.fields[0].rbxport.as_str(), skipped.fields[0].rekordbox.as_str()), ("Mine", "Track 002"));
        assert!(!skipped.linked_by_path);

        // Filled where rbxport had none; nothing replaced.
        let filled = |id: &str| r.filled.iter().find(|f| f.track.id == id).map(|f| f.what.clone());
        assert_eq!(filled(&t4), Some(vec![super::FillKind::MemoryCues]));
        assert_eq!(filled(&t6), Some(vec![super::FillKind::Analysis]));
        assert_eq!(w.one::<i64>("SELECT COUNT(*) FROM djmdCue WHERE ContentID = ?1", &[&t4]), 1);
        assert_eq!(w.one::<String>("SELECT AnalysisDataPath FROM djmdContent WHERE ID = ?1", &[&t6]), fill_dat);
        assert_eq!(w.one::<i64>("SELECT BPM FROM djmdContent WHERE ID = ?1", &[&t6]), 13_000);
        assert_eq!(std::fs::read(rbl_anlz::resolve(&w.rbxport.share_root, &fill_dat.replace(".DAT", ".EXT"))).unwrap(), b"fill-ext");
        assert_eq!((r.files_copied, r.cues), (3, 2));

        // Deleted in rbxport: left deleted.
        assert_eq!(r.kept_deleted, 1);
        assert_eq!(w.one::<i64>("SELECT rb_local_deleted FROM djmdContent WHERE ID = ?1", &[&t7]), 1);

        // Playlists: appended where unchanged in rbxport, made where new.
        let p0 = playlist_id(0);
        let mut expected: Vec<String> = (0..5).map(track_id).collect();
        expected.extend([new.clone(), t10.clone()]);
        assert_eq!(w.members(&p0), expected);
        assert_eq!(r.playlists_appended, [super::PlaylistLine { name: "Playlist 0".into(), tracks: 2 }]);
        assert!(!w.members(&playlist_id(1)).contains(&t11), "changed in rbxport, so nothing appended");
        // Track 5, taken out in rbxport, and track 11, added in rekordbox.
        assert_eq!(r.playlists_skipped, [super::PlaylistLine { name: "Playlist 1".into(), tracks: 2 }]);
        let fresh: String = w.one("SELECT ID FROM djmdPlaylist WHERE Name = 'Fresh' AND rb_local_deleted = 0", &[]);
        assert_eq!(w.members(&fresh), [t3.clone(), new.clone()]);

        // History: the new session, filed where rekordbox has it, and the
        // play counted on the track rbxport already had.
        let session: String = w.one("SELECT ID FROM djmdHistory WHERE Name = 'HISTORY 2026-11-04'", &[]);
        assert_eq!(w.one::<String>("SELECT ParentID FROM djmdHistory WHERE ID = ?1", &[&session]), "202609");
        assert_eq!(w.one::<i64>("SELECT COUNT(*) FROM djmdSongHistory WHERE HistoryID = ?1", &[&session]), 2);
        assert_eq!((r.history_sessions, r.plays_added), (1, 1));
        assert_eq!(w.one::<i64>("SELECT DJPlayCount FROM djmdContent WHERE ID = ?1", &[&t3]), 1);
        assert_eq!(w.one::<i64>("SELECT COUNT(*) FROM djmdHistory WHERE ID = ?1", &[&history_id(0)]), 1);

        // Recorded: the new mappings and baselines.
        let db = open_import_db(w.rbxport.master_db.parent().unwrap()).unwrap();
        let mapped: String = db.query_row("SELECT rbx_id FROM track_map WHERE rb_id = '990001'", [], |r| r.get(0)).unwrap();
        assert_eq!(mapped, new);
        let kinds: Vec<String> = db
            .prepare("SELECT kind FROM imports ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(std::result::Result::unwrap)
            .collect();
        assert_eq!(kinds, ["first", "delta"]);
        let log = std::fs::read_to_string(&summary.log).unwrap();
        assert!(log.contains("rekordbox's library unchanged: yes"), "{log}");
        assert!(log.contains("title: rbxport \"Mine\", rekordbox \"Track 002\""), "{log}");

        // Again, with nothing new in rekordbox: nothing changes.
        let again = w.import().report;
        assert!(again.new_tracks.is_empty() && again.updated.is_empty() && again.filled.is_empty(), "{again:?}");
        assert!(again.playlists_created.is_empty() && again.playlists_appended.is_empty());
        assert_eq!(again.history_sessions, 0);
        assert_eq!(again.skipped.iter().map(|s| s.track.id.as_str()).collect::<Vec<_>>(), [t2.as_str()]);
        assert_eq!(w.one::<i64>("SELECT DJPlayCount FROM djmdContent WHERE ID = ?1", &[&t3]), 1);

        // And a track rbxport never edited follows rekordbox again.
        w.rb(&format!("UPDATE djmdContent SET Title = 'Renamed twice' WHERE ID = '{t1}';"));
        assert_eq!(w.import().report.updated.len(), 1);
        assert_eq!(w.one::<String>("SELECT Title FROM djmdContent WHERE ID = ?1", &[&t1]), "Renamed twice");
    }

    #[test]
    fn rekordbox_tags_are_taken_for_kept_tracks_you_choose_while_they_still_differ_as_shown() {
        let w = world();
        let (t1, t2, t3) = (track_id(1), track_id(2), track_id(3));
        // Edited in rbxport: a title by hand, and a rating as a CDJ sets it
        // over LINK (Q1: that counts as an edit too).
        let mut writer = w.writer();
        writer.set_field(&t1, TrackField::Title, "Mine").unwrap();
        writer.set_field(&t2, TrackField::Title, "Mine too").unwrap();
        writer.set_rating(&t3, 4).unwrap();
        drop(writer);
        w.rb(&format!(
            "UPDATE djmdContent SET Commnt = 'from rekordbox' WHERE ID IN ('{t1}', '{t2}', '{t3}');"
        ));
        let first = w.import().report;
        let ids: Vec<&str> = first.skipped.iter().map(|s| s.track.id.as_str()).collect();
        assert_eq!(ids, [t1.as_str(), t2.as_str(), t3.as_str()]);

        // Take t1 and t2 as the report showed them; t2 then changes again
        // in rekordbox, so its choice no longer holds.
        let take: Take = first
            .skipped
            .iter()
            .filter(|s| s.track.id != t3)
            .map(|s| (s.track.id.clone(), s.fields.clone()))
            .collect();
        w.rb(&format!("UPDATE djmdContent SET Commnt = 'changed again' WHERE ID = '{t2}';"));
        let taken = import_again(&w.rekordbox, &mut w.writer(), &take, &mut |_| {}).unwrap().report;
        assert_eq!(taken.updated.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), [t1.as_str()]);
        assert_eq!(w.one::<String>("SELECT Title FROM djmdContent WHERE ID = ?1", &[&t1]), "Track 001");
        assert_eq!(w.one::<String>("SELECT Commnt FROM djmdContent WHERE ID = ?1", &[&t1]), "from rekordbox");
        assert_eq!(w.one::<String>("SELECT Title FROM djmdContent WHERE ID = ?1", &[&t2]), "Mine too");
        assert_eq!(taken.skipped.iter().map(|s| s.track.id.as_str()).collect::<Vec<_>>(), [t2.as_str(), t3.as_str()]);
        assert!(taken.problems.iter().any(|p| p.contains("changed after the report")), "{:?}", taken.problems);
        assert_eq!(w.one::<i64>("SELECT Rating FROM djmdContent WHERE ID = ?1", &[&t3]), 4);

        // Taken, t1 is untouched again: it follows rekordbox from now on.
        w.rb(&format!("UPDATE djmdContent SET Title = 'Renamed in rekordbox' WHERE ID = '{t1}';"));
        assert_eq!(w.import().report.updated.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), [t1.as_str()]);
        assert_eq!(w.one::<String>("SELECT Title FROM djmdContent WHERE ID = ?1", &[&t1]), "Renamed in rekordbox");
    }

    #[test]
    fn a_track_rbxport_has_by_its_file_is_linked_not_doubled() {
        let w = world();
        // Backslashes in rbxport's, and on Windows another case in rekordbox's.
        let (ours, theirs) =
            if cfg!(windows) { ("C:\\Music\\Linked.mp3", "c:/music/LINKED.mp3") } else { ("\\Music\\Linked.mp3", "/Music/Linked.mp3") };
        w.ours(&track("880001", "Linked, my way", ours, "own-uuid"));
        w.rb(&track("990003", "Linked", theirs, "theirs-uuid"));
        w.rb(&cue("c-linked", "990003", 1, 777));

        let r = w.import().report;
        assert!(r.new_tracks.is_empty(), "{r:?}");
        assert_eq!(r.path_matches.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["880001"]);
        // Its tags differ and nothing says whether rbxport changed them: kept.
        assert_eq!(w.one::<String>("SELECT Title FROM djmdContent WHERE ID = '880001'", &[]), "Linked, my way");
        let skipped = r.skipped.iter().find(|s| s.track.id == "880001").unwrap();
        assert!(skipped.linked_by_path);
        // Filled all the same: it had no hot cue.
        assert_eq!(w.one::<i64>("SELECT COUNT(*) FROM djmdCue WHERE ContentID = '880001' AND InMsec = 777", &[]), 1);
        let db = open_import_db(w.rbxport.master_db.parent().unwrap()).unwrap();
        let mapped: String = db.query_row("SELECT rbx_id FROM track_map WHERE rb_id = '990003'", [], |r| r.get(0)).unwrap();
        assert_eq!(mapped, "880001");

        // Next time it is matched by that mapping.
        let again = w.import().report;
        assert!(again.path_matches.is_empty() && again.new_tracks.is_empty());
    }

    #[test]
    fn a_new_rekordbox_track_whose_id_rbxport_uses_gets_a_fresh_one() {
        let w = world();
        w.ours(&track("990002", "Own", "/elsewhere/own.mp3", "own-990002"));
        w.rb(&track("990002", "Collide", "/fixture/audio/collide.mp3", "theirs-990002"));

        let r = w.import().report;
        assert_eq!(w.one::<String>("SELECT Title FROM djmdContent WHERE ID = '990002'", &[]), "Own");
        let new = w.id_at("/fixture/audio/collide.mp3");
        assert_ne!(new, "990002");
        assert_eq!(r.new_tracks.len(), 1);
        assert_eq!(w.one::<String>("SELECT Title FROM djmdContent WHERE ID = ?1", &[&new]), "Collide");
        let db = open_import_db(w.rbxport.master_db.parent().unwrap()).unwrap();
        let mapped: String = db.query_row("SELECT rbx_id FROM track_map WHERE rb_id = '990002'", [], |r| r.get(0)).unwrap();
        assert_eq!(mapped, new);
    }

    #[test]
    fn a_failed_write_leaves_no_copied_files_behind() {
        let w = world();
        let dat = "/PIONEER/USBANLZ/P011/0001/ANLZ0000.DAT";
        w.rb(&format!("{} UPDATE djmdContent SET AnalysisDataPath = '{dat}' WHERE ID = '990004';", track("990004", "N", "/n.mp3", "n-uuid")));
        put_file(&w.rekordbox.share_root, dat, b"dat");
        // rbxport's update counter is gone, so the transaction fails at its end.
        w.ours("DELETE FROM agentRegistry WHERE registry_id = 'localUpdateCount';");
        assert!(import_again(&w.rekordbox, &mut w.writer(), &Take::new(), &mut |_| {}).is_err());
        assert!(!rbl_anlz::resolve(&w.rbxport.share_root, dat).exists());
        assert_eq!(w.one::<i64>("SELECT COUNT(*) FROM djmdContent WHERE FolderPath = '/n.mp3'", &[]), 0);
    }

    #[test]
    fn paths_match_across_separators_and_on_windows_case() {
        assert_eq!(super::path_key("C:\\Music\\A.mp3"), super::path_key("C:/Music/A.mp3"));
        assert_eq!(super::path_key("C:/Music/A.mp3") == super::path_key("c:/music/a.mp3"), cfg!(windows));
    }

    #[test]
    fn differences_name_each_field_that_differs() {
        let diffs = super::differences(
            r#"{"title":"A","rating":3,"my_tags":["Peak"],"album":null}"#,
            r#"{"title":"B","rating":3,"my_tags":[],"album":null}"#,
        );
        let shown: Vec<(&str, &str, &str)> = diffs.iter().map(|d| (d.field.as_str(), d.rbxport.as_str(), d.rekordbox.as_str())).collect();
        assert_eq!(shown, [("title", "A", "B"), ("my_tags", "Peak", "")]);
    }
}
