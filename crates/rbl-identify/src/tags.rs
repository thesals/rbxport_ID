//! Reading what a file already says: its tags, its true duration, and an
//! exact snapshot of every field identification may change. Read-only;
//! tag writing is Phase 4.

use std::collections::BTreeMap;
use std::path::Path;

use lofty::file::{AudioFile, TaggedFileExt};
use lofty::prelude::ItemKey;
use lofty::probe::Probe;
use lofty::tag::{Tag, TagType};

use crate::error::{IdentifyError, Result};
use crate::model::{ExistingTags, Field, FrameValues, RawId3, TagSnapshot};

/// A file's tags, its exact original values, and the duration of its
/// whole audio stream.
#[derive(Debug, Clone, PartialEq)]
pub struct FileInfo {
    pub tags: ExistingTags,
    /// Every [`Field`] exactly as stored, for undo. Taken from the same
    /// read as `tags`.
    pub snapshot: TagSnapshot,
    /// The whole file, in seconds: AcoustID's `duration` and the duration
    /// factor compare against this, not against the fingerprinted part.
    pub duration_secs: f64,
}

/// The keys a field is read from, in order of preference. The first that
/// holds a value wins for [`ExistingTags`]; the snapshot records the one
/// that was found. `Catalog #` is a ripping-tool spelling seen in the wild.
fn keys(field: Field) -> Vec<ItemKey> {
    match field {
        Field::Title => vec![ItemKey::TrackTitle],
        Field::Artist => vec![ItemKey::TrackArtist],
        Field::Album => vec![ItemKey::AlbumTitle],
        Field::Label => vec![ItemKey::Label, ItemKey::Publisher],
        Field::Year => vec![ItemKey::RecordingDate, ItemKey::Year],
        Field::Genre => vec![ItemKey::Genre],
        Field::Isrc => vec![ItemKey::Isrc],
        Field::CatalogNumber => vec![
            ItemKey::CatalogNumber,
            ItemKey::Unknown("Catalog #".to_owned()),
            ItemKey::Unknown("CATALOG #".to_owned()),
            ItemKey::Unknown("CATALOG".to_owned()),
        ],
        Field::MbRecordingId => vec![ItemKey::MusicBrainzRecordingId],
        Field::MbReleaseId => vec![ItemKey::MusicBrainzReleaseId],
        Field::MbReleaseGroupId => vec![ItemKey::MusicBrainzReleaseGroupId],
        Field::MbArtistId => vec![ItemKey::MusicBrainzArtistId],
        Field::MbReleaseTrackId => vec![ItemKey::MusicBrainzTrackId],
        Field::MbAlbumArtistId => vec![ItemKey::MusicBrainzReleaseArtistId],
        Field::DiscogsReleaseId => {
            vec![ItemKey::Unknown("DISCOGS_RELEASE_ID".to_owned()), ItemKey::Unknown("DISCOGS_RELEASEID".to_owned())]
        }
        Field::Artwork => Vec::new(),
    }
}

const TEXT_FIELDS: [Field; 15] = [
    Field::Title,
    Field::Artist,
    Field::Album,
    Field::Label,
    Field::Year,
    Field::Genre,
    Field::Isrc,
    Field::CatalogNumber,
    Field::MbRecordingId,
    Field::MbReleaseId,
    Field::MbReleaseGroupId,
    Field::MbArtistId,
    Field::MbReleaseTrackId,
    Field::MbAlbumArtistId,
    Field::DiscogsReleaseId,
];

/// Every frame holding a field, with its values exactly as stored, in the
/// order of [`keys`]. Empty when the field is absent.
fn frames(tag: &Tag, field: Field) -> Vec<FrameValues> {
    let tag_type = tag.tag_type();
    keys(field)
        .iter()
        .filter_map(|key| {
            let values: Vec<String> = tag.get_strings(key).map(str::to_owned).collect();
            (!values.is_empty()).then(|| FrameValues { frame: frame_name(key, tag_type), values })
        })
        .collect()
}

/// A key as the tag format names it: `TPUB`, `TXXX:Catalog #`,
/// `CATALOGNUMBER`. ID3 user-defined text frames are written `TXXX:<name>`.
fn frame_name(key: &ItemKey, tag_type: TagType) -> String {
    let native = match key {
        ItemKey::Unknown(name) => Some(name.as_str()),
        _ => key.map_key(tag_type, true),
    };
    match native {
        Some(name) if tag_type == TagType::Id3v2 && !is_id3_frame_id(name) => format!("TXXX:{name}"),
        Some(name) => name.to_owned(),
        None => format!("{key:?}"),
    }
}

/// A four-character `ID3v2` frame ID (`TPUB`, `TSRC`), as opposed to a
/// user-defined frame's description. lofty names Picard's recording ID
/// `UFID:http://musicbrainz.org`, which is kept as it is.
fn is_id3_frame_id(name: &str) -> bool {
    (name.len() == 4 && name.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())) || name.starts_with("UFID:")
}

/// Reads the primary tag (or the first there is), the exact snapshot and
/// the stream duration. Unlike the library import, an absent title stays
/// empty: identification must know a tag is missing rather than see the
/// filename in its place.
///
/// Cover art is read (not skipped as the library import does) only to know
/// whether the file has any: artwork is fetched only for a file without.
pub fn read(path: &Path) -> Result<FileInfo> {
    let tagged = Probe::open(path)
        .and_then(Probe::read)
        .map_err(|e| IdentifyError::Io(std::io::Error::other(format!("{}: {e}", path.display()))))?;
    let duration_secs = tagged.properties().duration().as_secs_f64();
    let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) else {
        return Ok(FileInfo { tags: ExistingTags::default(), snapshot: TagSnapshot::default(), duration_secs });
    };

    let values: BTreeMap<Field, Vec<FrameValues>> = TEXT_FIELDS
        .iter()
        .map(|&field| (field, frames(tag, field)))
        .filter(|(_, found)| !found.is_empty())
        .collect();
    let has_artwork = tagged.tags().iter().any(|t| t.picture_count() > 0);
    // The first value of the first frame, for scoring.
    let first_value = |field: Field| values.get(&field).and_then(|f| f.first()).and_then(|f| f.values.first());
    let first = |field: Field| first_value(field).map(|v| v.trim().to_owned()).unwrap_or_default();

    let tags = ExistingTags {
        artist: first(Field::Artist),
        title: first(Field::Title),
        album: first(Field::Album),
        label: first(Field::Label),
        year: first_value(Field::Year).and_then(|v| v.trim().get(..4)).and_then(|y| y.parse().ok()),
        isrc: first(Field::Isrc),
        genre: first(Field::Genre),
        catalog_number: first(Field::CatalogNumber),
        mb_recording_id: first(Field::MbRecordingId),
        mb_release_id: first(Field::MbReleaseId),
        discogs_release_id: first(Field::DiscogsReleaseId),
        has_artwork,
    };
    let snapshot = TagSnapshot { format: format!("{:?}", tag.tag_type()), values, has_artwork, raw_id3: raw_id3(path)? };
    Ok(FileInfo { tags, snapshot, duration_secs })
}

/// The `ID3v2` block at the start of the file and the `ID3v1` trailer at
/// its end, byte for byte; `None` when the file has neither.
fn raw_id3(path: &Path) -> Result<Option<RawId3>> {
    use base64::Engine as _;
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let mut header = [0_u8; 10];
    let v2 = if len >= 10 && file.read_exact(&mut header).is_ok() && &header[..3] == b"ID3" {
        // Syncsafe size of everything after the header, plus a 10-byte
        // footer when flag bit 4 says there is one.
        let size = header[6..10].iter().fold(0_u64, |acc, &b| (acc << 7) | u64::from(b & 0x7f));
        let footer = if header[5] & 0x10 == 0 { 0 } else { 10 };
        let total = 10 + size + footer;
        if total > len {
            return Err(IdentifyError::Io(std::io::Error::other(format!("{}: ID3v2 size beyond the file", path.display()))));
        }
        let mut block = vec![0_u8; usize::try_from(total).unwrap_or(usize::MAX)];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut block)?;
        Some(block)
    } else {
        None
    };
    let v1 = if len >= 128 {
        let mut trailer = [0_u8; 128];
        file.seek(SeekFrom::End(-128))?;
        file.read_exact(&mut trailer)?;
        (&trailer[..3] == b"TAG").then(|| trailer.to_vec())
    } else {
        None
    };
    if v2.is_none() && v1.is_none() {
        return Ok(None);
    }
    let encode = |bytes: Vec<u8>| base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes);
    Ok(Some(RawId3 { v2: v2.map(encode), v1: v1.map(encode) }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use lofty::tag::TagType;

    use super::*;

    #[test]
    fn a_missing_file_is_an_error_naming_it() {
        let error = read(Path::new("no/such/track.mp3")).unwrap_err().to_string();
        assert!(error.contains("track.mp3"), "{error}");
    }

    #[test]
    fn every_frame_is_kept_exactly_and_absent_fields_are_missing() {
        let mut tag = Tag::new(TagType::Id3v2);
        tag.insert_text(ItemKey::TrackTitle, "  Spaced Title ".to_owned());
        tag.insert_text(ItemKey::RecordingDate, "2014-06-01".to_owned());
        tag.insert_text(ItemKey::CatalogNumber, "STD-1".to_owned());
        // Unknown keys are only accepted unchecked in memory; reading a file
        // with TXXX:Catalog # yields this key (checked against the test set).
        tag.insert_unchecked(lofty::tag::TagItem::new(
            ItemKey::Unknown("Catalog #".to_owned()),
            lofty::tag::ItemValue::Text("ANCD2K10E".to_owned()),
        ));
        let frame = |frame: &str, value: &str| FrameValues { frame: frame.to_owned(), values: vec![value.to_owned()] };
        assert_eq!(frames(&tag, Field::Title), vec![frame("TIT2", "  Spaced Title ")]);
        assert_eq!(frames(&tag, Field::Year), vec![frame("TDRC", "2014-06-01")]);
        // Both catalog frames, each under its own name, so a write can update
        // the one in use and undo can restore exactly these.
        assert_eq!(
            frames(&tag, Field::CatalogNumber),
            vec![frame("TXXX:CATALOGNUMBER", "STD-1"), frame("TXXX:Catalog #", "ANCD2K10E")]
        );
        assert_eq!(frames(&tag, Field::Label), Vec::new());
    }

    fn decode(text: &str) -> Vec<u8> {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD_NO_PAD.decode(text).unwrap()
    }

    #[test]
    fn the_raw_id3_blocks_are_captured_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.mp3");
        // ID3v2.3 header with a 20-byte body (syncsafe), audio, ID3v1 trailer.
        let mut v2 = b"ID3\x03\x00\x00\x00\x00\x00\x14".to_vec();
        v2.extend_from_slice(&[0xAB; 20]);
        let mut v1 = b"TAG".to_vec();
        v1.extend_from_slice(&[0x20; 125]);
        let mut file = v2.clone();
        file.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        file.extend_from_slice(&v1);
        std::fs::write(&path, &file).unwrap();
        let raw = raw_id3(&path).unwrap().unwrap();
        assert_eq!(decode(raw.v2.as_deref().unwrap()), v2);
        assert_eq!(decode(raw.v1.as_deref().unwrap()), v1);

        std::fs::write(&path, [0xFF_u8, 0xFB, 0x90, 0x00]).unwrap();
        assert_eq!(raw_id3(&path).unwrap(), None);
        // A size claiming more than the file holds is refused, not truncated.
        std::fs::write(&path, b"ID3\x03\x00\x00\x7f\x7f\x7f\x7f").unwrap();
        assert!(raw_id3(&path).is_err());
    }
}
