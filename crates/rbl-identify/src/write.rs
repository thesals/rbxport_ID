//! Writing accepted changes to a file's tags, and putting them back.
//!
//! Only the audio file is ever written: identification is one-way and never
//! changes the rekordbox library (it only reads it, into its own store).
//!
//! Nothing here is ever called without the user: a write happens only on an
//! explicit apply (or auto-apply, off by default and only for fill-only
//! proposals), and always as the second step of a recorded application
//! ([`apply_result`]): the file's tags are snapshotted in the store before
//! the file is touched, so every write can be undone.
//!
//! # What is written
//!
//! Only the fields of the proposal the user ticked. Rating, colour, cue
//! points, the beat grid, BPM, key and analysis data are not [`Field`]s and
//! are never touched. The comment is touched only to APPEND a sub-genre
//! ([`crate::comment`]): the styles are appended to the comment as the file
//! holds it at write time, never written verbatim from the proposal.
//!
//! Every change is checked against the file as it is now: a field whose
//! value is no longer the one the proposal was made against is refused (the
//! user, or another program, changed it since), so nothing the user has not
//! seen is ever overwritten.
//!
//! # ID3 (MP3)
//!
//! An `ID3v2.3` or `ID3v2.4` tag at the start of the file is edited frame by
//! frame: the version stays as it was, every frame not being changed is
//! copied byte for byte (`APIC`, `PRIV`, `UFID`, `WOAF`, `TKEY`, `TBPM`, other
//! `TXXX` and `COMM` frames, ...), and the `ID3v1` trailer and the audio are
//! left exactly as they were. A field already held in a frame (a variant
//! such as `TXXX:Catalog #` included) is updated in that frame, in its own
//! text encoding where it can hold the new text; a field without one gets
//! the standard frame: `TIT2`, `TPE1`, `TALB`, `TPUB`, `TYER` (2.3) or `TDRC`
//! (2.4), `TCON`, `TSRC`, `TXXX:CATALOGNUMBER`, Picard's MusicBrainz names
//! (`UFID:http://musicbrainz.org`, `TXXX:MusicBrainz Album Id`, ...),
//! `TXXX:DISCOGS_RELEASE_ID`, a front-cover `APIC`, and a `COMM` (`eng`, no
//! description) for the comment. While the edited frames fit in the old tag
//! with its padding, the tag keeps its size, so the audio does not move. An
//! MPEG file without an `ID3v2` tag gets a new `ID3v2.3` one. A tag this
//! writer cannot edit safely (`ID3v2.2`, whole-tag unsynchronisation, a
//! compressed or encrypted frame to change) is refused, never rewritten.
//!
//! # Other formats
//!
//! FLAC, MP4 and the rest go through lofty's tag writer, which
//! keeps what it can read of the other fields. Undo there puts the changed
//! fields' recorded values back, rather than bytes, and checks they read
//! back. Their comment items carry no description, so the comment there is
//! simply the comment item.
//!
//! # On disk
//!
//! A write builds the whole new file beside the original (a hidden
//! temporary file in the same folder, so the same volume) and renames it
//! over the original, so a crash leaves either the old file or the new one.
//! On Windows the original creation time is kept; the modification time is
//! the write's, as for any tag editor. A read-only file is refused with a
//! clear message. A file another program holds open is not written now: the
//! write is kept in the store as a waiting write ([`apply_result`]) and made
//! once the file is free ([`retry_waiting`]). The rename replaces
//! the file object, so alternate data streams (`Zone.Identifier`), hard
//! links and explicit ACLs of the original are not carried over. A crash
//! between staging and renaming can leave a `.rbxport-tags-*` file beside
//! the track; [`remove_stale_temps`] deletes those (exactly that name, and
//! nothing else) before the next write or undo there and when the store is
//! reconciled at startup.
//!
//! AIFF and WAV keep their ID3 tag in a chunk: it is edited with the same
//! frame-level editor, the chunk's and the container's sizes set to match,
//! every other chunk left in place byte for byte, and undo puts the chunk's
//! recorded bytes back exactly.

use std::io::Write as _;
use std::path::Path;

use base64::Engine as _;
use lofty::config::WriteOptions;
use lofty::file::{AudioFile, FileType, TaggedFileExt};
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::ItemKey;
use lofty::probe::Probe;
use lofty::tag::Tag;

use crate::error::{IdentifyError, Result};
use crate::clock::Deadline;
use crate::model::{ArtworkRef, Field, FieldChange, TagSnapshot};
use crate::pipeline::current;
use crate::store::{ApplicationId, ResultId, ReviewState, Store, TrackKey, WaitingWrite};
use crate::tags;

/// One planned change to a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    /// The field's whole new text.
    Set(Field, String),
    /// Every frame holding the field is removed.
    Clear(Field),
    /// A front cover for a file without any.
    AddArtwork,
}

impl Edit {
    #[must_use]
    pub fn field(&self) -> Field {
        match self {
            Self::Set(field, _) | Self::Clear(field) => *field,
            Self::AddArtwork => Field::Artwork,
        }
    }
}

/// An image to embed, as the store holds it.
#[derive(Debug, Clone, Copy)]
pub struct Image<'a> {
    pub bytes: &'a [u8],
    pub mime: &'a str,
}

/// Proof that the user lets identification write to audio files: every
/// function here that changes a file ([`write`], [`write_until`],
/// [`restore`], [`apply_result`], [`retry_waiting`], [`undo_result`])
/// takes one, so no write can be made without it.
///
/// The app makes one in exactly one place, its file-writes gate, from the
/// user's "Write tags to audio files" setting read just before each write
/// (off by default). Nothing else should call [`FileWritesAllowed::if_enabled`]
/// outside tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileWritesAllowed {
    _granted: (),
}

impl FileWritesAllowed {
    /// The proof, when the user's file-writes setting is on; `None` (no
    /// write may be made) when it is off.
    #[must_use]
    pub fn if_enabled(setting_on: bool) -> Option<Self> {
        setting_on.then_some(Self { _granted: () })
    }
}

fn refuse(message: impl Into<String>) -> IdentifyError {
    IdentifyError::Write(message.into())
}

fn changed(message: impl Into<String>) -> IdentifyError {
    IdentifyError::Changed(message.into())
}

/// Why a write to a file another program holds open was refused.
pub const OPEN_ELSEWHERE: &str = "The file is open in another program (a player, or rekordbox).";

/// What a write kept to be made later answers, naming the file.
fn deferred(path: &Path) -> IdentifyError {
    let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
    IdentifyError::Deferred(format!(
        "{name} is open in another program. It will be written when it's free: when rbxport starts or closes, \
         or when you retry."
    ))
}

/// A failure to read or open the file while another program holds it open
/// is that program's doing: [`IdentifyError::InUse`]. Everything else is as
/// it was.
fn in_use_or(path: &Path, e: IdentifyError) -> IdentifyError {
    match e {
        IdentifyError::Io(_) | IdentifyError::Audio(_) if held_open(path) => IdentifyError::InUse(OPEN_ELSEWHERE.to_owned()),
        other => other,
    }
}

/// The edits that carry out `changes` on a file whose tags are `now`.
///
/// Refused when a field's value in the file is no longer the `current` the
/// change was proposed against (the file was changed since it was
/// identified), when artwork is proposed for a file that has some, and
/// when the comment cannot be read. A sub-genre becomes the file's comment
/// as it is now with the proposal's styles appended; a change that would
/// add nothing is dropped.
pub fn plan(now: &TagSnapshot, changes: &[FieldChange]) -> Result<Vec<Edit>> {
    let mut edits = Vec::with_capacity(changes.len());
    for change in changes {
        let field = change.field;
        if field == Field::Artwork {
            if now.has_artwork {
                return Err(changed("The file has artwork now; identify it again before adding any."));
            }
            edits.push(Edit::AddArtwork);
            continue;
        }
        let value = current(now, field);
        if value.trim() != change.current.trim() {
            return Err(changed(format!(
                "The file's {} has changed since it was identified. Identify it again.",
                field.as_str()
            )));
        }
        if field == Field::SubGenre {
            if value.contains(tags::UNREADABLE) {
                return Err(refuse("The file's comment cannot be read, so nothing is appended to it."));
            }
            let added = crate::comment::added(&change.current, &change.proposed);
            let appended = crate::comment::append(&value, &added.join(crate::comment::SEPARATOR));
            if appended != value {
                edits.push(Edit::Set(field, appended));
            }
            continue;
        }
        let proposed = change.proposed.trim();
        edits.push(if proposed.is_empty() { Edit::Clear(field) } else { Edit::Set(field, proposed.to_owned()) });
    }
    Ok(edits)
}

/// Writes `edits` to the file, whose tags must still be `before` (taken by
/// the caller and recorded in the store first). Returns the tags as they
/// are after the write, read back from the file.
///
/// The write is checked by reading the file back: if any edit did not land
/// as planned, the original tags are put back and the write is refused.
pub fn write(
    path: &Path,
    before: &TagSnapshot,
    edits: &[Edit],
    artwork: Option<Image<'_>>,
    allowed: FileWritesAllowed,
) -> Result<TagSnapshot> {
    write_until(path, before, edits, artwork, Deadline::NONE, allowed)
}

/// [`write`], stopping with [`IdentifyError::OutOfTime`] once `deadline`
/// passes while the file is read or the new one is staged. A stop always
/// comes before the rename over the original: the file is never changed,
/// and the staged copy is deleted. The rename itself, and putting the file
/// back after a failed check, are never cut short.
pub fn write_until(
    path: &Path,
    before: &TagSnapshot,
    edits: &[Edit],
    artwork: Option<Image<'_>>,
    deadline: Deadline<'_>,
    _allowed: FileWritesAllowed,
) -> Result<TagSnapshot> {
    if edits.is_empty() {
        return Err(refuse("Nothing to write."));
    }
    if edits.contains(&Edit::AddArtwork) && artwork.is_none() {
        return Err(refuse("The proposed artwork is not in the identify store."));
    }
    // A leftover from a crash during an earlier write beside this file.
    remove_stale_temps(path);
    let original = read_until(path, deadline)?;
    if tags::read(path)?.snapshot != *before {
        return Err(refuse("The file's tags changed while the write was being prepared. Try again."));
    }
    let raw = match id3_kind(path, &original)? {
        Id3Kind::Tagged | Id3Kind::Untagged => {
            let rewritten = id3::rewrite(&original, edits, artwork)?;
            replace_until(path, &rewritten, deadline)?;
            true
        }
        Id3Kind::Chunk(container) => {
            let tag = chunk::payload(&original, container)?;
            let new_tag = id3::rewrite_tag(tag, edits, artwork)?;
            replace_until(path, &chunk::splice(&original, container, Some(&new_tag))?, deadline)?;
            true
        }
        Id3Kind::Other => {
            lofty_write(path, edits, artwork, deadline)?;
            false
        }
    };
    let after = tags::read(path)?.snapshot;
    if let Err(e) = verify(&after, edits) {
        // Put the file back before reporting: nothing half-written stays.
        if raw {
            replace(path, &original)?;
        } else {
            lofty_restore(path, before, &edits.iter().map(Edit::field).collect::<Vec<_>>())?;
        }
        return Err(e);
    }
    Ok(after)
}

/// Every edit reads back as planned.
fn verify(after: &TagSnapshot, edits: &[Edit]) -> Result<()> {
    for edit in edits {
        let ok = match edit {
            Edit::AddArtwork => after.has_artwork,
            Edit::Clear(field) => current(after, *field).is_empty(),
            Edit::Set(Field::Year, value) => current(after, Field::Year).get(..4) == value.get(..4),
            Edit::Set(field, value) => current(after, *field) == *value,
        };
        if !ok {
            return Err(refuse(format!(
                "The file's {} did not read back as written; its tags were put back.",
                edit.field().as_str()
            )));
        }
    }
    Ok(())
}

/// Whether `now` is exactly what writing `changes` (as an application
/// records them) over `before` leaves: every changed field holds its new
/// value (as [`write`] checks it), and every other field, and the artwork's
/// presence, is as it was. What startup reconciliation uses to recognise a
/// write that landed before its outcome was recorded.
#[must_use]
pub fn holds_exactly(before: &TagSnapshot, now: &TagSnapshot, changes: &[FieldChange]) -> bool {
    let is_changed = |field: Field| changes.iter().any(|c| c.field == field);
    let landed = changes.iter().all(|change| {
        let proposed = change.proposed.trim();
        match change.field {
            Field::Artwork => now.has_artwork,
            Field::Year => {
                let now = current(now, Field::Year);
                if proposed.is_empty() { now.is_empty() } else { now.get(..4) == proposed.get(..4) }
            }
            field => current(now, field) == proposed,
        }
    });
    let rest_kept = before
        .values
        .keys()
        .chain(now.values.keys())
        .filter(|field| !is_changed(**field))
        .all(|&field| current(now, field) == current(before, field));
    landed && rest_kept && (is_changed(Field::Artwork) || now.has_artwork == before.has_artwork)
}

/// Puts the file's tags back to `before`, undoing a write whose result was
/// `after`. Refused when the file's tags are no longer `after`: the user
/// or another program changed them since, and restoring would lose that.
/// A file already back at `before` is left alone. `fields` are the ones the
/// write changed (used where tags are restored field by field).
pub fn restore(
    path: &Path,
    before: &TagSnapshot,
    after: &TagSnapshot,
    fields: &[Field],
    _allowed: FileWritesAllowed,
) -> Result<()> {
    remove_stale_temps(path);
    let now = tags::read(path)?.snapshot;
    if now == *before {
        return Ok(());
    }
    if now != *after {
        return Err(refuse(
            "The file's tags have changed since they were written, so they are not put back. \
             Edit them by hand, or identify the file again.",
        ));
    }
    // An ID3 tag (at the start, or in an AIFF/WAV chunk) was written frame
    // by frame: put the recorded block back exactly (none, for a file that
    // had none), everything else untouched.
    let raw_written = after.raw_id3.as_ref().and_then(|r| r.v2.as_ref()).is_some();
    if raw_written {
        let file = std::fs::read(path).map_err(|e| io_error(path, &e))?;
        let block = before
            .raw_id3
            .as_ref()
            .and_then(|r| r.v2.as_deref())
            .map(|b| base64::engine::general_purpose::STANDARD_NO_PAD.decode(b))
            .transpose()
            .map_err(|_| refuse("The recorded tags cannot be decoded."))?;
        let restored = if let Some(container) = tags::Container::of(&file) {
            chunk::splice(&file, container, block.as_deref())?
        } else {
            let audio_at = id3::tag_len(&file).unwrap_or(0);
            let mut restored = block.unwrap_or_default();
            restored.extend_from_slice(file.get(audio_at..).unwrap_or_default());
            restored
        };
        replace(path, &restored)?;
    } else {
        lofty_restore(path, before, fields)?;
    }
    let back = tags::read(path)?.snapshot;
    let landed = if raw_written {
        back == *before
    } else {
        fields.iter().all(|&field| match field {
            Field::Artwork => back.has_artwork == before.has_artwork,
            field => current(&back, field) == current(before, field),
        })
    };
    if !landed {
        return Err(refuse("The file's tags did not read back as they were before the write."));
    }
    Ok(())
}

enum Id3Kind {
    /// An `ID3v2.3`/`2.4` tag at the start: edited frame by frame.
    Tagged,
    /// An MPEG file with no `ID3v2` tag: one is added.
    Untagged,
    /// An AIFF or WAV file: its ID3 chunk is edited frame by frame (one is
    /// added to a file without).
    Chunk(tags::Container),
    /// Anything else: lofty.
    Other,
}

fn id3_kind(path: &Path, bytes: &[u8]) -> Result<Id3Kind> {
    if let Some(container) = tags::Container::of(bytes) {
        return Ok(Id3Kind::Chunk(container));
    }
    if bytes.starts_with(b"ID3") {
        return match (bytes.get(3), bytes.get(5)) {
            (Some(3 | 4), Some(flags)) if flags & 0x80 == 0 => Ok(Id3Kind::Tagged),
            _ => Err(refuse("This file's ID3 tag (an old version, or unsynchronised) cannot be edited safely.")),
        };
    }
    let probe = Probe::open(path).map_err(|e| refuse(format!("The file's format could not be told: {e}")))?;
    let kind: Option<FileType> = probe
        .guess_file_type()
        .map_err(|e| refuse(format!("The file's format could not be told: {e}")))?
        .file_type();
    Ok(if kind == Some(FileType::Mpeg) { Id3Kind::Untagged } else { Id3Kind::Other })
}

/// The staged files' name: `.rbxport-tags-`, tempfile's six random
/// characters, then `.tmp` ([`replace`]) or the track's own extension
/// ([`stage_copy`]).
const TEMP_PREFIX: &str = ".rbxport-tags-";
const TEMP_RANDOM_LEN: usize = 6;

/// Whether `name` is exactly a staged file this writer makes beside a track
/// with extension `ext` (`None` for a track without one).
fn is_our_temp(name: &str, ext: Option<&str>) -> bool {
    let Some(rest) = name.strip_prefix(TEMP_PREFIX) else { return false };
    let (random, suffix) = rest.split_at(rest.char_indices().nth(TEMP_RANDOM_LEN).map_or(rest.len(), |(i, _)| i));
    if random.len() != TEMP_RANDOM_LEN || !random.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return false;
    }
    suffix == ".tmp" || ext.is_some_and(|ext| !ext.is_empty() && suffix.strip_prefix('.') == Some(ext))
}

/// Deletes staged files a crash between staging and renaming left beside
/// `track` (the file whose tags were being written): plain files named
/// exactly as [`replace`] and [`stage_copy`] name them, nothing else.
/// Best effort: one that cannot be deleted (open elsewhere) stays. Returns
/// how many were deleted.
pub fn remove_stale_temps(track: &Path) -> usize {
    let Some(folder) = track.parent().filter(|p| !p.as_os_str().is_empty()) else { return 0 };
    let ext = track.extension().and_then(|e| e.to_str());
    let Ok(entries) = std::fs::read_dir(folder) else { return 0 };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_our_temp(name, ext) {
            continue;
        }
        // Never follow a link or touch a folder, whatever its name.
        let is_file = std::fs::symlink_metadata(entry.path()).is_ok_and(|m| m.file_type().is_file());
        if is_file && std::fs::remove_file(entry.path()).is_ok() {
            tracing::info!("identify: removed a staged tag file left by an interrupted write");
            removed += 1;
        }
    }
    removed
}

fn io_error(path: &Path, e: &std::io::Error) -> IdentifyError {
    IdentifyError::Io(std::io::Error::new(e.kind(), format!("{}: {e}", path.display())))
}

/// Replaces the file with `bytes`: staged beside it, flushed, renamed over
/// it. Keeps the creation time on Windows.
pub(crate) fn replace(path: &Path, bytes: &[u8]) -> Result<()> {
    replace_until(path, bytes, Deadline::NONE)
}

/// How much is read or staged between two looks at the deadline.
const CHUNK: usize = 1 << 20;

/// Why a write stopped for its deadline.
fn out_of_time() -> IdentifyError {
    IdentifyError::OutOfTime("There was no time left to write the file; nothing was changed.".to_owned())
}

/// The whole file, read in chunks, stopping once `deadline` passes.
fn read_until(path: &Path, deadline: Deadline<'_>) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path).map_err(|e| io_error(path, &e))?;
    let mut bytes = Vec::with_capacity(file.metadata().map_or(0, |m| usize::try_from(m.len()).unwrap_or(0)));
    let mut chunk = vec![0_u8; CHUNK];
    loop {
        if deadline.passed() {
            return Err(out_of_time());
        }
        let n = file.read(&mut chunk).map_err(|e| io_error(path, &e))?;
        if n == 0 {
            return Ok(bytes);
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
}

/// Writes `bytes` to a staged file in chunks, stopping once `deadline`
/// passes (the staged file is then deleted by its owner).
fn stage_until(staged: &mut std::fs::File, bytes: &[u8], deadline: Deadline<'_>) -> Result<()> {
    for chunk in bytes.chunks(CHUNK) {
        if deadline.passed() {
            return Err(out_of_time());
        }
        staged.write_all(chunk).map_err(|e| refuse(format!("The new tags could not be staged: {e}")))?;
    }
    Ok(())
}

/// [`replace`], stopping before the rename once `deadline` passes.
fn replace_until(path: &Path, bytes: &[u8], deadline: Deadline<'_>) -> Result<()> {
    let meta = std::fs::metadata(path).map_err(|e| io_error(path, &e))?;
    if meta.permissions().readonly() {
        return Err(refuse("The file is read-only. Clear its read-only attribute to write its tags."));
    }
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let mut staged = tempfile::Builder::new()
        .prefix(TEMP_PREFIX)
        .rand_bytes(TEMP_RANDOM_LEN)
        .suffix(".tmp")
        .tempfile_in(parent)
        .map_err(|e| refuse(format!("The file's folder cannot be written to: {e}")))?;
    stage_until(staged.as_file_mut(), bytes, deadline)?;
    staged.as_file().sync_all().map_err(|e| refuse(format!("The new tags could not be staged: {e}")))?;
    #[cfg(windows)]
    if let Ok(created) = meta.created() {
        use std::os::windows::fs::FileTimesExt as _;
        // Best effort: the tags are what matter.
        let _ = staged.as_file().set_times(std::fs::FileTimes::new().set_created(created));
    }
    // The last moment to stop: after this the file is replaced.
    if deadline.passed() {
        return Err(out_of_time());
    }
    staged.persist(path).map_err(|e| replace_error(&e.error, held_open(path)))?;
    #[cfg(unix)]
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// `ERROR_SHARING_VIOLATION` and `ERROR_LOCK_VIOLATION`: another program
/// has the file open (or part of it locked).
const SHARING_VIOLATION: i32 = 32;
const LOCK_VIOLATION: i32 = 33;

/// Why renaming the staged file over the track failed, in the user's terms:
/// only a sharing or lock violation is "open in another program". Windows
/// answers a rename over a file another program holds open with ACCESS
/// DENIED rather than a sharing violation, so access denied counts as open
/// elsewhere only when `held_open` (a probe that hit a sharing violation)
/// says so; otherwise it is a read-only file or missing rights, said as such.
fn replace_error(e: &std::io::Error, held_open: bool) -> IdentifyError {
    let open_elsewhere = || IdentifyError::InUse(OPEN_ELSEWHERE.to_owned());
    match e.raw_os_error() {
        Some(SHARING_VIOLATION | LOCK_VIOLATION) => open_elsewhere(),
        _ if e.kind() == std::io::ErrorKind::PermissionDenied && held_open => open_elsewhere(),
        _ if e.kind() == std::io::ErrorKind::PermissionDenied => refuse(
            "The file could not be replaced: access was denied. It may be read-only, or you may not have permission \
             to change it or its folder.",
        ),
        _ => refuse(format!("The file could not be replaced: {e}")),
    }
}

/// Whether another program has `path` open: opening it with no sharing at
/// all fails with a sharing violation exactly then. Windows only; elsewhere
/// an open file never blocks the rename.
pub(crate) fn held_open(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        let probe = std::fs::OpenOptions::new().read(true).share_mode(0).open(path);
        matches!(probe.map_err(|e| e.raw_os_error()), Err(Some(SHARING_VIOLATION | LOCK_VIOLATION)))
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

// ------------------------------------------------------------------ lofty

/// The key a field is written under in a lofty tag: the one already
/// holding it when there is one (a variant spelling included), else the
/// first the reader looks for.
fn lofty_key(tag: &Tag, field: Field) -> Option<ItemKey> {
    let keys = tags::keys(field);
    keys.iter().find(|k| tag.get_string(k).is_some()).cloned().or_else(|| keys.into_iter().next())
}

fn lofty_write(path: &Path, edits: &[Edit], artwork: Option<Image<'_>>, deadline: Deadline<'_>) -> Result<()> {
    let staged = stage_copy(path, deadline)?;
    let mut tagged = Probe::open(staged.path())
        .and_then(Probe::read)
        .map_err(|e| refuse(format!("The file's tags could not be read for writing: {e}")))?;
    let tag_type = tagged.primary_tag_type();
    if tagged.tag(tag_type).is_none() {
        tagged.insert_tag(Tag::new(tag_type));
    }
    let tag = tagged.tag_mut(tag_type).ok_or_else(|| refuse("The file cannot hold tags."))?;
    for edit in edits {
        match edit {
            Edit::AddArtwork => {
                let image = artwork.ok_or_else(|| refuse("The proposed artwork is not in the identify store."))?;
                tag.push_picture(Picture::new_unchecked(
                    PictureType::CoverFront,
                    Some(MimeType::from_str(image.mime)),
                    None,
                    image.bytes.to_vec(),
                ));
            }
            Edit::Clear(field) => {
                for key in tags::keys(*field) {
                    tag.remove_key(&key);
                }
            }
            Edit::Set(field, value) => {
                let key = lofty_key(tag, *field).ok_or_else(|| refuse(format!("{} has no tag here.", field.as_str())))?;
                tag.remove_key(&key);
                if !tag.insert_text(key, value.clone()) {
                    return Err(refuse(format!("This file's tag format cannot hold {}.", field.as_str())));
                }
            }
        }
    }
    tagged
        .save_to_path(staged.path(), WriteOptions::default())
        .map_err(|e| refuse(format!("The tags could not be written: {e}")))?;
    publish(path, staged, deadline)
}

/// Puts the recorded values of `fields` back with lofty.
fn lofty_restore(path: &Path, before: &TagSnapshot, fields: &[Field]) -> Result<()> {
    let staged = stage_copy(path, Deadline::NONE)?;
    let mut tagged = Probe::open(staged.path())
        .and_then(Probe::read)
        .map_err(|e| refuse(format!("The file's tags could not be read: {e}")))?;
    let tag_type = tagged.primary_tag_type();
    if tagged.tag(tag_type).is_none() {
        tagged.insert_tag(Tag::new(tag_type));
    }
    let tag = tagged.tag_mut(tag_type).ok_or_else(|| refuse("The file cannot hold tags."))?;
    for &field in fields {
        if field == Field::Artwork {
            if !before.has_artwork {
                tag.remove_picture_type(PictureType::CoverFront);
            }
            continue;
        }
        let keys = tags::keys(field);
        for key in &keys {
            tag.remove_key(key);
        }
        for frame in before.values.get(&field).into_iter().flatten() {
            let key = keys.iter().find(|k| tags::frame_name(k, tag_type) == frame.frame).or_else(|| keys.first());
            if let Some(key) = key {
                for value in &frame.values {
                    let _ = tag.push(lofty::tag::TagItem::new(key.clone(), lofty::tag::ItemValue::Text(value.clone())));
                }
            }
        }
    }
    tagged
        .save_to_path(staged.path(), WriteOptions::default())
        .map_err(|e| refuse(format!("The tags could not be written: {e}")))?;
    publish(path, staged, Deadline::NONE)
}

/// A copy of the file beside it, to write tags into before it replaces the
/// original.
fn stage_copy(path: &Path, deadline: Deadline<'_>) -> Result<tempfile::NamedTempFile> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let mut staged = tempfile::Builder::new()
        .prefix(TEMP_PREFIX)
        .rand_bytes(TEMP_RANDOM_LEN)
        .suffix(&path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default())
        .tempfile_in(parent)
        .map_err(|e| refuse(format!("The file's folder cannot be written to: {e}")))?;
    let mut source = std::fs::File::open(path).map_err(|e| io_error(path, &e))?;
    let mut chunk = vec![0_u8; CHUNK];
    loop {
        if deadline.passed() {
            return Err(out_of_time());
        }
        let n = std::io::Read::read(&mut source, &mut chunk).map_err(|e| refuse(format!("The file could not be staged: {e}")))?;
        if n == 0 {
            return Ok(staged);
        }
        staged.as_file_mut().write_all(&chunk[..n]).map_err(|e| refuse(format!("The file could not be staged: {e}")))?;
    }
}

/// Renames a staged copy over the original, as [`replace`] does.
fn publish(path: &Path, staged: tempfile::NamedTempFile, deadline: Deadline<'_>) -> Result<()> {
    let bytes = std::fs::read(staged.path()).map_err(|e| refuse(format!("The staged file could not be read: {e}")))?;
    drop(staged);
    replace_until(path, &bytes, deadline)
}

// ------------------------------------------------------------- AIFF / WAV

mod chunk {
    use super::refuse;
    use crate::error::Result;
    use crate::tags::{find_id3_chunk, Container};

    /// The ID3 chunk's payload; `None` when the file has none.
    pub(super) fn payload(file: &[u8], container: Container) -> Result<Option<&[u8]>> {
        let len = file.len() as u64;
        let found = find_id3_chunk(&mut std::io::Cursor::new(file), container, len)
            .map_err(|e| refuse(format!("The file's chunks could not be read: {e}")))?;
        Ok(found.map(|(at, size)| {
            let start = usize::try_from(at + 8).unwrap_or(usize::MAX);
            let end = start.saturating_add(usize::try_from(size).unwrap_or(usize::MAX)).min(file.len());
            &file[start.min(end)..end]
        }))
    }

    fn size_bytes(container: Container, size: u32) -> [u8; 4] {
        match container {
            Container::Aiff => size.to_be_bytes(),
            Container::Wav => size.to_le_bytes(),
        }
    }

    /// The file with its ID3 chunk's payload replaced by `tag` (the chunk
    /// added at the end when there was none, removed when `tag` is `None`),
    /// the chunk's size and pad byte and the container's size set to match.
    /// Every other chunk is kept byte for byte, in place.
    pub(super) fn splice(file: &[u8], container: Container, tag: Option<&[u8]>) -> Result<Vec<u8>> {
        let len = file.len() as u64;
        let found = find_id3_chunk(&mut std::io::Cursor::new(file), container, len)
            .map_err(|e| refuse(format!("The file's chunks could not be read: {e}")))?;
        let usize_of = |n: u64| usize::try_from(n).map_err(|_| refuse("The file is too large."));
        let (head_end, tail_start, id) = match found {
            Some((at, size)) => {
                let at = usize_of(at)?;
                let mut id = [0_u8; 4];
                id.copy_from_slice(&file[at..at + 4]);
                (at, usize_of(at as u64 + 8 + size + (size & 1))?.min(file.len()), id)
            }
            None => (file.len(), file.len(), *container.chunk_id()),
        };
        let mut out = Vec::with_capacity(file.len() + tag.map_or(0, <[u8]>::len) + 9);
        out.extend_from_slice(&file[..head_end]);
        if let Some(tag) = tag {
            if found.is_none() && out.len() % 2 == 1 {
                out.push(0); // chunks start on an even offset
            }
            let size = u32::try_from(tag.len()).map_err(|_| refuse("The tag is too large."))?;
            out.extend_from_slice(&id);
            out.extend_from_slice(&size_bytes(container, size));
            out.extend_from_slice(tag);
            if tag.len() % 2 == 1 {
                out.push(0);
            }
        }
        out.extend_from_slice(&file[tail_start..]);
        let total = u32::try_from(out.len() - 8).map_err(|_| refuse("The file is too large."))?;
        out[4..8].copy_from_slice(&size_bytes(container, total));
        Ok(out)
    }
}

// ------------------------------------------------------------- ID3 frames

mod id3 {
    use super::{refuse, Edit, Image};
    use crate::error::Result;
    use crate::model::Field;
    use crate::tags;

    /// Padding left after the frames when a tag has to grow, so the next
    /// edit fits in place.
    const PADDING: usize = 2048;

    /// The length of the `ID3v2` tag at the start (header, body, footer);
    /// `None` without one.
    pub(super) fn tag_len(bytes: &[u8]) -> Option<usize> {
        if !bytes.starts_with(b"ID3") || bytes.len() < 10 {
            return None;
        }
        let footer = if bytes[3] == 4 && bytes[5] & 0x10 != 0 { 10 } else { 0 };
        Some((10 + tags::syncsafe(&bytes[6..10]) + footer).min(bytes.len()))
    }

    struct Frame {
        id: [u8; 4],
        /// The frame exactly as stored (header and payload) when unchanged;
        /// rebuilt from `payload` when changed.
        raw: Option<Vec<u8>>,
        flags: [u8; 2],
        payload: Vec<u8>,
    }

    impl Frame {
        fn id(&self) -> &str {
            std::str::from_utf8(&self.id).unwrap_or("")
        }

        fn field(&self) -> Option<Field> {
            tags::frame_field(self.id(), &self.payload).map(|(field, _)| field)
        }

        /// A format flag (compression, encryption, grouping, per-frame
        /// unsynchronisation, data length): the payload is not plain.
        fn is_encoded(&self, version: u8) -> bool {
            let mask = if version == 4 { 0x4F } else { 0xE0 };
            self.flags[1] & mask != 0
        }

        fn set(&mut self, payload: Vec<u8>) {
            self.payload = payload;
            self.raw = None;
            self.flags[1] = 0;
        }

        fn new(id: &str, payload: Vec<u8>) -> Self {
            let mut bytes = [0_u8; 4];
            bytes.copy_from_slice(&id.as_bytes()[..4]);
            Self { id: bytes, raw: None, flags: [0, 0], payload }
        }

        fn bytes(&self, version: u8, out: &mut Vec<u8>) -> Result<()> {
            if let Some(raw) = &self.raw {
                out.extend_from_slice(raw);
                return Ok(());
            }
            let size = u32::try_from(self.payload.len()).map_err(|_| refuse("A tag frame is too large."))?;
            out.extend_from_slice(&self.id);
            out.extend_from_slice(&size_bytes(version, size)?);
            out.extend_from_slice(&self.flags);
            out.extend_from_slice(&self.payload);
            Ok(())
        }
    }

    fn size_bytes(version: u8, size: u32) -> Result<[u8; 4]> {
        if version == 4 {
            syncsafe(size)
        } else {
            Ok(size.to_be_bytes())
        }
    }

    fn syncsafe(size: u32) -> Result<[u8; 4]> {
        if size >= 1 << 28 {
            return Err(refuse("The tag is too large."));
        }
        let byte = |shift: u32| u8::try_from((size >> shift) & 0x7F).unwrap_or(0);
        Ok([byte(21), byte(14), byte(7), byte(0)])
    }

    /// The tag's version, flags, body size (after the header, without the
    /// footer) and frames.
    fn parse(bytes: &[u8]) -> Result<(u8, u8, usize, Vec<Frame>)> {
        let version = bytes[3];
        let flags = bytes[5];
        let size = tags::syncsafe(&bytes[6..10]);
        let body_end = 10 + size;
        if body_end > bytes.len() {
            return Err(refuse("The file's ID3 tag claims more than the file holds."));
        }
        let mut at = 10;
        if flags & 0x40 != 0 {
            let ext = bytes.get(10..14).ok_or_else(|| refuse("The file's ID3 tag is cut short."))?;
            at += if version == 4 { tags::syncsafe(ext) } else { 4 + tags::be_u32(ext) };
        }
        let mut frames = Vec::new();
        while at + 10 <= body_end {
            let header = &bytes[at..at + 10];
            if header[0] == 0 {
                break; // padding
            }
            if !header[..4].iter().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()) {
                return Err(refuse("The file's ID3 tag has a frame that cannot be read; it is not rewritten."));
            }
            let len = if version == 4 { tags::syncsafe(&header[4..8]) } else { tags::be_u32(&header[4..8]) };
            let end = at + 10 + len;
            if end > body_end {
                return Err(refuse("The file's ID3 tag has a frame that runs past its end; it is not rewritten."));
            }
            let mut id = [0_u8; 4];
            id.copy_from_slice(&header[..4]);
            frames.push(Frame {
                id,
                raw: Some(bytes[at..end].to_vec()),
                flags: [header[8], header[9]],
                payload: bytes[at + 10..end].to_vec(),
            });
            at = end;
        }
        Ok((version, flags, size, frames))
    }

    /// The file with `edits` made to its `ID3v2` tag (one is added to a file
    /// without), everything else byte for byte.
    pub(super) fn rewrite(file: &[u8], edits: &[Edit], artwork: Option<Image<'_>>) -> Result<Vec<u8>> {
        let audio_at = tag_len(file).unwrap_or(0);
        let mut out = rewrite_tag((audio_at > 0).then(|| &file[..audio_at]), edits, artwork)?;
        out.extend_from_slice(&file[audio_at..]);
        Ok(out)
    }

    /// A whole `ID3v2` tag (header, frames, padding, footer) with `edits`
    /// made to `tag`, or a new `ID3v2.3` tag when there is none. Bytes after
    /// the tag's own end (an AIFF chunk's slack) are kept as padding.
    pub(super) fn rewrite_tag(tag: Option<&[u8]>, edits: &[Edit], artwork: Option<Image<'_>>) -> Result<Vec<u8>> {
        let (version, flags, old_size, mut frames, revision) = match tag {
            Some(tag) => {
                if !tag.starts_with(b"ID3") || tag.len() < 10 || !matches!(tag[3], 3 | 4) || tag[5] & 0x80 != 0 {
                    return Err(refuse("This file's ID3 tag (an old version, or unsynchronised) cannot be edited safely."));
                }
                let (version, flags, size, frames) = parse(tag)?;
                let footer = if version == 4 && flags & 0x10 != 0 { 10 } else { 0 };
                // Room the tag had: its own size, or the whole chunk.
                (version, flags, Some(size.max(tag.len().saturating_sub(10 + footer))), frames, tag[4])
            }
            None => (3, 0, None, Vec::new(), 0),
        };
        for edit in edits {
            apply(&mut frames, version, edit, artwork)?;
        }
        let mut body = Vec::new();
        for frame in &frames {
            frame.bytes(version, &mut body)?;
        }
        let footer = version == 4 && flags & 0x10 != 0;
        let size = match old_size {
            // Fits: the tag keeps its size, the audio stays where it was.
            Some(old) if !footer && body.len() <= old => old,
            _ if footer => body.len(),
            _ => body.len() + PADDING,
        };
        body.resize(size, 0);
        let size = u32::try_from(size).map_err(|_| refuse("The tag is too large."))?;
        // The extended header is dropped (its CRC would no longer match).
        let flags = flags & !0x40;
        let mut out = Vec::with_capacity(10 + body.len() + 10);
        out.extend_from_slice(b"ID3");
        out.push(version);
        out.push(revision);
        out.push(flags);
        out.extend_from_slice(&syncsafe(size)?);
        out.extend_from_slice(&body);
        if footer {
            let mut footer = *b"3DI\0\0\0\0\0\0\0";
            footer[3..].copy_from_slice(&out[3..10]);
            out.extend_from_slice(&footer);
        }
        Ok(out)
    }

    fn apply(frames: &mut Vec<Frame>, version: u8, edit: &Edit, artwork: Option<Image<'_>>) -> Result<()> {
        match edit {
            Edit::AddArtwork => {
                let image = artwork.ok_or_else(|| refuse("The proposed artwork is not in the identify store."))?;
                frames.push(Frame::new("APIC", apic(image)));
                Ok(())
            }
            Edit::Clear(field) => {
                frames.retain(|f| f.field() != Some(*field));
                Ok(())
            }
            Edit::Set(Field::SubGenre, text) => set_comment(frames, version, text),
            Edit::Set(field, value) => {
                let mut found = false;
                for frame in frames.iter_mut().filter(|f| f.field() == Some(*field)) {
                    if frame.is_encoded(version) {
                        return Err(refuse(format!(
                            "The frame holding {} is compressed or encrypted; it is not rewritten.",
                            field.as_str()
                        )));
                    }
                    let payload = updated(frame, version, value)?;
                    frame.set(payload);
                    found = true;
                }
                if !found {
                    frames.push(new_frame(*field, version, value)?);
                }
                Ok(())
            }
        }
    }

    /// The comment: the first description-less `COMM` the reader takes, else
    /// an empty description-less one, else a new one.
    fn set_comment(frames: &mut Vec<Frame>, version: u8, text: &str) -> Result<()> {
        let index = frames.iter().position(|f| f.field() == Some(Field::SubGenre)).or_else(|| {
            frames.iter().position(|f| {
                f.id() == "COMM" && comm_parts(&f.payload).is_some_and(|(_, _, description, _)| description.is_empty())
            })
        });
        let Some(i) = index else {
            frames.push(Frame::new("COMM", comm(choose(version, None, text), *b"eng", text)));
            return Ok(());
        };
        let frame = &mut frames[i];
        if frame.is_encoded(version) {
            return Err(refuse("The comment frame is compressed or encrypted; it is not rewritten."));
        }
        let (encoding, lang, _, _) = comm_parts(&frame.payload).ok_or_else(|| refuse("The comment frame cannot be read."))?;
        let encoding = choose(version, Some(encoding), text);
        frame.set(comm(encoding, lang, text));
        Ok(())
    }

    /// A `COMM` payload's encoding, language, description and text.
    fn comm_parts(payload: &[u8]) -> Option<(u8, [u8; 3], String, String)> {
        let (&encoding, rest) = payload.split_first()?;
        let mut lang = [0_u8; 3];
        lang.copy_from_slice(rest.get(..3)?);
        let mut text = vec![encoding];
        text.extend_from_slice(rest.get(3..)?);
        let (description, value) = tags::split_once_text(&text)?;
        Some((encoding, lang, description, value))
    }

    /// The payload of an existing frame holding the field, with `value`.
    fn updated(frame: &Frame, version: u8, value: &str) -> Result<Vec<u8>> {
        let encoding = frame.payload.first().copied();
        match frame.id() {
            "TXXX" => {
                let (description, _) = tags::split_once_text(&frame.payload)
                    .ok_or_else(|| refuse("A user-defined frame cannot be read; it is not rewritten."))?;
                let encoding = choose(version, encoding, &format!("{description}{value}"));
                Ok(txxx(encoding, &description, value))
            }
            "UFID" => {
                let end = frame.payload.iter().position(|&b| b == 0).ok_or_else(|| refuse("A UFID frame cannot be read."))?;
                let mut payload = frame.payload[..=end].to_vec();
                payload.extend_from_slice(value.as_bytes());
                Ok(payload)
            }
            _ => Ok(text(choose(version, encoding, value), value)),
        }
    }

    /// The standard frame for a field the tag does not hold yet.
    fn new_frame(field: Field, version: u8, value: &str) -> Result<Frame> {
        let plain = |id: &str| Frame::new(id, text(choose(version, None, value), value));
        let user = |description: &str| {
            Frame::new("TXXX", txxx(choose(version, None, &format!("{description}{value}")), description, value))
        };
        Ok(match field {
            Field::Title => plain("TIT2"),
            Field::Artist => plain("TPE1"),
            Field::Album => plain("TALB"),
            Field::Label => plain("TPUB"),
            Field::Year => plain(if version == 4 { "TDRC" } else { "TYER" }),
            Field::Genre => plain("TCON"),
            Field::Isrc => plain("TSRC"),
            Field::CatalogNumber => user("CATALOGNUMBER"),
            Field::MbRecordingId => {
                let mut payload = b"http://musicbrainz.org\0".to_vec();
                payload.extend_from_slice(value.as_bytes());
                Frame::new("UFID", payload)
            }
            Field::MbReleaseId => user("MusicBrainz Album Id"),
            Field::MbReleaseGroupId => user("MusicBrainz Release Group Id"),
            Field::MbArtistId => user("MusicBrainz Artist Id"),
            Field::MbReleaseTrackId => user("MusicBrainz Release Track Id"),
            Field::MbAlbumArtistId => user("MusicBrainz Album Artist Id"),
            Field::DiscogsReleaseId => user("DISCOGS_RELEASE_ID"),
            Field::SubGenre | Field::Artwork => return Err(refuse("That field is not a text frame.")),
        })
    }

    /// The encoding to write `text` in: the frame's own where it can hold
    /// the text, else UTF-8 in `ID3v2.4` and Latin-1 or UTF-16 in `ID3v2.3`.
    fn choose(version: u8, existing: Option<u8>, text: &str) -> u8 {
        let latin1 = text.chars().all(|c| u32::from(c) <= 0xFF);
        match existing {
            Some(0) if latin1 => 0,
            Some(1) => 1,
            Some(2 | 3) if version == 4 => existing.unwrap_or(3),
            _ if version == 4 => 3,
            _ if latin1 => 0,
            _ => 1,
        }
    }

    fn encode(encoding: u8, text: &str) -> Vec<u8> {
        match encoding {
            0 => text.chars().map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?')).collect(),
            1 => [0xFF, 0xFE].into_iter().chain(text.encode_utf16().flat_map(u16::to_le_bytes)).collect(),
            2 => text.encode_utf16().flat_map(u16::to_be_bytes).collect(),
            _ => text.as_bytes().to_vec(),
        }
    }

    fn terminator(encoding: u8) -> &'static [u8] {
        if matches!(encoding, 1 | 2) {
            &[0, 0]
        } else {
            &[0]
        }
    }

    fn text(encoding: u8, value: &str) -> Vec<u8> {
        let mut payload = vec![encoding];
        payload.extend(encode(encoding, value));
        payload
    }

    fn txxx(encoding: u8, description: &str, value: &str) -> Vec<u8> {
        let mut payload = vec![encoding];
        payload.extend(encode(encoding, description));
        payload.extend_from_slice(terminator(encoding));
        payload.extend(encode(encoding, value));
        payload
    }

    fn comm(encoding: u8, lang: [u8; 3], value: &str) -> Vec<u8> {
        let mut payload = vec![encoding];
        payload.extend_from_slice(&lang);
        payload.extend(encode(encoding, ""));
        payload.extend_from_slice(terminator(encoding));
        payload.extend(encode(encoding, value));
        payload
    }

    fn apic(image: Image<'_>) -> Vec<u8> {
        let mut payload = vec![0];
        payload.extend_from_slice(image.mime.as_bytes());
        payload.push(0);
        payload.push(3); // front cover
        payload.push(0); // no description
        payload.extend_from_slice(image.bytes);
        payload
    }
}

// --------------------------------------------------------- with the store

/// What [`apply_result`] did.
#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    pub application: ApplicationId,
    /// The file's tags after the write.
    pub after: TagSnapshot,
}

/// Writes the ticked `fields` of a result to its file, as one recorded
/// application. The file only: identification never writes to the
/// rekordbox library.
///
/// 1. the result must be its track's current one, `Pending` or `Accepted`
///    (a pending one is accepted by this call), and every field must be in
///    its proposal;
/// 2. the file's tags are read and the changes planned against them
///    ([`plan`]: refused if the file changed since it was identified);
/// 3. [`Store::begin_application`] records the plan and the file's tags
///    now BEFORE the file is touched;
/// 4. the file is written ([`write`]);
/// 5. [`Store::complete_application_with`] records the outcome and the
///    file's tags after the write.
///
/// On a failed write the application is abandoned when the file is exactly
/// as it was (left open, for reconciliation, otherwise), and a result this
/// call accepted goes back to `Pending`, so a later bulk apply cannot write
/// fields the user did not tick.
///
/// A file another program holds open (it cannot be read, or the rename over
/// it is refused) is not a failure: the write is kept as a waiting write
/// ([`Store::defer_write`]: the result `Accepted`, these `fields`) and the
/// call answers [`IdentifyError::Deferred`]. [`retry_waiting`] makes it once
/// the file is free, through this same path and its checks. A write that
/// lands ends the result's waiting write. A result that already has a
/// waiting write is refused here: only its retry may make it.
pub fn apply_result(store: &Store, result: ResultId, fields: &[Field], allowed: FileWritesAllowed) -> Result<Applied> {
    if store.waiting_write(result)?.is_some() {
        return Err(refuse(
            "This result is already waiting to be written once the file is closed. Retry it, or cancel the waiting write.",
        ));
    }
    apply(store, result, fields, false, Deadline::NONE, allowed)
}

/// [`apply_result`]'s path. `retrying`: the call is the retry of the
/// result's own waiting write (checked by [`retry_waiting`]), which may
/// stop for `deadline` and then keeps waiting.
fn apply(
    store: &Store,
    result: ResultId,
    fields: &[Field],
    retrying: bool,
    deadline: Deadline<'_>,
    allowed: FileWritesAllowed,
) -> Result<Applied> {
    let (item, ..) = store.result(result)?.ok_or_else(|| refuse("That identification result is no longer in the store."))?;
    let accepted_here = match item.state {
        ReviewState::Accepted => false,
        ReviewState::Pending => true,
        other => return Err(refuse(format!("A {} result cannot be written.", other.as_str()))),
    };
    if fields.is_empty() {
        return Err(refuse("Choose at least one field to write."));
    }
    let proposal = item.proposal.as_ref().ok_or_else(|| refuse("The result proposes no changes."))?;
    let mut selected = Vec::with_capacity(fields.len());
    for field in fields {
        let change = proposal
            .changes
            .iter()
            .find(|c| c.field == *field)
            .ok_or_else(|| refuse(format!("The proposal has no change to {}.", field.as_str())))?;
        if !selected.contains(change) {
            selected.push(change.clone());
        }
    }
    let path = Path::new(&item.path);
    let defer = |store: &Store| -> IdentifyError {
        match store.defer_write(result, fields, OPEN_ELSEWHERE) {
            Ok(_) => deferred(path),
            Err(e) => e,
        }
    };
    let info = match tags::read(path).map_err(|e| in_use_or(path, e)) {
        Ok(info) => info,
        Err(IdentifyError::InUse(_)) => return Err(defer(store)),
        Err(e) => return Err(e),
    };
    let edits = plan(&info.snapshot, &selected)?;
    if edits.is_empty() {
        return Err(refuse("The file already holds these changes."));
    }
    // What is recorded is exactly what is written: the sub-genre as the
    // comment it becomes, a dropped no-op change not at all.
    let written: Vec<FieldChange> = edits
        .iter()
        .filter_map(|edit| {
            let change = selected.iter().find(|c| c.field == edit.field())?;
            Some(match edit {
                Edit::Set(Field::SubGenre, text) => FieldChange {
                    field: Field::SubGenre,
                    current: current(&info.snapshot, Field::SubGenre),
                    proposed: text.clone(),
                },
                _ => change.clone(),
            })
        })
        .collect();
    let art_ref: Option<&ArtworkRef> = proposal.artwork.as_ref().filter(|_| edits.contains(&Edit::AddArtwork));
    let image = art_ref.map(|a| store.artwork(&a.sha256)).transpose()?.flatten();
    if art_ref.is_some() && image.is_none() {
        return Err(refuse("The proposed artwork is not in the identify store."));
    }
    if accepted_here {
        store.set_state(result, ReviewState::Accepted)?;
    }
    let back_to_pending = |store: &Store| {
        if accepted_here {
            let _ = store.set_state(result, ReviewState::Pending);
        }
    };
    let application = match store.begin_application(result, &written, art_ref, &info.snapshot, None) {
        Ok(id) => id,
        Err(e) => {
            back_to_pending(store);
            return Err(e);
        }
    };
    let image = image.as_ref().map(|(bytes, mime)| Image { bytes, mime });
    let after = match write_until(path, &info.snapshot, &edits, image, deadline, allowed) {
        Ok(after) => after,
        Err(e) => {
            let e = in_use_or(path, e);
            // Abandoned only when the file is exactly as it was: a write that
            // landed (or half did) stays open for reconciliation, never
            // recorded as "not written".
            if tags::read(path).is_ok_and(|now| now.snapshot == info.snapshot) {
                if store.abandon_application(application, &format!("the write failed: {e}")).is_ok() {
                    // Open elsewhere: kept, the result accepted, to be
                    // written once the file is free.
                    if matches!(e, IdentifyError::InUse(_)) {
                        return Err(defer(store));
                    }
                    // Out of time: nothing changed, and it keeps waiting.
                    if retrying && matches!(e, IdentifyError::OutOfTime(_)) {
                        return Err(e);
                    }
                    back_to_pending(store);
                }
                return Err(e);
            }
            return Err(refuse(format!(
                "{e} The file may hold part of the changes; the write is left open to be checked."
            )));
        }
    };
    let key = TrackKey::for_file(path)?;
    let file_after = TrackKey { path: item.path.clone(), ..key };
    store.complete_application_with(application, &file_after, Some(&after), None)?;
    Ok(Applied { application, after })
}

/// How one try of a waiting write went ([`retry_waiting`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Retried {
    /// Written: the result is applied (or auto-applied) and undoable.
    Written(Applied),
    /// Not tried or not written, and still waiting: the file is still open
    /// in another program (or not there, or another write to it is being
    /// settled). Says why.
    StillWaiting(String),
    /// Refused for good (the file's tags are no longer the ones the review
    /// saw, or it cannot be written at all): the waiting write is dropped
    /// and the result is back in review with the warning, which says why.
    /// `returned` is whether the result's state changed.
    Dropped { warning: String, returned: bool },
    /// It is no longer the result's waiting write (cancelled, replaced, or
    /// already made by another pass): nothing was tried.
    NoLongerWaiting,
}

/// Tries a waiting write again, through exactly [`apply_result`]'s path and
/// checks: the file must still hold, in every field to write, the value the
/// review saw. `waiting` is only which one to try: it is read again here,
/// and tried only while it is still the result's waiting write, with the
/// fields it holds now ([`Retried::NoLongerWaiting`] otherwise). Stops,
/// still waiting, once `deadline` passes, before the file is changed. A
/// store failure is the only error; the waiting write is then kept.
pub fn retry_waiting(
    store: &Store,
    waiting: &WaitingWrite,
    deadline: Deadline<'_>,
    allowed: FileWritesAllowed,
) -> Result<Retried> {
    let Some(waiting) = store.waiting_write(waiting.result)?.filter(|w| w.id == waiting.id) else {
        return Ok(Retried::NoLongerWaiting);
    };
    if deadline.passed() {
        return Ok(Retried::StillWaiting(out_of_time().to_string()));
    }
    let path = Path::new(&waiting.path);
    if !path.exists() {
        return Ok(Retried::StillWaiting("The file is not there now.".to_owned()));
    }
    if store.open_applications()?.iter().any(|a| a.path == waiting.path) {
        return Ok(Retried::StillWaiting("An earlier write to this file is still being settled.".to_owned()));
    }
    match apply(store, waiting.result, &waiting.fields, true, deadline, allowed) {
        Ok(applied) => Ok(Retried::Written(applied)),
        Err(IdentifyError::Deferred(message) | IdentifyError::OutOfTime(message)) => Ok(Retried::StillWaiting(message)),
        Err(e @ IdentifyError::Store(_)) => Err(e),
        Err(e) => {
            let warning = format!("A write that was waiting for this file to be closed was not made: {e}");
            let returned = store.drop_waiting(waiting.result, &warning)?;
            Ok(Retried::Dropped { warning, returned })
        }
    }
}

/// What [`undo_result`] restored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Undone {
    pub application: ApplicationId,
}

/// Undoes a result's write: its track's latest written application. The
/// file's tags are put back exactly ([`restore`]: refused if the file has
/// changed since the write), then the undo is recorded
/// ([`Store::record_undo`]). The file only: a library side recorded by an
/// older build (which also wrote the rekordbox library) is ignored.
pub fn undo_result(store: &Store, result: ResultId, allowed: FileWritesAllowed) -> Result<Undone> {
    let (item, ..) = store.result(result)?.ok_or_else(|| refuse("That identification result is no longer in the store."))?;
    if !item.state.is_applied() {
        return Err(refuse("Only an applied result can be undone."));
    }
    let track = store.track_id(&item.path)?.ok_or_else(|| refuse("The result's track is not in the store."))?;
    let application = store
        .latest_written_application(track)?
        .filter(|a| a.result == result && a.undone_unix.is_none())
        .ok_or_else(|| refuse("This result's write is not the file's latest; only that one can be undone."))?;
    let before = store.application_snapshot(application.id)?.ok_or_else(|| refuse("No tags were recorded before the write."))?;
    let after = store
        .application_after_snapshot(application.id)?
        .ok_or_else(|| refuse("The tags after this write were not recorded, so it cannot be checked and undone."))?;
    let path = Path::new(&item.path);
    let fields: Vec<Field> = application.changes.iter().map(|c| c.field).collect();
    restore(path, &before, &after, &fields, allowed)?;
    store.record_undo(application.id)?;
    Ok(Undone { application: application.id })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// These tests write their own temporary files.
    const ALLOW: FileWritesAllowed = FileWritesAllowed { _granted: () };

    #[test]
    fn only_sharing_and_lock_violations_read_as_open_elsewhere() {
        let text = |e: std::io::Error, held: bool| replace_error(&e, held).to_string();
        for code in [SHARING_VIOLATION, LOCK_VIOLATION] {
            assert!(text(std::io::Error::from_raw_os_error(code), false).contains("open in another program"), "{code}");
        }
        let denied = || std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let plain = text(denied(), false);
        assert!(plain.contains("access was denied") && !plain.contains("open in another program"), "{plain}");
        assert!(text(denied(), true).contains("open in another program"), "denied because a player holds it");
        assert!(text(std::io::Error::other("disk full"), true).contains("disk full"));
    }

    #[test]
    fn a_file_nobody_holds_is_not_reported_held() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.mp3");
        std::fs::write(&file, b"x").unwrap();
        assert!(!held_open(&file));
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            let player = std::fs::OpenOptions::new().read(true).share_mode(1).open(&file).unwrap();
            assert!(held_open(&file));
            drop(player);
        }
    }

    #[test]
    fn only_our_exact_temp_names_match() {
        for name in [".rbxport-tags-a1B2c3.tmp", ".rbxport-tags-ZZZZZZ.mp3"] {
            assert!(is_our_temp(name, Some("mp3")), "{name}");
        }
        for name in [
            ".rbxport-tags-a1B2c.tmp",
            ".rbxport-tags-a1B2c34.tmp",
            ".rbxport-tags-a1-2c3.tmp",
            ".rbxport-tags-a1B2c3.tmp.bak",
            ".rbxport-tags-a1B2c3.flac",
            ".rbxport-tags-a1B2c3.",
            "rbxport-tags-a1B2c3.tmp",
            ".rbxport-tags-.tmp",
            "track.mp3",
            ".rbxport-tags-a1B2\u{e9}3.tmp",
        ] {
            assert!(!is_our_temp(name, Some("mp3")), "{name}");
        }
        assert!(!is_our_temp(".rbxport-tags-a1B2c3.", None));
    }

    #[test]
    fn stale_temps_beside_a_track_are_removed_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let track = dir.path().join("track.mp3");
        std::fs::write(&track, b"audio").unwrap();
        let ours = [".rbxport-tags-abc123.tmp", ".rbxport-tags-XyZ789.mp3"];
        let others = [".rbxport-tags-abc12.tmp", ".rbxport-tags-abc123.flac", "notes.tmp", ".rbxport-tags-abc123.tmp.keep"];
        for name in ours.iter().chain(&others) {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        // A folder with our name is never touched.
        std::fs::create_dir(dir.path().join(".rbxport-tags-dir456.tmp")).unwrap();
        assert_eq!(remove_stale_temps(&track), 2);
        for name in ours {
            assert!(!dir.path().join(name).exists(), "{name}");
        }
        for name in others {
            assert!(dir.path().join(name).exists(), "{name}");
        }
        assert!(dir.path().join(".rbxport-tags-dir456.tmp").is_dir());
        assert_eq!(std::fs::read(&track).unwrap(), b"audio");
        // Nothing left to remove; a file in no folder is a no-op.
        assert_eq!(remove_stale_temps(&track), 0);
        assert_eq!(remove_stale_temps(Path::new("track.mp3")), 0);
    }

    // ------------------------------------------------- waiting writes

    use crate::model::{Decision, ExistingTags, Proposal};
    use crate::store::{Filter, ResultId};
    use std::time::Duration;

    /// `secs` seconds of silent 16-bit mono WAV.
    fn wav(path: &Path, secs: u32) {
        let rate = 44_100_u32;
        let data_len = rate * 2 * secs;
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16_u32.to_le_bytes());
        out.extend_from_slice(&1_u16.to_le_bytes());
        out.extend_from_slice(&1_u16.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2).to_le_bytes());
        out.extend_from_slice(&2_u16.to_le_bytes());
        out.extend_from_slice(&16_u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        out.resize(out.len() + data_len as usize, 0);
        std::fs::write(path, out).unwrap();
    }

    /// A pending result proposing a title and a catalog number for `audio`.
    fn reviewed(store: &Store, audio: &Path) -> ResultId {
        let key = TrackKey::for_file(audio).unwrap();
        let track = store.upsert_track(&key, 1.0, None, &ExistingTags::default()).unwrap();
        let changes = vec![
            FieldChange { field: Field::Title, current: String::new(), proposed: "Mine".into() },
            FieldChange { field: Field::CatalogNumber, current: String::new(), proposed: "CAT001".into() },
        ];
        store.save_result(track, Decision::Review, &[], Some(&Proposal::new(changes, None, true)), None, &[], false).unwrap()
    }

    fn staged_beside(audio: &Path) -> Vec<String> {
        std::fs::read_dir(audio.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(TEMP_PREFIX))
            .collect()
    }

    #[test]
    fn a_waiting_result_is_refused_by_a_plain_apply() {
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("a.wav");
        wav(&audio, 1);
        let store = Store::open_in_memory().unwrap();
        let result = reviewed(&store, &audio);
        store.defer_write(result, &[Field::Title], OPEN_ELSEWHERE).unwrap();
        let original = std::fs::read(&audio).unwrap();
        let error = apply_result(&store, result, &[Field::CatalogNumber], ALLOW).unwrap_err().to_string();
        assert!(error.contains("already waiting"), "{error}");
        assert_eq!(std::fs::read(&audio).unwrap(), original);
        // Its retry is the one way to make it.
        let waiting = store.waiting_write(result).unwrap().unwrap();
        assert!(matches!(retry_waiting(&store, &waiting, Deadline::NONE, ALLOW).unwrap(), Retried::Written(_)));
        assert_eq!(current(&tags::read(&audio).unwrap().snapshot, Field::CatalogNumber), "", "only the waiting fields");
    }

    /// A pass that read the list before the user cancelled (and accepted
    /// again with fewer fields) never writes the stale one: it is skipped
    /// quietly, and the current one is written with its own fields.
    #[test]
    fn a_stale_waiting_write_is_skipped_after_a_cancel_and_a_new_accept() {
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("a.wav");
        wav(&audio, 1);
        let original = std::fs::read(&audio).unwrap();
        let store = Store::open_in_memory().unwrap();
        let result = reviewed(&store, &audio);
        store.defer_write(result, &[Field::Title, Field::CatalogNumber], OPEN_ELSEWHERE).unwrap();
        let stale = store.waiting_writes().unwrap().remove(0);

        store.cancel_waiting(result).unwrap();
        assert_eq!(retry_waiting(&store, &stale, Deadline::NONE, ALLOW).unwrap(), Retried::NoLongerWaiting);
        assert_eq!(std::fs::read(&audio).unwrap(), original, "a cancelled write is never made");
        assert_eq!(store.result(result).unwrap().unwrap().0.state, ReviewState::Pending);

        // Accepted again with only the catalog number, the file still held.
        store.defer_write(result, &[Field::CatalogNumber], OPEN_ELSEWHERE).unwrap();
        assert_eq!(retry_waiting(&store, &stale, Deadline::NONE, ALLOW).unwrap(), Retried::NoLongerWaiting);
        assert_eq!(std::fs::read(&audio).unwrap(), original);
        let fresh = store.waiting_write(result).unwrap().unwrap();
        assert_eq!(fresh.fields, [Field::CatalogNumber]);
        assert!(matches!(retry_waiting(&store, &fresh, Deadline::NONE, ALLOW).unwrap(), Retried::Written(_)));
        let now = tags::read(&audio).unwrap().snapshot;
        assert_eq!((current(&now, Field::CatalogNumber).as_str(), current(&now, Field::Title).as_str()), ("CAT001", ""));
        // A second pass holding the same (now made) write skips it quietly.
        assert_eq!(retry_waiting(&store, &fresh, Deadline::NONE, ALLOW).unwrap(), Retried::NoLongerWaiting);
        assert_eq!(store.count(Filter::Waiting).unwrap(), 0);
    }

    /// A clock that moves one second each time it is read.
    #[derive(Default)]
    struct Ticking(std::sync::Mutex<Duration>);

    impl crate::clock::Clock for Ticking {
        fn now(&self) -> Duration {
            let mut now = self.0.lock().unwrap();
            *now += Duration::from_secs(1);
            *now
        }

        fn sleep(&self, duration: Duration) {
            *self.0.lock().unwrap() += duration;
        }
    }

    /// However far into the read or the staged copy the deadline passes, the
    /// write stops cleanly: the file is unchanged, no staged file is left,
    /// no write is left open, and it keeps waiting. With time enough, it is
    /// made.
    #[test]
    #[allow(clippy::panic)]
    fn a_retry_out_of_time_stops_cleanly_at_any_point_and_keeps_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("big.wav");
        // About 5 MiB: several chunks to read and to stage.
        wav(&audio, 60);
        let original = std::fs::read(&audio).unwrap();
        let store = Store::open_in_memory().unwrap();
        let result = reviewed(&store, &audio);
        store.defer_write(result, &[Field::Title], OPEN_ELSEWHERE).unwrap();
        let mut stopped = 0;
        for budget in 0..200 {
            let clock = Ticking::default();
            let deadline = Deadline::new(&clock, Duration::from_secs(budget));
            let waiting = store.waiting_write(result).unwrap().unwrap();
            match retry_waiting(&store, &waiting, deadline, ALLOW).unwrap() {
                Retried::StillWaiting(why) => {
                    stopped += 1;
                    assert!(why.contains("no time left"), "{why}");
                    assert_eq!(std::fs::read(&audio).unwrap(), original, "budget {budget}");
                    assert_eq!(staged_beside(&audio), Vec::<String>::new(), "budget {budget}");
                    assert_eq!(store.open_applications().unwrap(), [], "budget {budget}");
                    let (item, ..) = store.result(result).unwrap().unwrap();
                    assert_eq!(item.state, ReviewState::Accepted, "budget {budget}");
                    assert!(item.waiting.is_some(), "budget {budget}");
                }
                Retried::Written(_) => {
                    assert!(stopped > 5, "it stopped part way several times first ({stopped})");
                    assert_eq!(current(&tags::read(&audio).unwrap().snapshot, Field::Title), "Mine");
                    assert_eq!(store.waiting_write(result).unwrap(), None);
                    return;
                }
                other @ (Retried::Dropped { .. } | Retried::NoLongerWaiting) => panic!("budget {budget}: {other:?}"),
            }
        }
        panic!("never written");
    }
}
