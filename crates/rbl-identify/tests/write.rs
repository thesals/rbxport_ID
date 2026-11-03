//! The tag writer on real files: temporary copies of `C:\dev\id-testset`
//! MP3s (skipped where the test set is absent, as in CI) and of the FLAC
//! fixture. The originals are only ever read.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use rbl_identify::model::{Decision, Field, FieldChange, Proposal};
use rbl_identify::store::{Filter, ReviewState, Store, TrackKey};
use rbl_identify::write::{self, Edit};
use rbl_identify::{tags, IdentifyError};

const TEST_SET: &str = r"C:\dev\id-testset";

/// A temporary copy of a test-set file, or `None` without the test set.
fn copy(dir: &Path, name: &str) -> Option<PathBuf> {
    let source = Path::new(TEST_SET).join(name);
    if !source.is_file() {
        return None;
    }
    let target = dir.join(name);
    std::fs::copy(&source, &target).unwrap();
    Some(target)
}

/// `(id, raw frame bytes)` of every frame in an `ID3v2` tag, in order.
fn frames(file: &[u8]) -> (u8, Vec<(String, Vec<u8>)>) {
    assert_eq!(&file[..3], b"ID3");
    let version = file[3];
    let syncsafe = |b: &[u8]| b.iter().fold(0_usize, |acc, &x| (acc << 7) | usize::from(x & 0x7f));
    let end = 10 + syncsafe(&file[6..10]);
    let mut at = 10;
    let mut out = Vec::new();
    while at + 10 <= end && file[at] != 0 {
        let len = if version == 4 {
            syncsafe(&file[at + 4..at + 8])
        } else {
            file[at + 4..at + 8].iter().fold(0_usize, |acc, &x| (acc << 8) | usize::from(x))
        };
        out.push((String::from_utf8_lossy(&file[at..at + 4]).into_owned(), file[at..at + 10 + len].to_vec()));
        at += 10 + len;
    }
    (version, out)
}

fn tag_len(file: &[u8]) -> usize {
    10 + file[6..10].iter().fold(0_usize, |acc, &x| (acc << 7) | usize::from(x & 0x7f))
}

fn change(field: Field, current: &str, proposed: &str) -> FieldChange {
    FieldChange { field, current: current.into(), proposed: proposed.into() }
}

#[test]
fn an_id3v23_write_keeps_the_version_every_other_frame_and_the_audio() {
    let dir = tempfile::tempdir().unwrap();
    let Some(path) = copy(dir.path(), "24 Last Ones Standing - TC Remix.mp3") else { return };
    let original = std::fs::read(&path).unwrap();
    let before = tags::read(&path).unwrap().snapshot;
    let comment = rbl_identify::pipeline::current(&before, Field::SubGenre);
    assert_eq!(comment, "Ministry of Sound UK", "the description-less COMM, not ID3v1 Comment or iTunNORM");
    let catalog = rbl_identify::pipeline::current(&before, Field::CatalogNumber);

    let changes = [
        change(Field::CatalogNumber, &catalog, "ANCD2K10X"),
        change(Field::SubGenre, &comment, &format!("{comment}; Tech House")),
        change(Field::MbRecordingId, "", "41f4b491-cc31-40f1-8d81-cc58b7d8f761"),
        change(Field::DiscogsReleaseId, "", "2509031"),
    ];
    let edits = write::plan(&before, &changes).unwrap();
    let after = write::write(&path, &before, &edits, None).unwrap();
    let written = std::fs::read(&path).unwrap();

    // The version is kept and the audio and ID3v1 trailer are untouched.
    assert_eq!(written[3], original[3]);
    assert_eq!(written[3], 3);
    assert_eq!(&written[tag_len(&written)..], &original[tag_len(&original)..]);
    assert_eq!(written.len(), original.len(), "the edits fit in the padding, so the tag kept its size");

    // Every frame not changed is there byte for byte, in order.
    let (_, old) = frames(&original);
    let (_, new) = frames(&written);
    let catalog_frame = |f: &(String, Vec<u8>)| f.0 == "TXXX" && String::from_utf8_lossy(&f.1).contains("Catalog #");
    let comment_frame = |f: &(String, Vec<u8>)| f.0 == "COMM" && f.1[14..].starts_with(b"\0Ministry");
    let kept: Vec<_> = old.iter().filter(|f| !catalog_frame(f) && !comment_frame(f)).collect();
    let unchanged: Vec<_> = new.iter().filter(|f| old.contains(f)).collect();
    assert_eq!(unchanged, kept);
    // The catalog variant was updated in place, never duplicated.
    assert_eq!(new.iter().filter(|f| catalog_frame(f)).count(), 1);
    assert!(!new.iter().any(|f| String::from_utf8_lossy(&f.1).contains("CATALOGNUMBER")));
    let index = |frames: &[(String, Vec<u8>)]| frames.iter().position(&catalog_frame).unwrap();
    assert_eq!(index(&new), index(&old), "in place");
    // Picard's and Discogs's names for new frames.
    assert!(new.iter().any(|f| f.0 == "UFID" && f.1.windows(22).any(|w| w == b"http://musicbrainz.org")));
    assert!(new.iter().any(|f| f.0 == "TXXX" && String::from_utf8_lossy(&f.1).contains("DISCOGS_RELEASE_ID")));

    let read = |field| rbl_identify::pipeline::current(&after, field);
    assert_eq!(read(Field::CatalogNumber), "ANCD2K10X");
    assert_eq!(read(Field::SubGenre), "Ministry of Sound UK; Tech House");
    assert_eq!(read(Field::MbRecordingId), "41f4b491-cc31-40f1-8d81-cc58b7d8f761");
    // The other comments are as they were.
    assert_eq!(after.raw_id3.as_ref().unwrap().v1, before.raw_id3.as_ref().unwrap().v1);
    assert_eq!(new.iter().filter(|f| f.0 == "COMM").count(), 3);

    // Undo puts the exact bytes back.
    let fields: Vec<Field> = changes.iter().map(|c| c.field).collect();
    write::restore(&path, &before, &after, &fields).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

#[test]
fn the_comment_is_appended_to_once_and_an_id3v24_tag_stays_24() {
    let dir = tempfile::tempdir().unwrap();
    let Some(path) = copy(dir.path(), "5457602_V12_Original_Mix.mp3") else { return };
    let original = std::fs::read(&path).unwrap();
    let before = tags::read(&path).unwrap().snapshot;
    let comment = rbl_identify::pipeline::current(&before, Field::SubGenre);
    assert_eq!(comment, "Purchased at Beatport.com", "read from COMM in the bogus language `Pur`");
    let sub = change(Field::SubGenre, &comment, "Purchased at Beatport.com; Minimal Techno; Acid");
    let after = write::write(&path, &before, &write::plan(&before, std::slice::from_ref(&sub)).unwrap(), None).unwrap();
    let written = std::fs::read(&path).unwrap();
    assert_eq!(written[3], 4);
    let now = rbl_identify::pipeline::current(&after, Field::SubGenre);
    assert_eq!(now, "Purchased at Beatport.com; Minimal Techno; Acid");
    // The same styles again: nothing to append.
    let again = change(Field::SubGenre, &now, "Purchased at Beatport.com; Minimal Techno; Acid");
    assert_eq!(write::plan(&after, &[again]).unwrap(), Vec::<Edit>::new());
    // A stale proposal (made against the old comment) is refused.
    assert!(write::plan(&after, &[sub]).is_err());
    // The language bytes of the COMM frame are kept.
    let (_, new) = frames(&written);
    let comm = new.iter().find(|f| f.0 == "COMM").unwrap();
    assert_eq!(&comm.1[11..14], b"Pur");
    write::restore(&path, &before, &after, &[Field::SubGenre]).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

#[test]
fn proposed_comment_text_is_never_written_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let Some(path) = copy(dir.path(), "5538816_Heart_Original_Mix.mp3") else { return };
    let before = tags::read(&path).unwrap().snapshot;
    let comment = rbl_identify::pipeline::current(&before, Field::SubGenre);
    // A proposal that drops the existing text still only appends.
    let sub = change(Field::SubGenre, &comment, "Deep House");
    let edits = write::plan(&before, &[sub]).unwrap();
    assert_eq!(edits, vec![Edit::Set(Field::SubGenre, format!("{comment}; Deep House"))]);
}

#[test]
fn undo_is_refused_once_the_file_has_changed_since_the_write() {
    let dir = tempfile::tempdir().unwrap();
    let Some(path) = copy(dir.path(), "6450109_Cannonball_Original_Mix.mp3") else { return };
    let before = tags::read(&path).unwrap().snapshot;
    let label = rbl_identify::pipeline::current(&before, Field::Label);
    let edits = write::plan(&before, &[change(Field::Label, &label, "Other Label")]).unwrap();
    let after = write::write(&path, &before, &edits, None).unwrap();
    // Someone edits the title afterwards.
    let title = rbl_identify::pipeline::current(&after, Field::Title);
    let edits = write::plan(&after, &[change(Field::Title, &title, "Edited By Hand")]).unwrap();
    let edited = write::write(&path, &after, &edits, None).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let error = write::restore(&path, &before, &after, &[Field::Label]).unwrap_err();
    assert!(matches!(error, IdentifyError::Write(_)), "{error}");
    assert!(error.to_string().contains("changed since"), "{error}");
    assert_eq!(std::fs::read(&path).unwrap(), bytes, "nothing was touched");
    assert_eq!(tags::read(&path).unwrap().snapshot, edited);
}

#[test]
fn a_read_only_file_is_refused_and_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let Some(path) = copy(dir.path(), "4236248_Oscillate_Original_Mix.mp3") else { return };
    let original = std::fs::read(&path).unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&path, permissions.clone()).unwrap();
    let before = tags::read(&path).unwrap().snapshot;
    let edits = write::plan(&before, &[change(Field::DiscogsReleaseId, "", "1")]).unwrap();
    let error = write::write(&path, &before, &edits, None).unwrap_err().to_string();
    assert!(error.contains("read-only"), "{error}");
    assert_eq!(std::fs::read(&path).unwrap(), original);
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    std::fs::set_permissions(&path, permissions).unwrap();
}

#[test]
fn a_flac_file_is_written_through_lofty_and_undone_field_by_field() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.flac");
    std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("../rbl-audio/tests/fixtures/stereo-96k.flac"), &path).unwrap();
    let before = tags::read(&path).unwrap().snapshot;
    let title = rbl_identify::pipeline::current(&before, Field::Title);
    let changes = [
        change(Field::Title, &title, "A Title (Extended Mix)"),
        change(Field::SubGenre, "", "Tech House"),
        change(Field::CatalogNumber, "", "CAT001"),
    ];
    let after = write::write(&path, &before, &write::plan(&before, &changes).unwrap(), None).unwrap();
    assert_eq!(after.format, "VorbisComments");
    assert_eq!(rbl_identify::pipeline::current(&after, Field::Title), "A Title (Extended Mix)");
    assert_eq!(rbl_identify::pipeline::current(&after, Field::SubGenre), "Tech House");
    let fields: Vec<Field> = changes.iter().map(|c| c.field).collect();
    write::restore(&path, &before, &after, &fields).unwrap();
    let back = tags::read(&path).unwrap().snapshot;
    for field in fields {
        assert_eq!(
            rbl_identify::pipeline::current(&back, field),
            rbl_identify::pipeline::current(&before, field),
            "{field:?}"
        );
    }
    // Audio still decodes.
    assert!(rbl_audio::decode_mono(&path, Some(1.0)).is_ok());
}

/// A store holding one pending result for `path` proposing `changes`.
fn pending(store: &Store, path: &Path, changes: Vec<FieldChange>) -> i64 {
    let key = TrackKey::for_file(path).unwrap();
    let track = store.upsert_track(&key, 300.0, None, &rbl_identify::model::ExistingTags::default()).unwrap();
    let proposal = Proposal::new(changes, None, true);
    store.save_result(track, Decision::Review, &[], Some(&proposal), None, &[], false).unwrap()
}

#[test]
fn an_applied_result_is_recorded_and_undone_through_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let Some(path) = copy(dir.path(), "5538816_Heart_Original_Mix.mp3") else { return };
    let original = std::fs::read(&path).unwrap();
    let store = Store::open_in_memory().unwrap();
    let before = tags::read(&path).unwrap().snapshot;
    let comment = rbl_identify::pipeline::current(&before, Field::SubGenre);
    let label = rbl_identify::pipeline::current(&before, Field::Label);
    let result = pending(
        &store,
        &path,
        vec![change(Field::Label, &label, "New Label"), change(Field::SubGenre, &comment, &format!("{comment}; Deep House"))],
    );

    // Only the ticked field is written.
    let applied = write::apply_result(&store, result, &[Field::SubGenre]).unwrap();
    assert_eq!(
        store.application(applied.application).unwrap().unwrap().changes[0].proposed,
        format!("{comment}; Deep House")
    );
    assert_eq!(rbl_identify::pipeline::current(&applied.after, Field::Label), label, "unticked");
    let item = store.result(result).unwrap().unwrap().0;
    assert_eq!(item.state, ReviewState::Applied);
    assert_eq!(store.application_snapshot(applied.application).unwrap(), Some(before.clone()));
    assert_eq!(store.application_after_snapshot(applied.application).unwrap(), Some(applied.after.clone()));
    assert_eq!(store.list(Filter::Applied, 0, 10).unwrap().len(), 1);

    write::undo_result(&store, result).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), original, "byte for byte");
    assert_eq!(store.result(result).unwrap().unwrap().0.state, ReviewState::Undone);
    assert!(write::undo_result(&store, result).is_err(), "once");
}

#[test]
fn a_refused_apply_leaves_the_file_and_the_result_as_they_were() {
    let dir = tempfile::tempdir().unwrap();
    let Some(path) = copy(dir.path(), "5538816_Heart_Original_Mix.mp3") else { return };
    let original = std::fs::read(&path).unwrap();
    let store = Store::open_in_memory().unwrap();
    let before = tags::read(&path).unwrap().snapshot;
    let label = rbl_identify::pipeline::current(&before, Field::Label);
    let result = pending(&store, &path, vec![change(Field::Label, &label, "New Label"), change(Field::Isrc, "x", "y")]);

    // A stale field (the file's ISRC is not "x") is refused before anything.
    let error = write::apply_result(&store, result, &[Field::Label, Field::Isrc]).unwrap_err();
    assert!(error.to_string().contains("changed since"), "{error}");
    assert_eq!(store.result(result).unwrap().unwrap().0.state, ReviewState::Pending);
    // A field the proposal does not list is refused.
    assert!(write::apply_result(&store, result, &[Field::Genre]).is_err());
    // A rejected result is not written.
    store.set_state(result, ReviewState::Rejected).unwrap();
    assert!(write::apply_result(&store, result, &[Field::Label]).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

/// One second of silent 16-bit mono PCM, as a WAV or an AIFF.
fn silent(path: &Path, aiff: bool) {
    let rate = 44_100_u32;
    let data_len = rate * 2;
    let mut out = Vec::new();
    if aiff {
        out.extend_from_slice(b"FORM");
        out.extend_from_slice(&(4 + 26 + 16 + data_len).to_be_bytes());
        out.extend_from_slice(b"AIFFCOMM");
        out.extend_from_slice(&18_u32.to_be_bytes());
        out.extend_from_slice(&1_u16.to_be_bytes()); // mono
        out.extend_from_slice(&rate.to_be_bytes()); // frames
        out.extend_from_slice(&16_u16.to_be_bytes());
        // 44100 as an 80-bit extended float.
        out.extend_from_slice(&[0x40, 0x0E, 0xAC, 0x44, 0, 0, 0, 0, 0, 0]);
        out.extend_from_slice(b"SSND");
        out.extend_from_slice(&(8 + data_len).to_be_bytes());
        out.extend_from_slice(&[0; 8]);
    } else {
        out.extend_from_slice(b"RIFF");
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
    }
    out.resize(out.len() + data_len as usize, 0);
    std::fs::write(path, out).unwrap();
}

#[test]
fn wav_and_aiff_files_are_written_and_undone() {
    let dir = tempfile::tempdir().unwrap();
    for (name, aiff) in [("t.wav", false), ("t.aiff", true)] {
        let path = dir.path().join(name);
        silent(&path, aiff);
        let before = tags::read(&path).unwrap().snapshot;
        let changes = [
            change(Field::Title, "", "Wave Title"),
            change(Field::Artist, "", "Someone"),
            change(Field::SubGenre, "", "Tech House"),
        ];
        let after = write::write(&path, &before, &write::plan(&before, &changes).unwrap(), None).unwrap();
        assert_eq!(rbl_identify::pipeline::current(&after, Field::Title), "Wave Title", "{name}");
        assert_eq!(rbl_identify::pipeline::current(&after, Field::SubGenre), "Tech House", "{name}");
        let fields: Vec<Field> = changes.iter().map(|c| c.field).collect();
        write::restore(&path, &before, &after, &fields).unwrap();
        let back = tags::read(&path).unwrap().snapshot;
        for field in fields {
            assert_eq!(rbl_identify::pipeline::current(&back, field), "", "{name} {field:?}");
        }
    }
}

// ------------------------------------------------- synthetic files (run in CI)

/// An `ID3v2.<version>` tag holding `frames` (id, payload) and some padding.
fn id3_tag(version: u8, frames: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let syncsafe = |n: usize| [21, 14, 7, 0].map(|shift| u8::try_from((n >> shift) & 0x7f).unwrap());
    let mut body = Vec::new();
    for (id, payload) in frames {
        body.extend_from_slice(id.as_bytes());
        if version == 4 {
            body.extend_from_slice(&syncsafe(payload.len()));
        } else {
            body.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
        }
        body.extend_from_slice(&[0, 0]);
        body.extend_from_slice(payload);
    }
    body.extend_from_slice(&[0; 256]);
    let mut tag = vec![b'I', b'D', b'3', version, 0, 0];
    tag.extend_from_slice(&syncsafe(body.len()));
    tag.extend_from_slice(&body);
    tag
}

fn latin1(text: &str) -> Vec<u8> {
    let mut payload = vec![0];
    payload.extend_from_slice(text.as_bytes());
    payload
}

fn comm_frame(lang: &str, description: &str, text: &str) -> (&'static str, Vec<u8>) {
    let mut payload = vec![0];
    payload.extend_from_slice(lang.as_bytes());
    payload.extend_from_slice(description.as_bytes());
    payload.push(0);
    payload.extend_from_slice(text.as_bytes());
    ("COMM", payload)
}

/// The frames of a Beatport-style download: a described iTunNORM and
/// `ID3v1 Comment` beside the real comment, a catalog number variant, a
/// private frame and a key and BPM identification must never touch.
fn store_frames(with_comment: bool) -> Vec<(&'static str, Vec<u8>)> {
    let mut frames = vec![
        ("TIT2", latin1("Last Ones Standing")),
        ("TPE1", latin1("Example")),
        ("TPUB", latin1("Ministry Of Sound (UK)")),
        ("TKEY", latin1("8A")),
        ("TBPM", latin1("126")),
        ("TXXX", latin1("Catalog #\0ANCD2K10E")),
        ("PRIV", b"AverageLevel\0\x01\x02\x03\x04".to_vec()),
        comm_frame("eng", "iTunNORM", " 000012A1 00001189"),
        comm_frame("eng", "ID3v1 Comment", "Purchased at Beatport"),
    ];
    if with_comment {
        frames.push(comm_frame("Pur", "", "Purchased at Beatport.com"));
    }
    frames
}

/// Silent MPEG-1 layer III frames (128 kb/s, 44.1 kHz).
fn mpeg_frames() -> Vec<u8> {
    let mut audio = Vec::new();
    for _ in 0..40 {
        audio.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        audio.extend_from_slice(&[0; 413]);
    }
    audio
}

/// `(id, raw frame)` of a tag, for the byte-for-byte comparisons.
fn tag_frames(tag: &[u8]) -> Vec<(String, Vec<u8>)> {
    frames(tag).1
}

/// Checks a write of the synthetic store frames: the version kept, every
/// frame not changed byte for byte, the catalog variant updated in place,
/// the comment appended to the description-less COMM only.
fn check_store_write(old_tag: &[u8], new_tag: &[u8], version: u8) {
    assert_eq!(new_tag[3], version);
    let old = tag_frames(old_tag);
    let new = tag_frames(new_tag);
    assert_eq!(new.len(), old.len(), "nothing added or lost");
    for (before, after) in old.iter().zip(&new) {
        assert_eq!(before.0, after.0, "same frames, same order");
        let text = String::from_utf8_lossy(&before.1);
        if text.contains("Catalog #") {
            assert!(String::from_utf8_lossy(&after.1).ends_with("Catalog #\0ANCD2K10X"));
        } else if before.0 == "COMM" && !text.contains("iTun") && !text.contains("ID3v1") {
            assert!(String::from_utf8_lossy(&after.1).ends_with("Purchased at Beatport.com; Tech House"));
            assert_eq!(&after.1[11..14], b"Pur", "the language is kept");
        } else {
            assert_eq!(before.1, after.1, "{} untouched", before.0);
        }
    }
}

fn store_changes() -> [FieldChange; 2] {
    [
        change(Field::CatalogNumber, "ANCD2K10E", "ANCD2K10X"),
        change(Field::SubGenre, "Purchased at Beatport.com", "Purchased at Beatport.com; Tech House"),
    ]
}

#[test]
fn synthetic_mp3s_keep_their_version_frames_and_audio_and_undo_exactly() {
    let dir = tempfile::tempdir().unwrap();
    for version in [3, 4] {
        let path = dir.path().join(format!("v2{version}.mp3"));
        let tag = id3_tag(version, &store_frames(true));
        let mut file = tag.clone();
        file.extend_from_slice(&mpeg_frames());
        let mut v1 = b"TAG".to_vec();
        v1.resize(128, b' ');
        file.extend_from_slice(&v1);
        std::fs::write(&path, &file).unwrap();

        let before = tags::read(&path).unwrap().snapshot;
        assert_eq!(rbl_identify::pipeline::current(&before, Field::SubGenre), "Purchased at Beatport.com");
        let changes = store_changes();
        let after = write::write(&path, &before, &write::plan(&before, &changes).unwrap(), None).unwrap();
        let written = std::fs::read(&path).unwrap();
        assert_eq!(written.len(), file.len(), "fits in the padding");
        check_store_write(&tag, &written[..tag.len()], version);
        assert_eq!(&written[tag.len()..], &file[tag.len()..], "audio and ID3v1 untouched");

        write::restore(&path, &before, &after, &[Field::CatalogNumber, Field::SubGenre]).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), file, "v2.{version} undo is byte for byte");
    }
}

#[test]
fn an_mp3_without_a_tag_gets_one_and_loses_it_on_undo() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bare.mp3");
    let file = mpeg_frames();
    std::fs::write(&path, &file).unwrap();
    let before = tags::read(&path).unwrap().snapshot;
    let changes = [change(Field::Title, "", "Bare"), change(Field::SubGenre, "", "Acid")];
    let after = write::write(&path, &before, &write::plan(&before, &changes).unwrap(), None).unwrap();
    let written = std::fs::read(&path).unwrap();
    assert_eq!(written[3], 3, "a new tag is ID3v2.3");
    assert!(written.ends_with(&file));
    assert_eq!(rbl_identify::pipeline::current(&after, Field::SubGenre), "Acid");
    write::restore(&path, &before, &after, &[Field::Title, Field::SubGenre]).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), file);
}

/// A one-second silent AIFF or WAV, its ID3 chunk holding `tag` when given.
fn container(aiff: bool, tag: Option<&[u8]>) -> Vec<u8> {
    let rate = 44_100_u32;
    let data_len = rate * 2;
    let mut out = Vec::new();
    let be = |n: u32| n.to_be_bytes();
    let le = |n: u32| n.to_le_bytes();
    if aiff {
        out.extend_from_slice(b"FORM\0\0\0\0AIFFCOMM");
        out.extend_from_slice(&be(18));
        out.extend_from_slice(&1_u16.to_be_bytes());
        out.extend_from_slice(&be(rate));
        out.extend_from_slice(&16_u16.to_be_bytes());
        out.extend_from_slice(&[0x40, 0x0E, 0xAC, 0x44, 0, 0, 0, 0, 0, 0]);
        out.extend_from_slice(b"SSND");
        out.extend_from_slice(&be(8 + data_len));
        out.resize(out.len() + 8 + data_len as usize, 0);
    } else {
        out.extend_from_slice(b"RIFF\0\0\0\0WAVEfmt ");
        out.extend_from_slice(&le(16));
        out.extend_from_slice(&1_u16.to_le_bytes());
        out.extend_from_slice(&1_u16.to_le_bytes());
        out.extend_from_slice(&le(rate));
        out.extend_from_slice(&le(rate * 2));
        out.extend_from_slice(&2_u16.to_le_bytes());
        out.extend_from_slice(&16_u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&le(data_len));
        out.resize(out.len() + data_len as usize, 0);
    }
    if let Some(tag) = tag {
        out.extend_from_slice(if aiff { b"ID3 " } else { b"id3 " });
        let size = u32::try_from(tag.len()).unwrap();
        out.extend_from_slice(&if aiff { be(size) } else { le(size) });
        out.extend_from_slice(tag);
        if tag.len() % 2 == 1 {
            out.push(0);
        }
    }
    let total = u32::try_from(out.len() - 8).unwrap();
    out[4..8].copy_from_slice(&if aiff { be(total) } else { le(total) });
    out
}

#[test]
fn aiff_and_wav_id3_chunks_are_edited_frame_by_frame() {
    let dir = tempfile::tempdir().unwrap();
    for aiff in [true, false] {
        let path = dir.path().join(if aiff { "t.aiff" } else { "t.wav" });
        // As the user's AIFFs: no description-less COMM, only described ones.
        let tag = id3_tag(3, &store_frames(false));
        let file = container(aiff, Some(&tag));
        std::fs::write(&path, &file).unwrap();
        let before = tags::read(&path).unwrap().snapshot;
        assert_eq!(
            rbl_identify::pipeline::current(&before, Field::SubGenre),
            "",
            "neither iTunNORM nor the ID3v1 Comment is the comment"
        );
        let changes = [change(Field::CatalogNumber, "ANCD2K10E", "ANCD2K10X"), change(Field::SubGenre, "", "Tech House")];
        let after = write::write(&path, &before, &write::plan(&before, &changes).unwrap(), None).unwrap();
        assert_eq!(rbl_identify::pipeline::current(&after, Field::SubGenre), "Tech House");
        let written = std::fs::read(&path).unwrap();
        // The audio chunk is where it was, byte for byte, and the sizes add up.
        let audio_end = file.len() - 8 - tag.len() - tag.len() % 2;
        assert_eq!(&written[8..audio_end], &file[8..audio_end]);
        let size = if aiff {
            u32::from_be_bytes(written[4..8].try_into().unwrap())
        } else {
            u32::from_le_bytes(written[4..8].try_into().unwrap())
        };
        assert_eq!(size as usize, written.len() - 8);
        // In the chunk: v2.3 kept, every described COMM and other frame kept,
        // one new description-less COMM.
        let new_tag = &written[audio_end + 8..];
        assert_eq!(new_tag[3], 3);
        let old = tag_frames(&tag);
        let new = tag_frames(new_tag);
        assert_eq!(new.len(), old.len() + 1);
        for (before, after) in old.iter().zip(&new) {
            if !String::from_utf8_lossy(&before.1).contains("Catalog #") {
                assert_eq!(before, after);
            }
        }
        assert!(String::from_utf8_lossy(&new.last().unwrap().1).ends_with("\0Tech House"));

        write::restore(&path, &before, &after, &[Field::CatalogNumber, Field::SubGenre]).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), file, "undo is byte for byte");
    }
}
