//! The Discogs fallback for a track nothing else identified: no
//! fingerprint or MusicBrainz candidate reached the review threshold, so
//! Discogs is searched by the file's own artist and title.
//!
//! This only ever confirms what the tags already say and fills release
//! fields they leave empty. The candidate is capped at
//! [`DISCOGS_CAP`] (well below auto-apply), the decision is always review,
//! and the proposal ([`propose`]) never touches title, artist or genre and
//! carries no MusicBrainz ID.
//!
//! Requests per track: at most [`MAX_SEARCHES`] searches (narrowed by the
//! file's label and catalog number when it has them, then without them,
//! then by the first credited artist alone) and [`MAX_RELEASES`] release
//! lookups, all under the Discogs limiter.
//!
//! A release counts only if its tracklist holds a track with our base title
//! and an agreeing mix (a remix is not the original, a radio edit not the
//! extended mix) and, when Discogs lists the track's duration, within
//! [`MAX_DURATION_DELTA_SECS`] of the file. Releases whose label or catalog
//! number agree with the file's are tried first; a match on a release from
//! another label is scored lower.

use crate::discogs::{DiscogsClient, DiscogsHit, DiscogsQuery, DiscogsRelease, DiscogsTrack};
use crate::error::Result;
use crate::mixname::{self, MixAgreement, ParsedTitle};
use crate::model::{Candidate, CandidateSource, Enrichment, ExistingTags, Field, FieldChange, Proposal, ReleaseInfo, Scored, TagSnapshot};
use crate::normalize;
use crate::pipeline;
use crate::score;

/// How many releases are opened per track.
pub const MAX_RELEASES: usize = 2;
/// How many searches are made per track.
pub const MAX_SEARCHES: usize = 3;
/// How far a duration Discogs lists may be from the file's.
pub const MAX_DURATION_DELTA_SECS: f64 = 3.0;
/// The ceiling of a fallback candidate: reviewable, never near the 0.92
/// auto-apply threshold.
pub const DISCOGS_CAP: f64 = 0.85;
/// The ceiling when Discogs lists no duration for the track, so only the
/// text vouches for it.
pub const NO_DURATION_CAP: f64 = 0.75;
/// The ceiling when the file's label (or catalog number) is not on the
/// release: perhaps the same track, but another release than the file's.
pub const LABEL_DIFFERS_CAP: f64 = 0.65;
/// Below the review threshold: a match without title, artist and mix all
/// agreeing is never proposed.
const DISAGREEMENT_CAP: f64 = 0.49;
const ARTIST_MATCH: f64 = 0.7;
const TITLE_MATCH: f64 = 0.85;
const LABEL_MATCH: f64 = 0.85;
const SEARCH_PAGE: u32 = 10;

/// What the fallback found for a track.
#[derive(Debug, Clone, PartialEq)]
pub struct FallbackMatch {
    /// A [`CandidateSource::DiscogsText`] candidate, scored.
    pub scored: Scored,
    /// The release's label, catalog number, year, styles and ID.
    pub enrichment: Enrichment,
    /// Whether the file's label or catalog number is on the release; `None`
    /// when the file has neither.
    pub label_agrees: Option<bool>,
}

/// Searches Discogs for the file's own artist and title and returns the
/// best release match, scored. `None` when the file lacks an artist or
/// title, nothing was found, or no opened release holds the track in the
/// same mix (and duration).
pub fn find(discogs: &DiscogsClient, existing: &ExistingTags, duration_secs: f64) -> Result<Option<FallbackMatch>> {
    let ours = mixname::parse_title(&existing.title);
    let (track, _) = normalize::strip_featuring(&ours.base);
    let artist = existing.artist.trim();
    if artist.is_empty() || track.is_empty() {
        return Ok(None);
    }
    let hits = search(discogs, existing, artist, &track)?;
    let mut kept: Option<FallbackMatch> = None;
    for hit in ranked_hits(hits, existing).iter().take(MAX_RELEASES) {
        let release = discogs.release(hit.release_id)?;
        let Some(found) = match_release(&release, existing, &ours, duration_secs) else { continue };
        if found.label_agrees != Some(false) {
            // As good as it gets; no need to open another.
            return Ok(Some(found));
        }
        if kept.as_ref().is_none_or(|k| found.scored.score > k.scored.score) {
            kept = Some(found);
        }
    }
    Ok(kept)
}

/// The searches, broadest last, until one finds something: narrowed by the
/// file's label and catalog number, then without them, then with only the
/// first credited artist (a multi-artist credit is often listed under one
/// name).
fn search(discogs: &DiscogsClient, existing: &ExistingTags, artist: &str, track: &str) -> Result<Vec<DiscogsHit>> {
    let known = |v: &str| Some(v.trim().to_owned()).filter(|v| !v.is_empty());
    let narrowed = DiscogsQuery {
        artist: artist.to_owned(),
        track: track.to_owned(),
        label: known(&existing.label),
        catno: known(&existing.catalog_number),
        per_page: SEARCH_PAGE,
    };
    let mut queries = Vec::with_capacity(MAX_SEARCHES);
    let broad = DiscogsQuery { label: None, catno: None, ..narrowed.clone() };
    if narrowed.label.is_some() || narrowed.catno.is_some() {
        queries.push(narrowed);
    }
    let first_artist = first_artist(artist);
    let single = first_artist.filter(|a| !a.eq_ignore_ascii_case(artist)).map(|a| DiscogsQuery { artist: a, ..broad.clone() });
    queries.push(broad);
    queries.extend(single);
    for query in queries.iter().take(MAX_SEARCHES) {
        let hits = discogs.search(query)?;
        if !hits.is_empty() {
            return Ok(hits);
        }
    }
    Ok(Vec::new())
}

/// The first name of a credit as written: `A, B`, `A & B`, `A feat. B`,
/// `A vs B` → `A`. A name with `&` in it (`E&G`) has no spaces around it
/// and stays whole.
fn first_artist(credit: &str) -> Option<String> {
    let lower = credit.to_lowercase();
    let cut = [",", ";", " & ", " and ", " x ", " vs ", " vs. ", " feat", " ft.", " featuring "]
        .iter()
        .filter_map(|sep| lower.find(sep))
        .min()?;
    // `find` on the lowercased text: the byte offsets match only for ASCII.
    let first = credit.get(..cut).filter(|_| lower.len() == credit.len())?.trim();
    (!first.is_empty()).then(|| first.to_owned())
}

/// The hits in the order they are opened: catalog number agreeing with the
/// file's first, then label agreeing, then as Discogs ranked them.
fn ranked_hits(mut hits: Vec<DiscogsHit>, existing: &ExistingTags) -> Vec<DiscogsHit> {
    let catno = squash(&existing.catalog_number);
    let label = existing.label.trim();
    hits.sort_by_key(|h| {
        let catno_agrees = !catno.is_empty() && squash(&h.catno) == catno;
        let label_agrees = !label.is_empty() && h.labels.iter().any(|l| same_label(l, label));
        (!catno_agrees, !label_agrees)
    });
    hits
}

/// Catalog numbers compared without case, spaces or dashes.
fn squash(catno: &str) -> String {
    catno.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_ascii_lowercase()
}

/// Two label names for the same label: close as text, or one the other
/// with a suffix (`Ministry Of Sound` and `Ministry Of Sound (UK)`).
#[must_use]
pub fn same_label(a: &str, b: &str) -> bool {
    let (na, nb) = (normalize::normalize(a), normalize::normalize(b));
    if na.is_empty() || nb.is_empty() {
        return false;
    }
    na.starts_with(&nb) || nb.starts_with(&na) || normalize::similarity(&na, &nb) >= LABEL_MATCH
}

/// Our track on one release, scored: the matching track whose duration is
/// closest to the file's, any listed duration within 3 s.
fn match_release(release: &DiscogsRelease, existing: &ExistingTags, ours: &ParsedTitle, duration_secs: f64) -> Option<FallbackMatch> {
    // An unknown file duration compares with nothing: as if none were listed.
    let file_known = duration_secs.is_finite() && duration_secs > 0.0;
    let delta = |t: &DiscogsTrack| t.duration_secs.filter(|_| file_known).map(|d| (f64::from(d) - duration_secs).abs());
    let track = release
        .tracklist
        .iter()
        .filter(|t| pipeline::track_matches(t, ours))
        .filter(|t| delta(t).is_none_or(|d| d <= MAX_DURATION_DELTA_SECS))
        // Tracks with a duration (closest first) before tracks without one.
        .min_by(|a, b| delta(a).unwrap_or(f64::INFINITY).total_cmp(&delta(b).unwrap_or(f64::INFINITY)))?;
    let (enrichment, _) = pipeline::enrichment_from(release);
    let label_agrees = label_agreement(release, existing);
    let candidate = candidate(release, track, &enrichment);
    let scored = score_match(existing, duration_secs, candidate, delta(track).is_some(), label_agrees);
    Some(FallbackMatch { scored, enrichment, label_agrees })
}

/// Whether the file's label or catalog number is on the release; `None`
/// when the file has neither.
fn label_agreement(release: &DiscogsRelease, existing: &ExistingTags) -> Option<bool> {
    let (label, catno) = (existing.label.trim(), squash(&existing.catalog_number));
    if label.is_empty() && catno.is_empty() {
        return None;
    }
    Some(release.labels.iter().any(|l| {
        (!label.is_empty() && same_label(&l.name, label)) || (!catno.is_empty() && squash(&l.catno) == catno)
    }))
}

/// The Discogs track as a candidate: its own artists (a compilation's) or
/// the release's, its remixers, and the release as its one release.
fn candidate(release: &DiscogsRelease, track: &DiscogsTrack, enrichment: &Enrichment) -> Candidate {
    let artists = if track.artists.is_empty() { release.artists.clone() } else { track.artists.clone() };
    Candidate {
        recording_mbid: String::new(),
        artist_credit: artists.join(", "),
        artists,
        title: track.title.clone(),
        disambiguation: String::new(),
        remixers: pipeline::discogs_remixers(track),
        length_ms: track.duration_secs.map(|d| d.saturating_mul(1000)),
        isrcs: Vec::new(),
        genres: Vec::new(),
        releases: vec![ReleaseInfo {
            title: release.title.clone(),
            date: enrichment.year.map(|y| y.to_string()).unwrap_or_default(),
            label: enrichment.label.clone(),
            catalog_number: enrichment.catalog_number.clone(),
            ..ReleaseInfo::default()
        }],
        acoustid_score: None,
        source: CandidateSource::DiscogsText,
    }
}

/// Scores a fallback candidate as a text-search one ([`score::score`]),
/// then caps it: at [`DISCOGS_CAP`] always, [`NO_DURATION_CAP`] without a
/// Discogs duration, [`LABEL_DIFFERS_CAP`] when the file's label is not on
/// the release, and below review unless title, artist and mix all agree.
#[must_use]
pub fn score_match(
    existing: &ExistingTags,
    duration_secs: f64,
    candidate: Candidate,
    has_duration: bool,
    label_agrees: Option<bool>,
) -> Scored {
    let ours = mixname::parse_title(&existing.title);
    let theirs = score::candidate_mix(&candidate);
    let (our_base, _) = normalize::strip_featuring(&ours.base);
    let (their_base, _) = normalize::strip_featuring(&theirs.base);
    let agrees = normalize::artist_similarity(&existing.artist, &candidate.artist_credit) >= ARTIST_MATCH
        && normalize::similarity(&our_base, &their_base) >= TITLE_MATCH
        && matches!(mixname::agree(&ours.mix, &theirs.mix), MixAgreement::Same | MixAgreement::Compatible);

    let mut scored = score::score(existing, duration_secs, candidate);
    let (cap, why) = if !agrees {
        (DISAGREEMENT_CAP, "Discogs fallback: title, artist or mix differ")
    } else if label_agrees == Some(false) {
        (LABEL_DIFFERS_CAP, "Discogs fallback: the file's label is not on this release")
    } else if !has_duration {
        (NO_DURATION_CAP, "Discogs fallback: no duration listed")
    } else {
        (DISCOGS_CAP, "Discogs fallback")
    };
    if scored.score > cap {
        scored.score = cap;
        scored.breakdown.rejected = Some(format!("{why}: capped at {cap:.2}"));
    }
    scored
}

/// What a fallback match may change: only release fields that are empty in
/// the file now (album, label, catalog number, year, Discogs release ID)
/// and the sub-genre from Discogs styles. Never title, artist or genre, no
/// MusicBrainz IDs, no artwork.
#[must_use]
pub fn propose(snapshot: &TagSnapshot, enrichment: &Enrichment) -> Proposal {
    let wanted = [
        (Field::Album, enrichment.release_title.clone()),
        (Field::Label, enrichment.label.clone()),
        (Field::CatalogNumber, enrichment.catalog_number.clone()),
        (Field::Year, enrichment.year.map(|y| y.to_string()).unwrap_or_default()),
        (Field::DiscogsReleaseId, enrichment.discogs_release_id.map(|id| id.to_string()).unwrap_or_default()),
        (Field::SubGenre, enrichment.styles.join("; ")),
    ];
    let changes = wanted
        .into_iter()
        .filter(|(field, proposed)| !proposed.trim().is_empty() && pipeline::current(snapshot, *field).trim().is_empty())
        .map(|(field, proposed)| FieldChange { field, current: String::new(), proposed })
        .collect();
    Proposal::new(changes, None, snapshot.has_artwork)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::FrameValues;

    fn tags(artist: &str, title: &str, label: &str) -> ExistingTags {
        ExistingTags { artist: artist.into(), title: title.into(), label: label.into(), ..ExistingTags::default() }
    }

    fn cand(artist: &str, title: &str, length_ms: Option<u32>) -> Candidate {
        Candidate {
            recording_mbid: String::new(),
            artists: vec![artist.into()],
            artist_credit: artist.into(),
            title: title.into(),
            disambiguation: String::new(),
            remixers: Vec::new(),
            length_ms,
            isrcs: Vec::new(),
            genres: Vec::new(),
            releases: Vec::new(),
            acoustid_score: None,
            source: CandidateSource::DiscogsText,
        }
    }

    #[test]
    fn a_full_agreement_reaches_review_but_never_auto_apply() {
        let file = tags("Example Artist", "Fallback Track (Extended Mix)", "");
        let s = score_match(&file, 390.0, cand("Example Artist", "Fallback Track (Extended Mix)", Some(390_000)), true, None);
        assert!((s.score - DISCOGS_CAP).abs() < f64::EPSILON, "{s:?}");
        assert!(s.score < crate::decide::AUTO_APPLY_THRESHOLD);
        assert_eq!(s.breakdown.rejected.as_deref(), Some("Discogs fallback: capped at 0.85"));
    }

    #[test]
    fn missing_duration_and_another_label_lower_the_score() {
        let file = tags("Example Artist", "Fallback Track (Extended Mix)", "Fallback Records");
        let c = || cand("Example Artist", "Fallback Track (Extended Mix)", None);
        let no_duration = score_match(&file, 390.0, c(), false, Some(true));
        assert!(no_duration.score >= 0.5 && no_duration.score <= NO_DURATION_CAP, "{no_duration:?}");
        let other_label = score_match(&file, 390.0, cand("Example Artist", "Fallback Track (Extended Mix)", Some(390_000)), true, Some(false));
        assert!((other_label.score - LABEL_DIFFERS_CAP).abs() < f64::EPSILON, "{other_label:?}");
        let both = score_match(&file, 390.0, c(), false, Some(false));
        assert!(both.score <= LABEL_DIFFERS_CAP && both.score >= 0.5, "{both:?}");
    }

    #[test]
    fn without_title_artist_and_mix_agreeing_it_stays_below_review() {
        let file = tags("Example Artist", "Fallback Track (Extended Mix)", "");
        for (artist, title) in [
            ("Someone Else", "Fallback Track (Extended Mix)"),
            ("Example Artist", "Fallback Track (Radio Edit)"),
            ("Example Artist", "Fallback Track (Someone Remix)"),
            ("Example Artist", "Another Track (Extended Mix)"),
        ] {
            let s = score_match(&file, 390.0, cand(artist, title, Some(390_000)), true, None);
            assert!(s.score < crate::decide::REVIEW_THRESHOLD, "{artist} - {title}: {s:?}");
        }
    }

    #[test]
    fn the_first_artist_is_cut_only_at_real_separators() {
        assert_eq!(first_artist("A, B").as_deref(), Some("A"));
        assert_eq!(first_artist("Mastiksoul feat. The Groove").as_deref(), Some("Mastiksoul"));
        assert_eq!(first_artist("Armin van Buuren & Vini Vici").as_deref(), Some("Armin van Buuren"));
        assert_eq!(first_artist("E&G"), None);
        assert_eq!(first_artist("Deepack"), None);
    }

    #[test]
    fn labels_compare_loosely_but_not_blindly() {
        assert!(same_label("Ministry Of Sound (UK)", "Ministry of Sound"));
        assert!(same_label("Fraction Records", "fraction records"));
        assert!(!same_label("Fraction Records", "Dirty Workz"));
        assert!(!same_label("", "Dirty Workz"));
    }

    fn snapshot(fields: &[(Field, &str)]) -> TagSnapshot {
        let mut snap = TagSnapshot::default();
        for (field, value) in fields {
            snap.values.insert(*field, vec![FrameValues { frame: "X".into(), values: vec![(*value).to_owned()] }]);
        }
        snap
    }

    #[test]
    fn the_proposal_only_fills_empty_release_fields() {
        let enrichment = Enrichment {
            discogs_release_id: Some(9_000_102),
            release_title: "Fallback EP".into(),
            label: "Fallback Records".into(),
            catalog_number: "FB001".into(),
            year: Some(2019),
            styles: vec!["Tech House".into(), "Minimal".into()],
            genres: vec!["Electronic".into()],
        };
        let snap = snapshot(&[(Field::Label, "Existing Label"), (Field::Year, crate::tags::UNREADABLE), (Field::Title, "T")]);
        let proposal = propose(&snap, &enrichment);
        let fields: Vec<Field> = proposal.changes.iter().map(|c| c.field).collect();
        assert_eq!(fields, [Field::Album, Field::CatalogNumber, Field::DiscogsReleaseId, Field::SubGenre]);
        assert!(proposal.changes.iter().all(|c| c.current.is_empty()));
        assert_eq!(proposal.changes[3].proposed, "Tech House; Minimal");
        assert!(proposal.artwork.is_none());
        // Everything already filled: nothing to change but the unread sub-genre.
        let full = snapshot(&[
            (Field::Album, "A"),
            (Field::Label, "L"),
            (Field::CatalogNumber, "C"),
            (Field::Year, "2019"),
            (Field::DiscogsReleaseId, "1"),
        ]);
        let fields: Vec<Field> = propose(&full, &enrichment).changes.iter().map(|c| c.field).collect();
        assert_eq!(fields, [Field::SubGenre]);
    }
}
