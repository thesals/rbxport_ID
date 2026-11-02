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
        // TODO(Phase 4): read from the frame sub-genre is written to, once
        // it is confirmed (COMM, TIT3 or TXXX:DESCRIPTION; comments are on the
        // never-modify list). Until then it is not read: never in the
        // snapshot, so always shown as empty, and never auto-applied.
        Field::SubGenre | Field::Artwork => Vec::new(),
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

    let mut values: BTreeMap<Field, Vec<FrameValues>> = TEXT_FIELDS
        .iter()
        .map(|&field| (field, frames(tag, field)))
        .filter(|(_, found)| !found.is_empty())
        .collect();
    let (raw_v2, raw_v1) = read_raw_id3(path)?;
    // A frame the reader could not parse (an invalid `2010-00-00` date) must
    // not look absent, or a proposal would count as filling an empty field
    // and auto-apply could overwrite it. Such a field is taken from the raw
    // frame instead.
    for (field, frame) in raw_v2.as_deref().map(id3v2_fields).unwrap_or_default() {
        values.entry(field).or_insert_with(|| vec![frame]);
    }
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
    let raw_id3 = (raw_v2.is_some() || raw_v1.is_some()).then(|| {
        use base64::Engine as _;
        let encode = |bytes: Vec<u8>| base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes);
        RawId3 { v2: raw_v2.map(encode), v1: raw_v1.map(encode) }
    });
    let snapshot = TagSnapshot { format: format!("{:?}", tag.tag_type()), values, has_artwork, raw_id3 };
    Ok(FileInfo { tags, snapshot, duration_secs })
}

/// What [`read_raw_id3`] marks a frame's value as when it cannot decode it.
pub const UNREADABLE: &str = "<unreadable>";

/// The fields an `ID3v2` block holds, straight from its frames: one entry
/// per frame that maps to a [`Field`], with the frame's text decoded
/// leniently (or [`UNREADABLE`]). Frame names are as stored: an `ID3v2.3`
/// file shows `TYER` where the tag reader reports `TDRC`. Versions before
/// 2.3 (three-character frame IDs) yield nothing.
fn id3v2_fields(block: &[u8]) -> Vec<(Field, FrameValues)> {
    let Some(&version) = block.get(3) else { return Vec::new() };
    if !(3..=4).contains(&version) || block.len() < 10 {
        return Vec::new();
    }
    let flags = block[5];
    if flags & 0x80 != 0 {
        // Whole-tag unsynchronisation: frames would need un-escaping first.
        // Rare; the reader's own values stand for such a tag.
        return Vec::new();
    }
    let body_end = block.len().min(10 + syncsafe(&block[6..10]));
    let mut at = 10;
    if flags & 0x40 != 0 {
        // Extended header: v2.3 gives its size without the 4 size bytes,
        // v2.4 as a syncsafe size including them.
        let Some(size_bytes) = block.get(10..14) else { return Vec::new() };
        at += if version == 4 { syncsafe(size_bytes) } else { 4 + be_u32(size_bytes) };
    }
    let mut found = Vec::new();
    while let Some(header) = block.get(at..at + 10).filter(|_| at + 10 <= body_end) {
        if header[0] == 0 || !header[..4].iter().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()) {
            break; // padding, or nothing a frame could start with
        }
        let id = String::from_utf8_lossy(&header[..4]).into_owned();
        let size = if version == 4 { syncsafe(&header[4..8]) } else { be_u32(&header[4..8]) };
        let Some(data) = block.get(at + 10..at + 10 + size) else { break };
        if let Some(entry) = frame_field(&id, data) {
            found.push(entry);
        }
        at += 10 + size;
    }
    found
}

/// The field a frame holds, with its values, when it is one identification
/// may change.
fn frame_field(id: &str, data: &[u8]) -> Option<(Field, FrameValues)> {
    let (name, values) = match id {
        "TXXX" => {
            let (description, value) = split_once_text(data)?;
            (format!("TXXX:{description}"), vec![value])
        }
        "UFID" => {
            let end = data.iter().position(|&b| b == 0)?;
            let owner = String::from_utf8_lossy(&data[..end]).into_owned();
            (format!("UFID:{owner}"), vec![String::from_utf8_lossy(&data[end + 1..]).into_owned()])
        }
        _ if id.starts_with('T') => (id.to_owned(), decode_text(data).map_or_else(|| vec![UNREADABLE.to_owned()], |text| split_values(&text))),
        _ => return None,
    };
    let field = match name.as_str() {
        "TIT2" => Field::Title,
        "TPE1" => Field::Artist,
        "TALB" => Field::Album,
        "TPUB" => Field::Label,
        "TDRC" | "TYER" => Field::Year,
        "TCON" => Field::Genre,
        "TSRC" => Field::Isrc,
        "UFID:http://musicbrainz.org" => Field::MbRecordingId,
        other => match other.strip_prefix("TXXX:").map(str::to_ascii_lowercase).as_deref() {
            Some("catalognumber" | "catalog #" | "catalog") => Field::CatalogNumber,
            Some("musicbrainz album id") => Field::MbReleaseId,
            Some("musicbrainz release group id") => Field::MbReleaseGroupId,
            Some("musicbrainz artist id") => Field::MbArtistId,
            Some("musicbrainz release track id") => Field::MbReleaseTrackId,
            Some("musicbrainz album artist id") => Field::MbAlbumArtistId,
            Some("discogs_release_id" | "discogs_releaseid") => Field::DiscogsReleaseId,
            _ => return None,
        },
    };
    let values = if values.iter().all(String::is_empty) { vec![UNREADABLE.to_owned()] } else { values };
    Some((field, FrameValues { frame: name, values }))
}

/// A text frame's payload: an encoding byte, then text.
fn decode_text(data: &[u8]) -> Option<String> {
    let (&encoding, text) = data.split_first()?;
    match encoding {
        0 => Some(text.iter().map(|&b| char::from(b)).collect()),
        3 => Some(String::from_utf8_lossy(text).into_owned()),
        1 | 2 => {
            let (big_endian, text) = match text {
                [0xFE, 0xFF, rest @ ..] => (true, rest),
                [0xFF, 0xFE, rest @ ..] => (false, rest),
                _ => (encoding == 2, text),
            };
            let units: Vec<u16> = text
                .chunks_exact(2)
                .map(|p| if big_endian { u16::from_be_bytes([p[0], p[1]]) } else { u16::from_le_bytes([p[0], p[1]]) })
                .collect();
            Some(String::from_utf16_lossy(&units))
        }
        _ => None,
    }
}

/// A `TXXX` payload: description and value, in the frame's encoding.
fn split_once_text(data: &[u8]) -> Option<(String, String)> {
    let text = decode_text(data)?;
    let (description, value) = text.split_once('\0').unwrap_or((text.as_str(), ""));
    // In UTF-16 each string has its own BOM; the first was consumed when
    // decoding, the value's would be left as a leading U+FEFF.
    let value = value.trim_end_matches('\0').trim_start_matches('\u{feff}');
    Some((description.trim_start_matches('\u{feff}').to_owned(), value.to_owned()))
}

/// Multiple values of a text frame are NUL-separated; trailing NULs are
/// padding.
fn split_values(text: &str) -> Vec<String> {
    text.trim_end_matches('\0').split('\0').map(str::to_owned).collect()
}

fn syncsafe(bytes: &[u8]) -> usize {
    bytes.iter().fold(0_usize, |acc, &b| (acc << 7) | usize::from(b & 0x7f))
}

fn be_u32(bytes: &[u8]) -> usize {
    bytes.iter().fold(0_usize, |acc, &b| (acc << 8) | usize::from(b))
}

/// A file's `ID3v2` block and `ID3v1` trailer, either possibly absent.
type RawBlocks = (Option<Vec<u8>>, Option<Vec<u8>>);

/// The `ID3v2` block at the start of the file and the `ID3v1` trailer at
/// its end, byte for byte.
fn read_raw_id3(path: &Path) -> Result<RawBlocks> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let mut header = [0_u8; 10];
    let v2 = if len >= 10 && file.read_exact(&mut header).is_ok() && &header[..3] == b"ID3" {
        // Syncsafe size of everything after the header, plus a 10-byte
        // footer, which only ID3v2.4 has (flag bit 4).
        let size = header[6..10].iter().fold(0_u64, |acc, &b| (acc << 7) | u64::from(b & 0x7f));
        let footer = if header[3] == 4 && header[5] & 0x10 != 0 { 10 } else { 0 };
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
    Ok((v2, v1))
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


    #[test]
    fn the_raw_id3_blocks_are_captured_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.mp3");
        // ID3v2.3 header with a 20-byte body (syncsafe), audio, ID3v1 trailer.
        let mut v2 = b"ID3\x03\x00\x00\x00\x00\x00\x14".to_vec();
        v2.extend_from_slice(&[0x00; 20]);
        let mut v1 = b"TAG".to_vec();
        v1.extend_from_slice(&[0x20; 125]);
        let mut file = v2.clone();
        file.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        file.extend_from_slice(&v1);
        std::fs::write(&path, &file).unwrap();
        assert_eq!(read_raw_id3(&path).unwrap(), (Some(v2), Some(v1)));

        std::fs::write(&path, [0xFF_u8, 0xFB, 0x90, 0x00]).unwrap();
        assert_eq!(read_raw_id3(&path).unwrap(), (None, None));
        // A size claiming more than the file holds is refused, not truncated.
        std::fs::write(&path, b"ID3\x03\x00\x00\x7f\x7f\x7f\x7f").unwrap();
        assert!(read_raw_id3(&path).is_err());
        // The footer flag only means a footer in ID3v2.4.
        let mut v23_with_flag = b"ID3\x03\x00\x10\x00\x00\x00\x04".to_vec();
        v23_with_flag.extend_from_slice(&[0; 4]);
        std::fs::write(&path, &v23_with_flag).unwrap();
        assert_eq!(read_raw_id3(&path).unwrap().0.unwrap().len(), 14);
    }

    /// An ID3v2.3 block holding the given (frame ID, payload) frames.
    fn id3v23(frames: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (id, payload) in frames {
            body.extend_from_slice(id.as_bytes());
            body.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
            body.extend_from_slice(&[0, 0]);
            body.extend_from_slice(payload);
        }
        body.extend_from_slice(&[0; 16]); // padding
        let size = body.len();
        let mut block = b"ID3\x03\x00\x00".to_vec();
        block.extend(
            [21, 14, 7, 0].iter().map(|shift| u8::try_from((size >> shift) & 0x7f).unwrap()),
        );
        block.extend_from_slice(&body);
        block
    }

    fn latin1(text: &str) -> Vec<u8> {
        let mut payload = vec![0];
        payload.extend_from_slice(text.as_bytes());
        payload
    }

    #[test]
    fn frames_the_reader_drops_are_still_seen_as_present() {
        let block = id3v23(&[
            ("TDRC", latin1("2010-00-00")),
            ("TXXX", latin1("Catalog #\0ANCD2K10E")),
            ("TPUB", latin1("Ministry Of Sound (UK)")),
            ("TCOM", latin1("not a field we change")),
        ]);
        let found = id3v2_fields(&block);
        let get = |field: Field| found.iter().find(|(f, _)| *f == field).map(|(_, v)| v.clone());
        assert_eq!(get(Field::Year), Some(FrameValues { frame: "TDRC".into(), values: vec!["2010-00-00".into()] }));
        assert_eq!(
            get(Field::CatalogNumber),
            Some(FrameValues { frame: "TXXX:Catalog #".into(), values: vec!["ANCD2K10E".into()] })
        );
        assert_eq!(get(Field::Label).unwrap().values, vec!["Ministry Of Sound (UK)".to_owned()]);
        assert_eq!(found.len(), 3, "TCOM is not a field identification changes");
    }

    #[test]
    fn utf16_values_and_undecodable_frames_are_handled() {
        let mut utf16 = vec![1, 0xFF, 0xFE];
        utf16.extend("Röyksopp".encode_utf16().flat_map(u16::to_le_bytes));
        let block = id3v23(&[("TPE1", utf16), ("TALB", vec![9, b'x'])]);
        let found = id3v2_fields(&block);
        assert_eq!(found[0].1.values, vec!["Röyksopp".to_owned()]);
        assert_eq!(found[1].1.values, vec![UNREADABLE.to_owned()], "an unknown encoding is present, not absent");
        assert!(id3v2_fields(b"ID3\x02\x00\x00\x00\x00\x00\x00").is_empty(), "v2.2 is not scanned");
    }
}
