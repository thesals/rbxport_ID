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
