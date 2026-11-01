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
use crate::model::{
    ArtworkRef, Candidate, CandidateSource, Decision, Enrichment, ExistingTags, Field, FieldChange, Proposal, ReleaseInfo,
    Scored, TagSnapshot,
};
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
    pub genre: GenrePolicy,
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
    /// The file's exact original values, for the store and undo.
    pub snapshot: TagSnapshot,
    /// What the best candidate would change, field by field, when one is
    /// worth reviewing.
    pub proposal: Option<Proposal>,
    /// What Discogs added to the best candidate, when it matched.
    pub enrichment: Option<Enrichment>,
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

    // 5. Propose and decide. (Discogs enrichment and artwork join here.)
    let enrichment: Option<Enrichment> = None;
    let proposal = ranked
        .first()
        .filter(|b| b.score >= decide::REVIEW_THRESHOLD)
        .map(|b| propose(&existing, &info.snapshot, &b.candidate, enrichment.as_ref(), None, settings.genre));
    let fills_only_empty = proposal.as_ref().is_some_and(Proposal::fills_only_empty);
    let decision = decide::decide(ranked.first(), fills_only_empty, settings.auto_apply);
    Ok(TrackResult {
        path: path.to_owned(),
        existing,
        snapshot: info.snapshot,
        duration_secs,
        fingerprint: outcome,
        candidates: ranked,
        decision,
        proposal,
        enrichment,
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

/// How Discogs styles reach the Genre tag. Key, BPM and analysis are never
/// touched by identification whatever the policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum GenrePolicy {
    /// The spec's default: styles joined with "; " fill an empty Genre; a
    /// genre the user set is left alone.
    #[default]
    FillEmpty,
    /// Styles replace whatever Genre holds.
    Replace,
    /// Genre is never proposed.
    Never,
}

/// What a candidate (and Discogs, when it matched) would change, field by
/// field, against the file's exact current values. Only fields that differ
/// are listed. Artwork is proposed only for a file without any.
///
/// Album, label, catalog number, year and the release IDs all come from
/// one release, never mixed: the MusicBrainz release whose title matches
/// the file's album (so a track bought from a compilation keeps that
/// compilation), else the preferred original release. Discogs fills only
/// the release fields MusicBrainz left empty, and its styles and release
/// ID count only when its release agrees with the chosen one (same catalog
/// number, or same title and year); with no MusicBrainz release, Discogs's
/// is the release.
///
/// When the MusicBrainz release ID changes, Picard's release-bound IDs
/// already in the file (release track, album artist) are proposed for
/// clearing, so the file never carries IDs that contradict each other.
#[must_use]
pub fn propose(
    existing: &ExistingTags,
    snapshot: &TagSnapshot,
    candidate: &Candidate,
    enrichment: Option<&Enrichment>,
    artwork: Option<ArtworkRef>,
    genre: GenrePolicy,
) -> Proposal {
    let release = chosen_release(&current(snapshot, Field::Album), &candidate.releases);
    let discogs = enrichment.filter(|e| release.is_none_or(|r| same_release(r, e)));
    let release_text = |pick: fn(&ReleaseInfo) -> &str| release.map(pick).filter(|v| !v.is_empty()).map(str::to_owned);
    let discogs_text = |pick: fn(&Enrichment) -> &str| discogs.map(pick).filter(|v| !v.is_empty()).map(str::to_owned);
    let year = release
        .and_then(|r| r.date.get(..4))
        .map(str::to_owned)
        .or_else(|| discogs.and_then(|e| e.year).map(|y| y.to_string()));
    let styles = discogs.map(|e| e.styles.join("; ")).filter(|s| !s.is_empty());
    let genre = match genre {
        GenrePolicy::FillEmpty => styles.filter(|_| current(snapshot, Field::Genre).trim().is_empty()),
        GenrePolicy::Replace => styles,
        GenrePolicy::Never => None,
    };

    let mut wanted: Vec<(Field, Option<String>)> = vec![
        (Field::Artist, Some(candidate.artist_credit.clone()).filter(|v| !v.is_empty())),
        (Field::Title, Some(proposed_title(existing, candidate)).filter(|v| !v.is_empty())),
        (Field::Album, release_text(|r| &r.title).or_else(|| discogs_text(|e| &e.release_title))),
        (Field::Label, release_text(|r| &r.label).or_else(|| discogs_text(|e| &e.label))),
        (Field::CatalogNumber, release_text(|r| &r.catalog_number).or_else(|| discogs_text(|e| &e.catalog_number))),
        (Field::Year, year),
        (Field::Isrc, candidate.isrcs.first().cloned()),
        (Field::Genre, genre),
        (Field::MbRecordingId, Some(candidate.recording_mbid.clone()).filter(|v| !v.is_empty())),
        (Field::MbReleaseId, release_text(|r| &r.mbid)),
        (Field::MbReleaseGroupId, release_text(|r| &r.release_group_mbid)),
        (Field::DiscogsReleaseId, discogs.and_then(|e| e.discogs_release_id).map(|id| id.to_string())),
    ];
    let new_release = wanted.iter().find(|(f, _)| *f == Field::MbReleaseId).and_then(|(_, v)| v.clone());
    if new_release.is_some_and(|new| new != current(snapshot, Field::MbReleaseId)) {
        wanted.push((Field::MbReleaseTrackId, Some(String::new())));
        wanted.push((Field::MbAlbumArtistId, Some(String::new())));
    }

    let changes = wanted
        .into_iter()
        .filter_map(|(field, proposed)| {
            let proposed = proposed?;
            let current = current(snapshot, field);
            (!same_value(field, &current, &proposed)).then_some(FieldChange { field, current, proposed })
        })
        .collect();
    Proposal::new(changes, artwork, snapshot.has_artwork)
}

/// The release a proposal takes every release field from: the one whose
/// title matches the file's current album, else the preferred original.
fn chosen_release<'a>(current_album: &str, releases: &'a [ReleaseInfo]) -> Option<&'a ReleaseInfo> {
    let album = current_album.trim();
    if !album.is_empty() {
        if let Some(matching) = releases
            .iter()
            .filter(|r| !r.title.is_empty())
            .map(|r| (normalize::similarity(album, &r.title), r))
            .filter(|(s, _)| *s >= ALBUM_MATCH)
            .max_by(|a, b| a.0.total_cmp(&b.0))
        {
            return Some(matching.1);
        }
    }
    preferred_release(releases)
}

/// How close a release title must be to the file's album to be "the
/// release this file came from".
const ALBUM_MATCH: f64 = 0.85;

/// Whether Discogs matched the same release MusicBrainz chose: the same
/// catalog number (case, spaces and dashes aside), or the same title and
/// year.
fn same_release(release: &ReleaseInfo, discogs: &Enrichment) -> bool {
    let catno = |c: &str| c.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_ascii_lowercase();
    let (ours, theirs) = (catno(&release.catalog_number), catno(&discogs.catalog_number));
    if !ours.is_empty() && !theirs.is_empty() {
        return ours == theirs;
    }
    let year = release.date.get(..4).and_then(|y| y.parse::<u16>().ok());
    normalize::similarity(&release.title, &discogs.release_title) >= ALBUM_MATCH && year.is_some() && year == discogs.year
}

/// A field's current value as the review screen shows it: every value of
/// every frame holding it, joined with "; ". Empty when absent.
fn current(snapshot: &TagSnapshot, field: Field) -> String {
    snapshot
        .values
        .get(&field)
        .map(|frames| frames.iter().flat_map(|f| f.values.iter().map(String::as_str)).collect::<Vec<_>>().join("; "))
        .unwrap_or_default()
}

/// Whether a proposal would change nothing that matters: whitespace aside,
/// and for Year only the year itself (`2010-00-00` is 2010). Clearing an
/// already absent field is no change either.
fn same_value(field: Field, current: &str, proposed: &str) -> bool {
    let (current, proposed) = (current.trim(), proposed.trim());
    match field {
        Field::Year => current.get(..4).unwrap_or(current) == proposed.get(..4).unwrap_or(proposed),
        _ => current == proposed,
    }
}

/// `Title (Mix)`, the mix taken as scoring saw it (remixers included):
/// - the same or a compatible mix the file already names ("Original Mix"
///   vs "extended mix") → the file's own text, so a review never proposes
///   only a respelling or an Original/Extended rename;
/// - the candidate names no mix → the file's mix text (a plain MusicBrainz
///   title must not strip "(Aerofoil Remix)" or "(Radio Edit)");
/// - otherwise (the file names none, or the pair is unknown) → the
///   candidate's, in store casing ("(extended mix)" → "(Extended Mix)").
///
/// A conflicting candidate is capped below review and never proposed.
fn proposed_title(existing: &ExistingTags, candidate: &Candidate) -> String {
    let theirs = score::candidate_mix(candidate);
    let ours = mixname::parse_title(&existing.title);
    let agreement = mixname::agree(&ours.mix, &theirs.mix);
    let file_names_it = !ours.mix_text.is_empty()
        && matches!(agreement, mixname::MixAgreement::Same | mixname::MixAgreement::Compatible);
    let mix_text = if file_names_it || theirs.mix_text.is_empty() { ours.mix_text } else { store_case(&theirs.mix_text) };
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

    fn snapshot(fields: &[(Field, &str, &str)]) -> TagSnapshot {
        let mut snap = TagSnapshot::default();
        for (field, frame, value) in fields {
            snap.values.entry(*field).or_default().push(crate::model::FrameValues {
                frame: (*frame).to_owned(),
                values: vec![(*value).to_owned()],
            });
        }
        snap
    }

    fn with_release(mut c: Candidate, mbid: &str, date: &str) -> Candidate {
        c.releases = vec![ReleaseInfo { mbid: mbid.into(), title: "Album".into(), date: date.into(), ..ReleaseInfo::default() }];
        c
    }

    fn change_for(proposal: &Proposal, field: Field) -> Option<&FieldChange> {
        proposal.changes.iter().find(|c| c.field == field)
    }

    #[test]
    fn an_unparseable_present_year_is_never_an_empty_fill() {
        // What tags::read records for the test set's TDRC "2010-00-00".
        let snap = snapshot(&[(Field::Year, "TDRC", "2010-00-00")]);
        let same = propose(&file(""), &snap, &with_release(candidate("x", "", &[]), "", "2010"), None, None, GenrePolicy::FillEmpty);
        assert_eq!(change_for(&same, Field::Year), None, "the same year is no change");
        let other = propose(&file(""), &snap, &with_release(candidate("x", "", &[]), "", "2011"), None, None, GenrePolicy::FillEmpty);
        assert_eq!(change_for(&other, Field::Year).unwrap().current, "2010-00-00");
        assert!(!other.fills_only_empty());
    }

    #[test]
    fn a_new_release_clears_stale_release_bound_picard_ids() {
        let snap = snapshot(&[
            (Field::MbReleaseId, "TXXX:MusicBrainz Album Id", "old-release"),
            (Field::MbReleaseTrackId, "TXXX:MusicBrainz Release Track Id", "old-track"),
        ]);
        let proposal =
            propose(&file(""), &snap, &with_release(candidate("x", "", &[]), "new-release", ""), None, None, GenrePolicy::FillEmpty);
        let cleared = change_for(&proposal, Field::MbReleaseTrackId).unwrap();
        assert_eq!((cleared.current.as_str(), cleared.proposed.as_str()), ("old-track", ""));
        assert_eq!(change_for(&proposal, Field::MbAlbumArtistId), None, "an absent ID needs no clearing");
        // The same release: nothing to clear.
        let same =
            propose(&file(""), &snap, &with_release(candidate("x", "", &[]), "old-release", ""), None, None, GenrePolicy::FillEmpty);
        assert_eq!(change_for(&same, Field::MbReleaseTrackId), None);
    }

    #[test]
    fn styles_fill_only_an_empty_genre_by_default() {
        let enrichment = Enrichment { styles: vec!["Tech House".into(), "Minimal".into()], ..Enrichment::default() };
        let c = candidate("x", "", &[]);
        let set = ExistingTags { genre: "House".into(), ..ExistingTags::default() };
        let kept = propose(&set, &snapshot(&[(Field::Genre, "TCON", "House")]), &c, Some(&enrichment), None, GenrePolicy::FillEmpty);
        assert_eq!(change_for(&kept, Field::Genre), None);
        let filled = propose(&file(""), &TagSnapshot::default(), &c, Some(&enrichment), None, GenrePolicy::FillEmpty);
        assert_eq!(change_for(&filled, Field::Genre).unwrap().proposed, "Tech House; Minimal");
        let replaced = propose(&set, &snapshot(&[(Field::Genre, "TCON", "House")]), &c, Some(&enrichment), None, GenrePolicy::Replace);
        assert_eq!(change_for(&replaced, Field::Genre).unwrap().proposed, "Tech House; Minimal");
    }

    #[test]
    fn discogs_label_and_catalog_number_win_and_unchanged_fields_are_left_out() {
        let enrichment = Enrichment {
            discogs_release_id: Some(42),
            label: "Fraction Records".into(),
            catalog_number: "FRA224".into(),
            ..Enrichment::default()
        };
        let snap = snapshot(&[(Field::Artist, "TPE1", "E&G"), (Field::CatalogNumber, "TXXX:Catalog #", "")]);
        let proposal = propose(&file(""), &snap, &candidate("Spirit", "", &[]), Some(&enrichment), None, GenrePolicy::FillEmpty);
        assert_eq!(change_for(&proposal, Field::Artist), None, "the same artist is not a change");
        assert_eq!(change_for(&proposal, Field::Label).unwrap().proposed, "Fraction Records");
        assert_eq!(change_for(&proposal, Field::CatalogNumber).unwrap().proposed, "FRA224");
        assert_eq!(change_for(&proposal, Field::DiscogsReleaseId).unwrap().proposed, "42");
    }

    fn full_release(mbid: &str, title: &str, date: &str, label: &str, catno: &str, secondary: &[&str]) -> ReleaseInfo {
        ReleaseInfo {
            mbid: mbid.into(),
            title: title.into(),
            date: date.into(),
            label: label.into(),
            catalog_number: catno.into(),
            secondary_types: secondary.iter().map(|s| (*s).to_owned()).collect(),
            ..ReleaseInfo::default()
        }
    }

    #[test]
    fn a_compilation_rip_keeps_its_compilation_release() {
        let mut c = candidate("Last Ones Standing (TC remix)", "", &[]);
        c.releases = vec![
            full_release("single", "Last Ones Standing", "2010-08-01", "Ministry of Sound", "MOS-1", &[]),
            full_release("annual", "MOS The Annual 2011", "2010-11-22", "Ministry Of Sound (UK)", "ANCD2K10E", &["Compilation"]),
        ];
        let snap = snapshot(&[
            (Field::Album, "TALB", "MOS The Annual 2011"),
            (Field::Label, "TPUB", "Ministry Of Sound (UK)"),
            (Field::CatalogNumber, "TXXX:Catalog #", "ANCD2K10E"),
        ]);
        let proposal = propose(&file(""), &snap, &c, None, None, GenrePolicy::FillEmpty);
        for field in [Field::Album, Field::Label, Field::CatalogNumber] {
            assert_eq!(change_for(&proposal, field), None, "{field:?} stays the compilation's");
        }
        assert_eq!(change_for(&proposal, Field::MbReleaseId).unwrap().proposed, "annual");
    }

    #[test]
    fn discogs_and_musicbrainz_release_fields_never_mix() {
        let mut c = candidate("x", "", &[]);
        c.releases = vec![full_release("mb", "Original EP", "2014-06-01", "", "", &[])];
        let other = Enrichment {
            discogs_release_id: Some(7),
            release_title: "Some Compilation".into(),
            label: "Other Label".into(),
            catalog_number: "OTH-1".into(),
            year: Some(2016),
            styles: vec!["Techno".into()],
            ..Enrichment::default()
        };
        let proposal = propose(&file(""), &TagSnapshot::default(), &c, Some(&other), None, GenrePolicy::FillEmpty);
        assert_eq!(change_for(&proposal, Field::Album).unwrap().proposed, "Original EP");
        assert_eq!(change_for(&proposal, Field::Year).unwrap().proposed, "2014");
        for field in [Field::Label, Field::CatalogNumber, Field::DiscogsReleaseId, Field::Genre] {
            assert_eq!(change_for(&proposal, field), None, "{field:?} must not come from another release");
        }
        // The same release on Discogs (title and year agree) fills the gaps.
        let same = Enrichment { release_title: "Original EP".into(), year: Some(2014), ..other };
        let proposal = propose(&file(""), &TagSnapshot::default(), &c, Some(&same), None, GenrePolicy::FillEmpty);
        assert_eq!(change_for(&proposal, Field::Label).unwrap().proposed, "Other Label");
        assert_eq!(change_for(&proposal, Field::DiscogsReleaseId).unwrap().proposed, "7");
    }

    #[test]
    fn an_unreadable_genre_is_not_filled_over() {
        let enrichment = Enrichment { styles: vec!["Techno".into()], ..Enrichment::default() };
        let snap = snapshot(&[(Field::Genre, "TCON", crate::tags::UNREADABLE)]);
        let proposal = propose(&file(""), &snap, &candidate("x", "", &[]), Some(&enrichment), None, GenrePolicy::FillEmpty);
        assert_eq!(change_for(&proposal, Field::Genre), None);
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
        // Compatible, and the file already names its mix: no rename churn.
        assert_eq!(proposed_title(&file("Ilya (Original Mix)"), &candidate("Ilya (extended mix)", "", &[])), "Ilya (Original Mix)");
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
