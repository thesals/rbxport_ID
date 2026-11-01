//! The data passed between the stages: what the file says, what the
//! services propose, and what scoring and the decision make of it.

use serde::{Deserialize, Serialize};

/// What a file's own tags say before identification. Empty strings and
/// `None` mean the tag is absent.
///
/// For scoring and proposals only. It is lossy (trimmed values, the year
/// alone of a full date, absent and empty alike), so it must never serve
/// as the undo snapshot; Phase 2 adds an exact raw snapshot for that.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExistingTags {
    pub artist: String,
    /// As tagged, mix name included: `The Music (Extended Mix)`.
    pub title: String,
    pub album: String,
    pub label: String,
    pub year: Option<u16>,
    pub isrc: String,
    pub genre: String,
    /// `CATALOGNUMBER`, or a variant spelling such as `Catalog #`.
    #[serde(default)]
    pub catalog_number: String,
    /// Picard's MusicBrainz identifiers, when the file carries them.
    #[serde(default)]
    pub mb_recording_id: String,
    #[serde(default)]
    pub mb_release_id: String,
    #[serde(default)]
    pub discogs_release_id: String,
    /// Whether the file has embedded artwork. Artwork is only ever fetched
    /// for a file without any.
    #[serde(default)]
    pub has_artwork: bool,
}

/// A tag field identification may propose a value for. Key, BPM, cues,
/// grid, rating, colour and comments are deliberately absent: the spec
/// forbids touching them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Field {
    Title,
    Artist,
    Album,
    Label,
    Year,
    Genre,
    Isrc,
    CatalogNumber,
    MbRecordingId,
    MbReleaseId,
    MbReleaseGroupId,
    MbArtistId,
    /// Picard's `MusicBrainz Release Track Id`: tied to the release, so it
    /// must change (or be cleared) whenever the release ID does.
    MbReleaseTrackId,
    /// Picard's `MusicBrainz Album Artist Id`, also tied to the release.
    MbAlbumArtistId,
    DiscogsReleaseId,
    Artwork,
}

impl Field {
    /// The value an absent artwork is described by in a [`FieldChange`].
    pub const NO_ARTWORK: &'static str = "none";
}

/// One proposed change, current value against proposed, as the review
/// screen shows it with a checkbox. `current` is empty when the field is
/// absent; for [`Field::Artwork`] the values are descriptions
/// (`"none"`, `"embedded"`, `"500×500 JPEG from Cover Art Archive"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldChange {
    pub field: Field,
    pub current: String,
    pub proposed: String,
}

/// Where proposed artwork comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtworkSource {
    CoverArtArchive,
    Discogs,
}

/// Artwork fetched for a file that has none, held in the store by hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtworkRef {
    pub source: ArtworkSource,
    /// Where it was fetched from; never a URL carrying a secret.
    pub url: String,
    /// Lowercase hex SHA-256 of the image bytes.
    pub sha256: String,
    /// `image/jpeg` or `image/png`.
    pub mime: String,
    pub bytes: u64,
}

/// What may change in a file: only fields that actually differ.
///
/// Artwork is one change like any other: `changes` holds a
/// [`Field::Artwork`] entry (current [`Field::NO_ARTWORK`]) exactly when
/// `artwork` is `Some`, and only for a file that has none. Build proposals
/// with [`Proposal::new`] so the two can never disagree.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    pub changes: Vec<FieldChange>,
    pub artwork: Option<ArtworkRef>,
}

impl Proposal {
    /// A proposal from text-field changes and, for a file without artwork,
    /// fetched artwork. Any `Field::Artwork` entry in `changes` is replaced
    /// by the one derived from `artwork`, and artwork for a file that already
    /// has some is dropped: identification never replaces artwork.
    #[must_use]
    pub fn new(changes: Vec<FieldChange>, artwork: Option<ArtworkRef>, file_has_artwork: bool) -> Self {
        let mut changes: Vec<FieldChange> = changes.into_iter().filter(|c| c.field != Field::Artwork).collect();
        let artwork = artwork.filter(|_| !file_has_artwork);
        if let Some(art) = &artwork {
            let source = match art.source {
                ArtworkSource::CoverArtArchive => "Cover Art Archive",
                ArtworkSource::Discogs => "Discogs",
            };
            let kind = if art.mime == "image/png" { "PNG" } else { "JPEG" };
            changes.push(FieldChange {
                field: Field::Artwork,
                current: Field::NO_ARTWORK.to_owned(),
                proposed: format!("{kind} from {source}"),
            });
        }
        Self { changes, artwork }
    }

    /// True when every change fills a field that is empty now (artwork:
    /// added to a file that has none): the only proposals auto-apply may
    /// ever take. An empty proposal changes nothing and is not "only empty
    /// fields".
    #[must_use]
    pub fn fills_only_empty(&self) -> bool {
        !self.changes.is_empty()
            && self.changes.iter().all(|c| {
                c.current.trim().is_empty() || (c.field == Field::Artwork && c.current == Field::NO_ARTWORK)
            })
    }
}

/// The exact original values of every [`Field`], taken before anything is
/// written so a change can be undone. Unlike [`ExistingTags`] nothing is
/// trimmed or reduced: a field's values are stored as the tag holds them
/// (all of a multi-value frame, a full `2014-06-01` date), per source
/// frame, and a field the file does not have is simply missing from
/// `values`.
///
/// Write rule for Phase 4: a field already held under a variant frame
/// (`TXXX:Catalog #`) is updated in that frame, never duplicated into the
/// standard one, and undo restores exactly the frames listed here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagSnapshot {
    /// The tag type the values were read from (`Id3v2`, `VorbisComments`,
    /// `Mp4Ilst`, ...). For ID3 the exact version is in `raw_id3`'s header;
    /// a write must keep it.
    pub format: String,
    /// Every frame holding each field, in reading order.
    pub values: std::collections::BTreeMap<Field, Vec<FrameValues>>,
    pub has_artwork: bool,
    /// For a file with ID3 tags: the `ID3v2` block (header included) and the
    /// 128-byte `ID3v1` trailer, byte for byte, as unpadded standard base64.
    /// `values` is only as exact as the tag reader; it drops what it cannot
    /// parse (an invalid `2010-00-00` date, say). These bytes let undo put a
    /// file's tags back exactly, whatever the reader made of them.
    #[serde(default)]
    pub raw_id3: Option<RawId3>,
}

/// One frame's values for a field: `frame` names it as the tag format does
/// (`TPUB`, `TXXX:Catalog #`, `CATALOGNUMBER`, `©day`), so undo restores the
/// same frame it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameValues {
    pub frame: String,
    pub values: Vec<String>,
}

/// A file's ID3 tags, byte for byte. Either part may be absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawId3 {
    pub v2: Option<String>,
    pub v1: Option<String>,
}

/// What Discogs adds to an identified track.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Enrichment {
    pub discogs_release_id: Option<u64>,
    pub label: String,
    pub catalog_number: String,
    pub year: Option<u16>,
    /// Discogs styles: the sub-genre granularity (Tech House, Progressive
    /// House) the genre policy writes.
    pub styles: Vec<String>,
    pub genres: Vec<String>,
}

/// Where a candidate came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CandidateSource {
    /// An AcoustID fingerprint match, resolved through MusicBrainz.
    Fingerprint,
    /// A MusicBrainz text search on the existing artist and title.
    TextSearch,
}

/// One release a recording appears on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseInfo {
    pub mbid: String,
    pub title: String,
    /// `YYYY`, `YYYY-MM` or `YYYY-MM-DD`, as MusicBrainz gives it.
    pub date: String,
    pub country: String,
    pub label: String,
    pub catalog_number: String,
    pub release_group_mbid: String,
    /// `Album`, `Single`, `EP`, ... (`primary-type`).
    pub release_group_type: String,
    /// `Compilation`, `Remix`, `DJ-mix`, ... (`secondary-types`).
    pub secondary_types: Vec<String>,
}

/// A recording the services propose for a file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub recording_mbid: String,
    /// The credited artist names, in order.
    pub artists: Vec<String>,
    /// The credit as MusicBrainz joins it: names with their join phrases.
    pub artist_credit: String,
    /// The recording title. MusicBrainz often carries the mix in it
    /// (`The Music (Extended Mix)`), sometimes only in `disambiguation`.
    pub title: String,
    pub disambiguation: String,
    /// Artists with a MusicBrainz `remixer` relationship to the recording.
    /// Non-empty means the recording is a remix even when its title does
    /// not say so. Only a full lookup fills it: search results and AcoustID
    /// data carry no relationships.
    #[serde(default)]
    pub remixers: Vec<String>,
    pub length_ms: Option<u32>,
    pub isrcs: Vec<String>,
    pub genres: Vec<String>,
    pub releases: Vec<ReleaseInfo>,
    /// AcoustID's match score, 0..=1; `None` for a text-search candidate.
    pub acoustid_score: Option<f64>,
    pub source: CandidateSource,
}

/// How each factor scored, for the review screen and the evaluation CSV.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScoreBreakdown {
    /// `None` for a text-search candidate.
    pub acoustid: Option<f64>,
    pub duration: f64,
    pub text: f64,
    pub mix: f64,
    /// Why a hard rule rejected or capped the candidate, when one did.
    pub rejected: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scored {
    pub candidate: Candidate,
    /// 0..=1.
    pub score: f64,
    pub breakdown: ScoreBreakdown,
}

/// What may happen to a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    /// Score ≥ 0.92, only empty fields would change, and the user enabled
    /// auto-apply (off by default).
    AutoApply,
    /// A candidate worth showing; the user decides.
    Review,
    /// No candidate scored ≥ 0.5. Nothing is guessed.
    Unidentified,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(current: &str) -> FieldChange {
        FieldChange { field: Field::Label, current: current.to_owned(), proposed: "New".to_owned() }
    }

    fn art() -> ArtworkRef {
        ArtworkRef {
            source: ArtworkSource::CoverArtArchive,
            url: "https://coverartarchive.org/release/x/front-500".into(),
            sha256: "ab".into(),
            mime: "image/jpeg".into(),
            bytes: 2048,
        }
    }

    #[test]
    fn only_a_proposal_that_fills_empty_fields_may_auto_apply() {
        assert!(Proposal::new(vec![change(""), change("  ")], None, false).fills_only_empty());
        assert!(!Proposal::new(vec![change(""), change("Old")], None, false).fills_only_empty());
        assert!(!Proposal::default().fills_only_empty(), "nothing to change is not an empty-field fill");
    }

    #[test]
    fn artwork_is_one_change_and_counts_as_a_fill_only_for_a_file_without_any() {
        let only_art = Proposal::new(Vec::new(), Some(art()), false);
        assert_eq!(only_art.changes.len(), 1);
        assert_eq!(only_art.changes[0].field, Field::Artwork);
        assert_eq!(only_art.changes[0].current, Field::NO_ARTWORK);
        assert!(only_art.fills_only_empty());
        // With a filled field beside it, the proposal is not fill-only.
        assert!(!Proposal::new(vec![change("Old")], Some(art()), false).fills_only_empty());
        // A file that has artwork never gets any proposed, and a stray
        // Artwork change cannot slip in without the image.
        let stray = FieldChange { field: Field::Artwork, current: "none".into(), proposed: "x".into() };
        let has_art = Proposal::new(vec![stray], Some(art()), true);
        assert_eq!(has_art, Proposal::default());
    }
}
