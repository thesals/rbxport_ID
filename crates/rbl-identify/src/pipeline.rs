//! The per-track pipeline: read the file, verify its tags with a
//! MusicBrainz text search, fingerprint it and ask AcoustID, score every
//! candidate, resolve the winner's full details, and decide.
//!
//! Requests are bounded per track: one AcoustID lookup, one MusicBrainz
//! search, and a full MusicBrainz lookup (two requests) for each candidate
//! that scores at least the review threshold, at most
//! [`MAX_FULL_LOOKUPS`]. The full lookup is what reveals a remixer
//! relationship, so no candidate is offered for review without it. Those
//! below the threshold are not looked up: a remix flag can only lower a
//! score, so they could never rise above it.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::acoustid::{AcoustIdClient, AcoustIdMatch, AcoustIdRecording};
use crate::decide;
use crate::error::Result;
use crate::fingerprint;
use crate::mixname;
use crate::model::{Candidate, CandidateSource, Decision, ExistingTags, ReleaseInfo, Scored};
use crate::musicbrainz::MusicBrainzClient;
use crate::normalize;
use crate::score;
use crate::tags;

/// How many text-search results are scored.
const SEARCH_LIMIT: u32 = 5;
/// How many AcoustID recordings are scored, best AcoustID score first.
const MAX_FINGERPRINT_CANDIDATES: usize = 10;
/// How many candidates at or above the review threshold get the full
/// MusicBrainz lookup. Those beyond it are dropped, not offered unchecked.
pub const MAX_FULL_LOOKUPS: usize = 4;

#[derive(Debug, Clone, Copy, Default)]
pub struct Settings {
    /// Off by default, as the spec requires.
    pub auto_apply: bool,
}

/// The services a run talks to.
pub struct Services<'a> {
    pub acoustid: &'a AcoustIdClient,
    pub musicbrainz: &'a MusicBrainzClient,
}

/// What AcoustID said about the fingerprint, for the evaluation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct FingerprintOutcome {
    /// AcoustID results (fingerprint clusters) returned.
    pub matches: usize,
    /// Best AcoustID score among them, 0 when none.
    pub best_score: f64,
    /// Recordings linked to those results.
    pub recordings: usize,
}

/// Everything known about one track after identification.
#[derive(Debug, Clone, Serialize)]
pub struct TrackResult {
    pub path: PathBuf,
    pub existing: ExistingTags,
    pub duration_secs: f64,
    pub fingerprint: FingerprintOutcome,
    /// Ranked, best first.
    pub candidates: Vec<Scored>,
    pub decision: Decision,
    /// The tags the best candidate proposes, when one is worth reviewing.
    pub proposed: Option<ExistingTags>,
    /// Stage failures that did not stop the track (a service down, a
    /// fingerprint that could not be computed). Never contain a secret.
    pub warnings: Vec<String>,
}

impl TrackResult {
    #[must_use]
    pub fn best(&self) -> Option<&Scored> {
        self.candidates.first()
    }
}

/// Identifies one file. Only reading the file can fail the whole track;
/// every later stage degrades to a warning.
pub fn identify(path: &Path, services: &Services<'_>, settings: Settings) -> Result<TrackResult> {
    let info = tags::read(path)?;
    let existing = info.tags;
    let duration_secs = info.duration_secs;
    let mut warnings = Vec::new();
    let mut candidates = Vec::new();

    // 1. Pre-check: are the tags already right? Searched on the bare title,
    //    without the mix or a featuring clause, for recall; the mix is
    //    judged by scoring, not by the search.
    if !existing.artist.is_empty() && !existing.title.is_empty() {
        let base = mixname::parse_title(&existing.title).base;
        let (base, _) = normalize::strip_featuring(&base);
        match services.musicbrainz.search_recordings(&existing.artist, &base, SEARCH_LIMIT) {
            Ok(found) => candidates.extend(found),
            Err(e) => warnings.push(format!("MusicBrainz search: {e}")),
        }
    }

    // 2. Fingerprint and AcoustID.
    let mut outcome = FingerprintOutcome::default();
    match fingerprint::fingerprint_file(path) {
        Ok(print) => {
            let seconds = duration_secs.round().clamp(0.0, f64::from(u32::MAX));
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "rounded and clamped to the u32 range")]
            let seconds = seconds as u32;
            match services.acoustid.lookup(&print.encoded, seconds) {
                Ok(matches) => {
                    outcome = summarise(&matches);
                    candidates.extend(fingerprint_candidates(&matches));
                }
                Err(e) => warnings.push(format!("AcoustID: {e}")),
            }
        }
        Err(e) => warnings.push(format!("fingerprint: {e}")),
    }

    // 3. Score everything; 4. fully look up every candidate worth
    //    reviewing (remixers, labels, ISRCs) and score again. Only looked-up
    //    candidates are kept above the threshold: a candidate whose lookup
    //    failed or was beyond the limit is dropped rather than offered with
    //    its remix status unknown.
    let ranked = score::rank(&existing, duration_secs, candidates);
    let mut resolved = Vec::new();
    for scored in ranked.iter().filter(|s| s.score >= decide::REVIEW_THRESHOLD).take(MAX_FULL_LOOKUPS) {
        let c = &scored.candidate;
        match services.musicbrainz.lookup_recording(&c.recording_mbid, c.acoustid_score, c.source) {
            Ok(full) => resolved.push(full),
            Err(e) => warnings.push(format!("MusicBrainz lookup {}: {e}", c.recording_mbid)),
        }
    }
    let below: Vec<Candidate> =
        ranked.iter().filter(|s| s.score < decide::REVIEW_THRESHOLD).map(|s| s.candidate.clone()).collect();
    let ranked = score::rank(&existing, duration_secs, resolved.into_iter().chain(below).collect());

    // 5. Decide.
    let proposed = ranked.first().filter(|b| b.score >= decide::REVIEW_THRESHOLD).map(|b| propose(&existing, &b.candidate));
    let fills_only_empty = proposed.as_ref().is_some_and(|p| decide::fills_only_empty(&existing, p));
    let decision = decide::decide(ranked.first(), fills_only_empty, settings.auto_apply);
    Ok(TrackResult {
        path: path.to_owned(),
        existing,
        duration_secs,
        fingerprint: outcome,
        candidates: ranked,
        decision,
        proposed,
        warnings,
    })
}

fn summarise(matches: &[AcoustIdMatch]) -> FingerprintOutcome {
    FingerprintOutcome {
        matches: matches.len(),
        best_score: matches.iter().map(|m| m.score).fold(0.0, f64::max),
        recordings: matches.iter().map(|m| m.recordings.len()).sum(),
    }
}

/// Candidates from AcoustID's recording data, best AcoustID score first.
/// A recording reached through several results keeps its best score.
fn fingerprint_candidates(matches: &[AcoustIdMatch]) -> Vec<Candidate> {
    let mut found: Vec<(f64, &AcoustIdRecording)> =
        matches.iter().flat_map(|m| m.recordings.iter().map(move |r| (m.score, r))).collect();
    found.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.mbid.cmp(&b.1.mbid)));
    let mut seen = std::collections::HashSet::new();
    found
        .into_iter()
        .filter(|(_, r)| !r.mbid.is_empty() && seen.insert(r.mbid.clone()))
        .take(MAX_FINGERPRINT_CANDIDATES)
        .map(|(score, r)| from_acoustid(score, r))
        .collect()
}

fn from_acoustid(score: f64, recording: &AcoustIdRecording) -> Candidate {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "a track length in ms fits u32; negative or NaN durations are dropped")]
    let length_ms = recording.duration_secs.filter(|d| d.is_finite() && *d > 0.0).map(|d| (d * 1000.0).round() as u32);
    Candidate {
        recording_mbid: recording.mbid.clone(),
        artists: recording.artists.clone(),
        artist_credit: recording.artist_credit.clone(),
        title: recording.title.clone(),
        disambiguation: String::new(),
        // AcoustID carries no relationships; the full lookup fills these.
        remixers: Vec::new(),
        length_ms,
        isrcs: Vec::new(),
        genres: Vec::new(),
        releases: recording
            .release_groups
            .iter()
            .map(|g| ReleaseInfo {
                title: g.title.clone(),
                release_group_mbid: g.mbid.clone(),
                release_group_type: g.kind.clone(),
                secondary_types: g.secondary_types.clone(),
                ..ReleaseInfo::default()
            })
            .collect(),
        acoustid_score: Some(score),
        source: CandidateSource::Fingerprint,
    }
}

/// The tags a candidate proposes. Only fields the candidate knows are
/// proposed; the rest keep the file's values. Genre is left alone in
/// Phase 1 (Discogs styles fill it in Phase 2), and key, BPM and analysis
/// are never touched by identification at all.
#[must_use]
pub fn propose(existing: &ExistingTags, candidate: &Candidate) -> ExistingTags {
    let mut proposed = existing.clone();
    if !candidate.artist_credit.is_empty() {
        proposed.artist.clone_from(&candidate.artist_credit);
    }
    if !candidate.title.is_empty() {
        proposed.title = proposed_title(existing, candidate);
    }
    if let Some(release) = preferred_release(&candidate.releases) {
        if !release.title.is_empty() {
            proposed.album.clone_from(&release.title);
        }
        if !release.label.is_empty() {
            proposed.label.clone_from(&release.label);
        }
        if let Some(year) = release.date.get(..4).and_then(|y| y.parse().ok()) {
            proposed.year = Some(year);
        }
    }
    if let Some(isrc) = candidate.isrcs.first() {
        proposed.isrc.clone_from(isrc);
    }
    proposed
}

/// `Title (Mix)`, the mix taken as scoring saw it (remixers included):
/// - the same mix → the file's own mix text, so a review never proposes
///   only a change of spelling or case;
/// - the candidate names no mix → the file's mix text (a plain MusicBrainz
///   title must not strip "(Aerofoil Remix)" or "(Radio Edit)");
/// - the candidate names a different, compatible mix → the candidate's, in
///   store casing ("(extended mix)" becomes "(Extended Mix)").
///
/// A conflicting candidate is capped below review and never proposed.
fn proposed_title(existing: &ExistingTags, candidate: &Candidate) -> String {
    let theirs = score::candidate_mix(candidate);
    let ours = mixname::parse_title(&existing.title);
    let agreement = mixname::agree(&ours.mix, &theirs.mix);
    let mix_text = if agreement == mixname::MixAgreement::Same || theirs.mix_text.is_empty() {
        ours.mix_text
    } else {
        store_case(&theirs.mix_text)
    };
    mixname::format_title(&theirs.base, &mix_text)
}

/// Store casing for a mix name: the generic words capitalised ("Extended
/// Mix", "Radio Edit", "VIP"), a remixer's name kept as written.
fn store_case(mix_text: &str) -> String {
    const GENERIC: [&str; 22] = [
        "original", "extended", "radio", "club", "dub", "instrumental", "mix", "remix", "edit", "version", "rework",
        "bootleg", "refix", "flip", "remake", "reconstruction", "interpretation", "redux", "album", "main", "vocal", "re-edit",
    ];
    mix_text
        .split(' ')
        .map(|word| {
            let lower = word.to_lowercase();
            if lower == "vip" {
                "VIP".to_owned()
            } else if GENERIC.contains(&lower.as_str()) {
                let mut chars = lower.chars();
                chars.next().map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
            } else {
                word.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The release a proposal takes album, label and year from: an original
/// release (not a compilation or DJ mix) when there is one, then the
/// earliest dated, then the first listed.
fn preferred_release(releases: &[ReleaseInfo]) -> Option<&ReleaseInfo> {
    let is_compilation = |r: &ReleaseInfo| {
        r.secondary_types.iter().any(|t| t.eq_ignore_ascii_case("compilation") || t.eq_ignore_ascii_case("dj-mix"))
    };
    releases.iter().enumerate().min_by(|(ia, a), (ib, b)| {
        is_compilation(a)
            .cmp(&is_compilation(b))
            .then_with(|| a.date.is_empty().cmp(&b.date.is_empty()))
            .then_with(|| a.date.cmp(&b.date))
            .then_with(|| ia.cmp(ib))
    })
    .map(|(_, r)| r)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn release(title: &str, date: &str, secondary: &[&str]) -> ReleaseInfo {
        ReleaseInfo {
            title: title.into(),
            date: date.into(),
            secondary_types: secondary.iter().map(|s| (*s).to_owned()).collect(),
            ..ReleaseInfo::default()
        }
    }

    fn candidate(title: &str, disambiguation: &str, remixers: &[&str]) -> Candidate {
        Candidate {
            recording_mbid: "m".into(),
            artists: vec!["E&G".into()],
            artist_credit: "E&G".into(),
            title: title.into(),
            disambiguation: disambiguation.into(),
            remixers: remixers.iter().map(|r| (*r).to_owned()).collect(),
            length_ms: None,
            isrcs: Vec::new(),
            genres: Vec::new(),
            releases: Vec::new(),
            acoustid_score: Some(1.0),
            source: CandidateSource::Fingerprint,
        }
    }

    fn file(title: &str) -> ExistingTags {
        ExistingTags { title: title.into(), ..ExistingTags::default() }
    }

    #[test]
    fn a_plain_titled_remix_keeps_the_files_remix_name() {
        let title = proposed_title(&file("Spirit (Aerofoil Remix)"), &candidate("Spirit", "", &["Aerofoil"]));
        assert_eq!(title, "Spirit (Aerofoil Remix)");
    }

    #[test]
    fn a_plain_musicbrainz_title_does_not_strip_a_radio_edit() {
        assert_eq!(proposed_title(&file("Song (Radio Edit)"), &candidate("Song", "", &[])), "Song (Radio Edit)");
        assert_eq!(proposed_title(&file("Song (Extended Mix)"), &candidate("Song", "", &[])), "Song (Extended Mix)");
    }

    #[test]
    fn the_same_mix_keeps_the_files_spelling_and_a_new_one_is_store_cased() {
        assert_eq!(proposed_title(&file("The Music (Extended Mix)"), &candidate("The Music (extended mix)", "", &[])), "The Music (Extended Mix)");
        assert_eq!(proposed_title(&file("The Music"), &candidate("The Music (extended mix)", "", &[])), "The Music (Extended Mix)");
        assert_eq!(store_case("Aerofoil remix"), "Aerofoil Remix");
        assert_eq!(store_case("DJ Shog vip"), "DJ Shog VIP");
        assert_eq!(store_case("deadmau5 edit"), "deadmau5 Edit");
    }

    #[test]
    fn the_original_release_beats_an_earlier_compilation() {
        let releases =
            [release("Best Of 2014", "2014-01-01", &["Compilation"]), release("The Music", "2014-06-01", &[]), release("Later", "2015", &[])];
        assert_eq!(preferred_release(&releases).unwrap().title, "The Music");
        assert_eq!(preferred_release(&[release("Undated", "", &[]), release("Dated", "2010", &[])]).unwrap().title, "Dated");
        assert!(preferred_release(&[]).is_none());
    }
}
