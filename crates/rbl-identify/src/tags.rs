//! Reading what a file already says: its tags, its true duration, and an
//! exact snapshot of every field identification may change. Read-only;
//! writing is the tag writer's (Phase 4).

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
pub(crate) fn keys(field: Field) -> Vec<ItemKey> {
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
        // The comment the sub-genre is appended to (crate::comment). In an
        // ID3v2 tag read from the file's raw frames instead, where present:
        // only a description-less COMM counts, which the generic item
        // cannot be relied on to tell apart.
        Field::SubGenre => vec![ItemKey::Comment],
        Field::Artwork => Vec::new(),
    }
}

const TEXT_FIELDS: [Field; 16] = [
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
    Field::SubGenre,
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
pub(crate) fn frame_name(key: &ItemKey, tag_type: TagType) -> String {
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
    let raw_fields = raw_v2.as_deref().and_then(id3v2_fields);
    if raw_fields.is_some() {
        // The comment is exactly the first description-less COMM frame of a
        // block that was scanned; otherwise the reader's comment stands.
        values.remove(&Field::SubGenre);
    }
    for (field, frame) in raw_fields.unwrap_or_default() {
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
        comment: first_value(Field::SubGenre).cloned().unwrap_or_default(),
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
/// file shows `TYER` where the tag reader reports `TDRC`. `None` when the
/// block is not scanned at all: versions before 2.3 (three-character frame
/// IDs) and whole-tag unsynchronisation.
fn id3v2_fields(block: &[u8]) -> Option<Vec<(Field, FrameValues)>> {
    let &version = block.get(3)?;
    if !(3..=4).contains(&version) || block.len() < 10 {
        return None;
    }
    let flags = block[5];
    if flags & 0x80 != 0 {
        // Whole-tag unsynchronisation: frames would need un-escaping first.
        // Rare; the reader's own values stand for such a tag.
        return None;
    }
    let body_end = block.len().min(10 + syncsafe(&block[6..10]));
    let mut at = 10;
    if flags & 0x40 != 0 {
        // Extended header: v2.3 gives its size without the 4 size bytes,
        // v2.4 as a syncsafe size including them.
        let size_bytes = block.get(10..14)?;
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
    Some(found)
}

/// The field a frame holds, with its values, when it is one identification
/// may change.
pub(crate) fn frame_field(id: &str, data: &[u8]) -> Option<(Field, FrameValues)> {
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
        "COMM" => {
            // Encoding, three bytes of language, then description and text.
            let (&encoding, rest) = data.split_first()?;
            let mut text_data = vec![encoding];
            text_data.extend_from_slice(rest.get(3..).unwrap_or_default());
            let Some((description, text)) = split_once_text(&text_data) else {
                // Not decodable, so not known to be described or empty: it
                // may be the comment, and must not look absent (a proposal
                // would then write over it).
                return Some((Field::SubGenre, FrameValues { frame: "COMM".to_owned(), values: vec![UNREADABLE.to_owned()] }));
            };
            let text = text.trim_end_matches('\0').to_owned();
            // A described COMM (iTunNORM, ID3v1 Comment) is not the comment,
            // and an empty one says nothing.
            if !description.is_empty() || text.is_empty() {
                return None;
            }
            return Some((Field::SubGenre, FrameValues { frame: "COMM".to_owned(), values: vec![text] }));
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
pub(crate) fn split_once_text(data: &[u8]) -> Option<(String, String)> {
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

pub(crate) fn syncsafe(bytes: &[u8]) -> usize {
    bytes.iter().fold(0_usize, |acc, &b| (acc << 7) | usize::from(b & 0x7f))
}

pub(crate) fn be_u32(bytes: &[u8]) -> usize {
    bytes.iter().fold(0_usize, |acc, &b| (acc << 8) | usize::from(b))
}

/// A file's `ID3v2` block and `ID3v1` trailer, either possibly absent.
type RawBlocks = (Option<Vec<u8>>, Option<Vec<u8>>);

/// A file that keeps its `ID3v2` tag in a chunk rather than at its start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Container {
    /// `FORM` ... `AIFF`/`AIFC`: big-endian chunk sizes, an `ID3 ` chunk.
    Aiff,
    /// `RIFF` ... `WAVE`: little-endian chunk sizes, an `id3 ` or `ID3 ` chunk.
    Wav,
}

impl Container {
    /// The container a file's first twelve bytes say it is.
    pub(crate) fn of(header: &[u8]) -> Option<Self> {
        match (header.get(..4), header.get(8..12)) {
            (Some(b"FORM"), Some(b"AIFF" | b"AIFC")) => Some(Self::Aiff),
            (Some(b"RIFF"), Some(b"WAVE")) => Some(Self::Wav),
            _ => None,
        }
    }

    pub(crate) fn size(self, bytes: [u8; 4]) -> u64 {
        u64::from(match self {
            Self::Aiff => u32::from_be_bytes(bytes),
            Self::Wav => u32::from_le_bytes(bytes),
        })
    }

    /// The id a new tag chunk gets: `ID3 ` in AIFF (what rekordbox's AIFF
    /// downloads carry), lofty's `id3 ` in WAV.
    pub(crate) fn chunk_id(self) -> &'static [u8; 4] {
        match self {
            Self::Aiff => b"ID3 ",
            Self::Wav => b"id3 ",
        }
    }
}

/// Where a container's ID3 chunk is: the offset of its 8-byte header and the
/// payload's length. Walks the chunk headers only.
pub(crate) fn find_id3_chunk<R: std::io::Read + std::io::Seek>(
    reader: &mut R,
    container: Container,
    len: u64,
) -> std::io::Result<Option<(u64, u64)>> {
    use std::io::SeekFrom;
    let mut at = 12_u64;
    let mut header = [0_u8; 8];
    while at + 8 <= len {
        reader.seek(SeekFrom::Start(at))?;
        reader.read_exact(&mut header)?;
        let size = container.size([header[4], header[5], header[6], header[7]]);
        if header[..4].eq_ignore_ascii_case(b"ID3 ") {
            return Ok((at + 8 + size <= len).then_some((at, size)));
        }
        at += 8 + size + (size & 1);
    }
    Ok(None)
}

/// The `ID3v2` block at the start of the file and the `ID3v1` trailer at
/// its end, byte for byte. For an AIFF or WAV file, the `v2` block is the
/// whole payload of its ID3 chunk, and there is no trailer.
fn read_raw_id3(path: &Path) -> Result<RawBlocks> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let mut header = [0_u8; 12];
    let mut got = 0;
    while got < header.len() {
        let n = file.read(&mut header[got..])?;
        if n == 0 {
            break;
        }
        got += n;
    }
    if let Some(container) = Container::of(&header[..got]) {
        let Some((at, size)) = find_id3_chunk(&mut file, container, len)? else { return Ok((None, None)) };
        let mut block = vec![0_u8; usize::try_from(size).unwrap_or(usize::MAX)];
        file.seek(SeekFrom::Start(at + 8))?;
        file.read_exact(&mut block)?;
        return Ok((Some(block), None));
    }
    let v2 = if got >= 10 && &header[..3] == b"ID3" {
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
        let found = id3v2_fields(&block).unwrap();
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
    fn the_comment_is_the_first_comm_frame_without_a_description() {
        let comm = |lang: &str, description: &str, text: &str| {
            let mut payload = vec![0];
            payload.extend_from_slice(lang.as_bytes());
            payload.extend_from_slice(description.as_bytes());
            payload.push(0);
            payload.extend_from_slice(text.as_bytes());
            ("COMM", payload)
        };
        let mut utf16 = vec![1];
        utf16.extend_from_slice(b"Pur");
        utf16.extend_from_slice(&[0xFF, 0xFE, 0, 0, 0xFF, 0xFE]);
        utf16.extend("Purchased at Beatport.com".encode_utf16().flat_map(u16::to_le_bytes));
        let block = id3v23(&[
            comm("eng", "iTunNORM", " 000012A1"),
            comm("eng", "ID3v1 Comment", "Ministry"),
            comm("eng", "", ""),
            ("COMM", utf16),
            comm("eng", "", "second"),
        ]);
        let found: Vec<_> = id3v2_fields(&block).unwrap().into_iter().filter(|(f, _)| *f == Field::SubGenre).collect();
        assert_eq!(found.len(), 2, "described and empty COMM frames are not the comment");
        assert_eq!(found[0].1, FrameValues { frame: "COMM".into(), values: vec!["Purchased at Beatport.com".into()] });
    }

    #[test]
    fn utf16_values_and_undecodable_frames_are_handled() {
        let mut utf16 = vec![1, 0xFF, 0xFE];
        utf16.extend("Röyksopp".encode_utf16().flat_map(u16::to_le_bytes));
        let block = id3v23(&[("TPE1", utf16), ("TALB", vec![9, b'x'])]);
        let found = id3v2_fields(&block).unwrap();
        assert_eq!(found[0].1.values, vec!["Röyksopp".to_owned()]);
        assert_eq!(found[1].1.values, vec![UNREADABLE.to_owned()], "an unknown encoding is present, not absent");
        assert_eq!(id3v2_fields(b"ID3\x02\x00\x00\x00\x00\x00\x00"), None, "v2.2 is not scanned");
        assert_eq!(id3v2_fields(b"ID3\x03\x00\x80\x00\x00\x00\x00"), None, "nor an unsynchronised tag");
        // An undecodable COMM may be the comment: it reads as unreadable.
        let block = id3v23(&[("COMM", vec![9, b'e', b'n', b'g', b'x'])]);
        assert_eq!(
            id3v2_fields(&block).unwrap(),
            vec![(Field::SubGenre, FrameValues { frame: "COMM".into(), values: vec![UNREADABLE.into()] })]
        );
    }
}
