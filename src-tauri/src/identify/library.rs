//! rbxport's own library as Identify reads and writes it.
//!
//! An accepted result is written to the library first, then to the file
//! (`docs/library/OWN_LIBRARY_SPEC.md`, "Identify after the switch"). The
//! library side goes through the single edit path: [`AppState::write_then`]
//! with a `rbl_db::write::Writer`, then [`refresh_after_edit`], then
//! `library:changed`. Each write keeps the exact values before and after
//! ([`rbl_db::write::TrackEditRecord`]), so undo can put them back, and only
//! while the library still holds what the write left.
//!
//! rekordbox's library is never written: the writer refuses it whatever
//! asks (`rbl_db::write_refusal_reason`).

use std::sync::Arc;

use rbl_db::write::{TrackEdit, TrackEditRecord, TrackField};
use rbl_identify::model::Field;

use crate::commands::{refresh_after_edit, Touched};
use crate::state::AppState;

/// What rbxport's library holds for a track, in the fields Identify writes.
/// Absent values are "".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LibraryRecord {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub label: String,
    pub genre: String,
    /// The year as text (`"2014"`); "" when unknown.
    pub year: String,
    pub comment: String,
    pub isrc: String,
    /// The track has artwork.
    pub artwork: bool,
}

impl LibraryRecord {
    /// What the library holds for `field`, or `None` for a field it does
    /// not keep (the MusicBrainz and Discogs IDs, the catalog number:
    /// those go to the file only). The sub-genre lives in the comment.
    #[must_use]
    pub fn value(&self, field: Field) -> Option<&str> {
        Some(match field {
            Field::Title => self.title.as_str(),
            Field::Artist => self.artist.as_str(),
            Field::Album => self.album.as_str(),
            Field::Label => self.label.as_str(),
            Field::Genre => self.genre.as_str(),
            Field::Year => self.year.as_str(),
            Field::SubGenre => self.comment.as_str(),
            Field::Isrc => self.isrc.as_str(),
            Field::Artwork => if self.artwork { "present" } else { "" },
            _ => return None,
        })
    }
}

/// Whether the library keeps `field` at all.
#[must_use]
pub fn kept(field: Field) -> bool {
    LibraryRecord::default().value(field).is_some()
}

/// One value to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryValue {
    Text(Field, String),
    /// A cover: the image's bytes and MIME type.
    Artwork(Vec<u8>, String),
}

/// rbxport's library, as Identify uses it. A trait so the identify tests
/// can stand in a fixture-backed one.
pub trait LibrarySide: Send + Sync {
    /// What the library holds for the track now.
    fn record(&self, track: &str) -> Result<LibraryRecord, String>;
    /// Writes `values` to the track, all or (as far as can be put back)
    /// none, and returns the exact edits made as JSON.
    fn write(&self, track: &str, values: &[LibraryValue]) -> Result<String, String>;
    /// Whether the library still holds what the edits in `edits_json` left.
    fn still_holds(&self, edits_json: &str) -> Result<bool, String>;
    /// Puts the edits in `edits_json` back, refused if any value changed since.
    fn undo(&self, edits_json: &str) -> Result<(), String>;
}

/// [`LibrarySide`] over the app's open library.
pub struct AppLibrary {
    state: Arc<AppState>,
    /// Tells the window: `library:changed` with the new generation.
    changed: Box<dyn Fn(u32) + Send + Sync>,
}

impl AppLibrary {
    pub fn new(state: Arc<AppState>, changed: Box<dyn Fn(u32) + Send + Sync>) -> Self {
        Self { state, changed }
    }

    /// Runs `edit` through the single edit path and refreshes the tracks.
    fn edit<T>(
        &self,
        edit: impl FnOnce(&mut rbl_db::write::Writer) -> Result<T, rbl_db::DbError>,
    ) -> Result<T, String> {
        let mut out = None;
        let generation = self
            .state
            .write_then(edit, |db, value| {
                out = Some(value);
                // Title, artist, year and artwork are columns the quick
                // metadata refresh does not re-read: the info panel's field
                // edits reload the tracks too.
                refresh_after_edit(&self.state, db, Touched::Tracks)
            })
            .map_err(|e| e.to_string())?;
        // As every edit outside the undo history does: a redo made before
        // it could put back a value from before this one.
        self.state.edit_history.lock().clear_redo();
        (self.changed)(generation);
        out.ok_or_else(|| "the library edit returned nothing".to_owned())
    }
}

impl LibrarySide for AppLibrary {
    fn record(&self, track: &str) -> Result<LibraryRecord, String> {
        let library = self.state.library().map_err(|e| e.message)?;
        let row = library.row_of(track).ok_or("The track is not in rbxport's library.")? as usize;
        let lookup = |ids: &[u32], names: &rbl_index::strings::Interner| ids.get(row).map_or("", |&n| names.name(n)).to_owned();
        let year = library.year.get(row).copied().unwrap_or(0);
        let isrc = self
            .state
            .read_db(|db| {
                db.connection()
                    .query_row("SELECT ISRC FROM djmdContent WHERE ID = ?1", [track], |r| r.get::<_, Option<String>>(0))
                    .map_err(Into::into)
            })
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        Ok(LibraryRecord {
            title: library.title.get(row).to_owned(),
            artist: lookup(&library.artist, &library.artists),
            album: lookup(&library.album, &library.albums),
            label: lookup(&library.label, &library.labels),
            genre: lookup(&library.genre, &library.genres),
            year: if year == 0 { String::new() } else { year.to_string() },
            comment: library.comment.get(row).to_owned(),
            isrc: isrc.trim().to_owned(),
            artwork: !library.artwork_path.get(row).is_empty(),
        })
    }

    fn write(&self, track: &str, values: &[LibraryValue]) -> Result<String, String> {
        // A cover goes through a file, as the writer takes one.
        let scratch = tempfile::tempdir().map_err(|e| format!("a temporary folder could not be made: {e}"))?;
        let edits = self.edit(|writer| {
            let mut made: Vec<TrackEdit> = Vec::with_capacity(values.len());
            let outcome = values.iter().try_for_each(|value| {
                let (_, edit) = write_one(writer, track, value, scratch.path())?;
                made.push(edit);
                Ok::<_, rbl_db::DbError>(())
            });
            if let Err(e) = outcome {
                // Each field is its own transaction: put back what landed.
                for edit in made.iter().rev() {
                    if let Err(undo) = writer.undo_track_edit(edit) {
                        tracing::warn!(error = %undo, "identify: a partial library write could not be put back");
                    }
                }
                return Err(e);
            }
            Ok(made)
        })?;
        let records: Vec<TrackEditRecord> = edits.iter().map(TrackEdit::record).collect();
        serde_json::to_string(&records).map_err(|e| format!("the library edits could not be recorded: {e}"))
    }

    fn still_holds(&self, edits_json: &str) -> Result<bool, String> {
        let edits = parse(edits_json)?;
        self.state.read_db(|db| rbl_db::write::track_edits_still_hold(db, &edits)).map_err(|e| e.to_string())
    }

    fn undo(&self, edits_json: &str) -> Result<(), String> {
        let edits = parse(edits_json)?;
        self.edit(|writer| writer.undo_track_edits_if_unchanged(&edits))
    }
}

/// The edits recorded by [`LibrarySide::write`].
pub fn parse(edits_json: &str) -> Result<Vec<TrackEdit>, String> {
    let records: Vec<TrackEditRecord> =
        serde_json::from_str(edits_json).map_err(|e| format!("the recorded library edits could not be read: {e}"))?;
    records.iter().map(|r| TrackEdit::from_record(r).map_err(|e| e.to_string())).collect()
}

/// Writes one value with the writer's own undo record.
pub fn write_one(
    writer: &mut rbl_db::write::Writer,
    track: &str,
    value: &LibraryValue,
    scratch: &std::path::Path,
) -> Result<(rbl_db::write::Changed, TrackEdit), rbl_db::DbError> {
    let column = |field: Field| match field {
        Field::Title => Some(TrackField::Title),
        Field::Artist => Some(TrackField::Artist),
        Field::Album => Some(TrackField::Album),
        Field::Label => Some(TrackField::Label),
        Field::Genre => Some(TrackField::Genre),
        Field::Year => Some(TrackField::Year),
        _ => None,
    };
    match value {
        LibraryValue::Text(Field::Year, text) => writer.set_field_with_undo(track, TrackField::Year, &year_of(text)),
        LibraryValue::Text(Field::SubGenre, text) => writer.set_comment_with_undo(track, text),
        LibraryValue::Text(Field::Isrc, text) => writer.set_isrc_with_undo(track, text),
        LibraryValue::Text(field, text) => match column(*field) {
            Some(column) => writer.set_field_with_undo(track, column, text),
            None => Err(rbl_db::DbError::WriteRefused(format!("rbxport's library keeps no {}", field.as_str()))),
        },
        LibraryValue::Artwork(bytes, mime) => {
            let extension = if mime.eq_ignore_ascii_case("image/png") { "png" } else { "jpg" };
            let image = scratch.join(format!("cover.{extension}"));
            std::fs::write(&image, bytes)?;
            writer.set_artwork_with_undo(track, Some(&image))
        }
    }
}

/// The year in a date (`2014-05-01` → `2014`); the text itself otherwise,
/// which the writer refuses if it is not a number.
#[must_use]
pub fn year_of(text: &str) -> String {
    let text = text.trim();
    match text.get(..4) {
        Some(year)
            if year.chars().all(|c| c.is_ascii_digit()) && text.get(4..).is_some_and(|rest| rest.starts_with(['-', '/', '.'])) =>
        {
            year.to_owned()
        }
        _ => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_year_is_taken_from_a_date() {
        assert_eq!(year_of("2014"), "2014");
        assert_eq!(year_of("2014-05-01"), "2014");
        assert_eq!(year_of(" 1999/12 "), "1999");
        assert_eq!(year_of("20140"), "20140");
    }

    #[test]
    fn the_library_keeps_only_its_own_fields() {
        for field in [Field::Title, Field::Artist, Field::Album, Field::Label, Field::Genre, Field::Year, Field::SubGenre, Field::Isrc, Field::Artwork] {
            assert!(kept(field), "{field:?}");
        }
        for field in [Field::CatalogNumber, Field::MbRecordingId, Field::MbReleaseId, Field::DiscogsReleaseId] {
            assert!(!kept(field), "{field:?}");
        }
    }
}
