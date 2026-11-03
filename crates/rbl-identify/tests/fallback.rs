//! The Discogs fallback for unidentified tracks, against hand-written
//! Discogs bodies (see `fixtures/README.md`) replayed by URL path and query.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;

use common::FakeTransport;
use rbl_identify::IdentifyError;
use rbl_identify::clock::ManualClock;
use rbl_identify::config::Secret;
use rbl_identify::decide::{AUTO_APPLY_FLOOR, REVIEW_THRESHOLD};
use rbl_identify::discogs::DiscogsClient;
use rbl_identify::fallback::{self, FallbackMatch, DISCOGS_CAP, LABEL_DIFFERS_CAP, NO_DURATION_CAP};
use rbl_identify::model::{CandidateSource, ExistingTags};
use rbl_identify::ratelimit::Limiters;

/// A made-up token and User-Agent: test sources never hold real ones.
const TOKEN: &str = "TESTTOKENtesttoken0123456789abcdefABCDEF";
const UA: &str = "rbxport-identify-tests/0.1 ( https://example.invalid/ )";

const BROAD: &str = "/database/search?type=release&artist=Example%20Artist&track=Fallback%20Track&per_page=10";

fn narrowed(label: &str) -> String {
    format!(
        "/database/search?type=release&artist=Example%20Artist&track=Fallback%20Track&label={}&per_page=10",
        label.replace(' ', "%20")
    )
}

fn releases(transport: FakeTransport) -> FakeTransport {
    transport
        .route("/releases/9000101", "discogs_fallback_release_9000101.json")
        .route("/releases/9000102", "discogs_fallback_release_9000102.json")
        .route("/releases/9000103", "discogs_fallback_release_9000103.json")
}

fn client(transport: &Arc<FakeTransport>) -> DiscogsClient {
    let clock = Arc::new(ManualClock::new());
    let limiters = Limiters::new(clock.clone());
    DiscogsClient::new(transport.clone(), clock, limiters.discogs, Secret::new(TOKEN), UA.to_owned())
}

fn file(title: &str, label: &str) -> ExistingTags {
    ExistingTags { artist: "Example Artist".into(), title: title.into(), label: label.into(), ..ExistingTags::default() }
}

fn find(transport: &Arc<FakeTransport>, existing: &ExistingTags, duration: f64) -> Option<FallbackMatch> {
    fallback::find(&client(transport), existing, duration).unwrap()
}

fn release_requests(transport: &FakeTransport) -> Vec<String> {
    transport
        .requests()
        .iter()
        .map(|r| common::path_and_query(&r.url).to_owned())
        .filter(|p| p.starts_with("/releases/"))
        .collect()
}

#[test]
fn the_release_on_the_files_label_is_opened_first_and_matched() {
    let transport = Arc::new(releases(FakeTransport::new().route(&narrowed("Fallback Records"), "discogs_fallback_search.json")));
    let found = find(&transport, &file("Fallback Track (Extended Mix)", "Fallback Records"), 390.4).unwrap();
    assert_eq!(release_requests(&transport), ["/releases/9000102"], "the label's release first, and only it");
    assert_eq!(found.label_agrees, Some(true));
    assert_eq!(found.scored.candidate.source, CandidateSource::DiscogsText);
    assert_eq!(found.scored.candidate.title, "Fallback Track (Extended Mix)");
    assert_eq!(found.scored.candidate.artist_credit, "Example Artist");
    assert!(found.scored.candidate.recording_mbid.is_empty(), "no MusicBrainz recording");
    assert!((found.scored.score - DISCOGS_CAP).abs() < f64::EPSILON, "{:?}", found.scored);
    assert!(found.scored.score < AUTO_APPLY_FLOOR);
    assert_eq!(found.enrichment.discogs_release_id, Some(9_000_102));
    assert_eq!((found.enrichment.label.as_str(), found.enrichment.catalog_number.as_str()), ("Fallback Records", "FB 001"));
    assert_eq!(found.enrichment.styles, ["Tech House", "Minimal"]);
}

#[test]
fn a_radio_edit_matches_the_radio_edit_not_the_extended_mix() {
    let transport = Arc::new(releases(FakeTransport::new().route(BROAD, "discogs_fallback_search.json")));
    let found = find(&transport, &file("Fallback Track (Radio Edit)", ""), 189.0).unwrap();
    // The compilation (first in Discogs' order) holds only the extended mix.
    assert_eq!(release_requests(&transport), ["/releases/9000101", "/releases/9000102"]);
    assert_eq!(found.scored.candidate.title, "Fallback Track (Radio Edit)");
    assert_eq!(found.label_agrees, None, "the file has no label to compare");
    assert!(found.scored.score >= REVIEW_THRESHOLD && found.scored.score <= DISCOGS_CAP);
}

#[test]
fn a_remix_is_never_the_original_and_a_remixer_credit_names_one() {
    let transport = Arc::new(releases(FakeTransport::new().route(BROAD, "discogs_fallback_search.json")));
    assert_eq!(find(&transport, &file("Fallback Track (Other Person Remix)", ""), 420.0), None);
    assert_eq!(release_requests(&transport).len(), fallback::MAX_RELEASES, "at most two releases per track");
    // The plainly titled third track is credited to a remixer.
    let found = find(&transport, &file("Fallback Track (Someone Remix)", ""), 420.0).unwrap();
    assert_eq!(found.scored.candidate.title, "Fallback Track");
    assert_eq!(found.scored.candidate.remixers, ["Someone"]);
    assert!(found.scored.score >= REVIEW_THRESHOLD);
}

#[test]
fn a_listed_duration_more_than_3_s_off_is_another_track() {
    let transport = Arc::new(releases(FakeTransport::new().route(BROAD, "discogs_fallback_search.json")));
    // 6:31 and 6:30 on Discogs; the file is 6:35.
    assert_eq!(find(&transport, &file("Fallback Track (Extended Mix)", ""), 395.0), None);
}

#[test]
fn without_a_listed_duration_the_match_scores_lower() {
    let transport =
        Arc::new(releases(FakeTransport::new().route(&narrowed("No Duration Records"), "discogs_fallback_search.json")));
    let found = find(&transport, &file("Fallback Track (Extended Mix)", "No Duration Records"), 390.0).unwrap();
    assert_eq!(found.enrichment.discogs_release_id, Some(9_000_103));
    assert_eq!(found.enrichment.year, None);
    assert!(found.scored.score >= REVIEW_THRESHOLD && found.scored.score <= NO_DURATION_CAP, "{:?}", found.scored);
}

#[test]
fn a_release_on_another_label_scores_lower_and_both_lookups_are_used() {
    // Narrowed by the file's label, nothing; then the broad search.
    let transport = Arc::new(releases(
        FakeTransport::new()
            .route(&narrowed("Unrelated Label"), "discogs_search_empty.json")
            .route(BROAD, "discogs_fallback_search.json"),
    ));
    let found = find(&transport, &file("Fallback Track (Extended Mix)", "Unrelated Label"), 390.0).unwrap();
    assert_eq!(release_requests(&transport), ["/releases/9000101", "/releases/9000102"]);
    assert_eq!(found.label_agrees, Some(false));
    assert!((found.scored.score - LABEL_DIFFERS_CAP).abs() < f64::EPSILON, "{:?}", found.scored);
}

#[test]
fn the_first_credited_artist_is_searched_last_and_a_partial_credit_is_not_proposed() {
    let transport = Arc::new(releases(
        FakeTransport::new()
            .route(
                "/database/search?type=release&artist=Example%20Artist%20%26%20Guest&track=Fallback%20Track&per_page=10",
                "discogs_search_empty.json",
            )
            .route(BROAD, "discogs_fallback_search.json"),
    ));
    let existing = ExistingTags { artist: "Example Artist & Guest".into(), ..file("Fallback Track (Extended Mix)", "") };
    let found = find(&transport, &existing, 390.0).unwrap();
    let searches = transport.requests().iter().filter(|r| r.url.contains("/database/search")).count();
    assert_eq!(searches, 2);
    // Discogs credits only one of the two artists: not enough to propose.
    assert!(found.scored.score < REVIEW_THRESHOLD, "{:?}", found.scored);
}

#[test]
fn a_file_without_artist_or_title_makes_no_request() {
    let transport = Arc::new(FakeTransport::new());
    assert_eq!(find(&transport, &ExistingTags { title: "Fallback Track".into(), ..ExistingTags::default() }, 390.0), None);
    assert_eq!(find(&transport, &ExistingTags { artist: "Example Artist".into(), ..ExistingTags::default() }, 390.0), None);
    assert_eq!(transport.requests().len(), 0);
}

#[test]
fn a_refused_token_is_an_error_and_stops_discogs() {
    let transport = Arc::new(FakeTransport::new().route_status(BROAD, 401, br#"{"message": "You must authenticate to access this resource."}"#));
    let discogs = client(&transport);
    let error = fallback::find(&discogs, &file("Fallback Track (Extended Mix)", ""), 390.0).unwrap_err();
    assert!(matches!(error, IdentifyError::Service { service: "discogs", .. }), "{error:?}");
    assert!(discogs.is_refused());
    assert!(!error.to_string().contains(TOKEN));
}

/// A silent mono 16-bit WAV of `seconds` at 8 kHz, tagged with `ID3v2`.
fn tagged_wav(dir: &std::path::Path, seconds: u32, artist: &str, title: &str) -> std::path::PathBuf {
    use lofty::config::WriteOptions;
    use lofty::prelude::Accessor as _;
    use lofty::tag::{Tag, TagExt as _, TagType};
    const RATE: u32 = 8_000;
    let data_len = seconds * RATE * 2;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1_u16.to_le_bytes()); // mono
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.resize(44 + data_len as usize, 0);
    let path = dir.join("track.wav");
    std::fs::write(&path, wav).unwrap();
    let mut tag = Tag::new(TagType::Id3v2);
    tag.set_artist(artist.to_owned());
    tag.set_title(title.to_owned());
    tag.save_to_path(&path, WriteOptions::default()).unwrap();
    path
}

#[test]
fn an_unidentified_track_found_on_discogs_goes_to_review_even_with_auto_apply_on() {
    use rbl_identify::acoustid::AcoustIdClient;
    use rbl_identify::model::{Decision, Field};
    use rbl_identify::musicbrainz::MusicBrainzClient;
    use rbl_identify::pipeline::{self, GenrePolicy, Services, Settings};

    let dir = tempfile::tempdir().unwrap();
    let path = tagged_wav(dir.path(), 190, "Example Artist", "Fallback Track (Radio Edit)");
    // AcoustID and MusicBrainz know nothing (404); only Discogs answers.
    let transport = Arc::new(releases(FakeTransport::new().route(BROAD, "discogs_fallback_search.json")));
    let clock = Arc::new(ManualClock::new());
    let limiters = Limiters::new(clock.clone());
    let acoustid = AcoustIdClient::new(transport.clone(), clock.clone(), limiters.acoustid.clone(), Secret::new("TESTKEY"));
    let musicbrainz = MusicBrainzClient::new(transport.clone(), clock.clone(), limiters.musicbrainz.clone(), UA.to_owned());
    let discogs = DiscogsClient::new(transport.clone(), clock, limiters.discogs.clone(), Secret::new(TOKEN), UA.to_owned());
    let services = Services { acoustid: &acoustid, musicbrainz: &musicbrainz, discogs: Some(&discogs), coverart: None };

    let result = pipeline::identify(&path, &services, Settings { auto_apply: true, genre: GenrePolicy::Replace, ..Settings::default() }).unwrap();
    assert_eq!(result.decision, Decision::Review);
    let best = result.best().unwrap();
    assert_eq!(best.candidate.source, CandidateSource::DiscogsText);
    assert!(best.score >= REVIEW_THRESHOLD && best.score <= DISCOGS_CAP, "{best:?}");
    assert_eq!(result.enrichment.as_ref().and_then(|e| e.discogs_release_id), Some(9_000_102));
    assert!(result.artwork.is_none());
    let proposal = result.proposal.as_ref().unwrap();
    let fields: Vec<Field> = proposal.changes.iter().map(|c| c.field).collect();
    assert_eq!(
        fields,
        [Field::Album, Field::Label, Field::CatalogNumber, Field::Year, Field::DiscogsReleaseId, Field::SubGenre],
        "fill-only release fields: never title, artist, genre or MusicBrainz IDs"
    );
    assert!(proposal.changes.iter().all(|c| c.current.is_empty()));
    assert!(!proposal.fills_only_empty(), "the unread sub-genre keeps it from ever counting as a pure fill");
    // The services that failed are warnings, not errors.
    assert!(result.warnings.iter().any(|w| w.starts_with("MusicBrainz search")), "{:?}", result.warnings);
    assert!(result.warnings.iter().any(|w| w.starts_with("AcoustID") || w.starts_with("fingerprint")), "{:?}", result.warnings);
}
