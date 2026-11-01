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
    DiscogsReleaseId,
    Artwork,
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    pub changes: Vec<FieldChange>,
    pub artwork: Option<ArtworkRef>,
}

impl Proposal {
    /// True when every change fills a field that is empty now: the only
    /// proposals auto-apply may ever take. An empty proposal changes nothing
    /// and is not "only empty fields".
    #[must_use]
    pub fn fills_only_empty(&self) -> bool {
        !self.changes.is_empty() && self.changes.iter().all(|c| c.current.trim().is_empty())
    }
}

/// The exact original values of every [`Field`], taken before anything is
/// written so a change can be undone. Unlike [`ExistingTags`] nothing is
/// trimmed or reduced: a field's values are stored as the tag holds them
/// (all of a multi-value frame, a full `2014-06-01` date), and a field the
/// file does not have is simply missing from `values`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagSnapshot {
    /// The tag format the values were read from: `ID3v2.3`, `ID3v2.4`,
    /// `VorbisComments`, `MP4ilst`, ...
    pub format: String,
    pub values: std::collections::BTreeMap<Field, Vec<String>>,
    pub has_artwork: bool,
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

    #[test]
    fn only_a_proposal_that_fills_empty_fields_may_auto_apply() {
        assert!(Proposal { changes: vec![change(""), change("  ")], artwork: None }.fills_only_empty());
        assert!(!Proposal { changes: vec![change(""), change("Old")], artwork: None }.fills_only_empty());
        assert!(!Proposal::default().fills_only_empty(), "nothing to change is not an empty-field fill");
    }
}
