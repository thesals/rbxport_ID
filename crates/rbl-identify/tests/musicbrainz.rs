//! The MusicBrainz client against bodies recorded from musicbrainz.org
//! (see `fixtures/README.md`), replayed by URL path and query.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use common::FakeTransport;
use rbl_identify::IdentifyError;
use rbl_identify::clock::ManualClock;
use rbl_identify::model::{CandidateSource, ReleaseInfo};
use rbl_identify::musicbrainz::{LOOKUP_INC, MusicBrainzClient};
use rbl_identify::ratelimit::Limiters;
use rbl_identify::transport::Method;

/// A made-up User-Agent: test sources never hold the real one.
const UA: &str = "rbxport-identify-tests/0.1 ( https://example.invalid/ )";

const SPIRIT_REMIX: &str = "a6087c9d-507d-4d3e-bcf1-3eb51e008bb1";
const MUSIC_IS_THE_KEY: &str = "91bd1ecd-c2fc-4d03-a720-ae6800ee9963";
const FRINGE_DJ_MIX: &str = "41f4b491-cc31-40f1-8d81-cc58b7d8f761";

/// Every recorded body, routed by the URL it answered.
fn recorded() -> FakeTransport {
    let index: BTreeMap<String, String> = serde_json::from_slice(&common::fixture("musicbrainz_index.json")).unwrap();
    index.iter().fold(FakeTransport::new(), |transport, (url, file)| transport.route(url, file))
}

fn client(transport: Arc<FakeTransport>) -> (Arc<ManualClock>, MusicBrainzClient) {
    let clock = Arc::new(ManualClock::new());
    let limiters = Limiters::new(clock.clone());
    let client = MusicBrainzClient::new(transport, clock.clone(), limiters.musicbrainz, UA.to_owned());
    (clock, client)
}

#[test]
fn a_search_with_no_hits_is_empty_not_an_error() {
    // Neither track is in MusicBrainz (recorded 2026-11-01).
    let (_, client) = client(Arc::new(recorded()));
    assert_eq!(client.search_recordings("Deepack", "The Music", 25).unwrap(), []);
    assert_eq!(client.search_recordings("Boral Kibil", "End of Life", 25).unwrap(), []);
}

#[test]
fn a_remix_search_finds_the_remix() {
    let (_, client) = client(Arc::new(recorded()));
    let found = client.search_recordings("E&G", "Spirit (Aerofoil Remix)", 25).unwrap();
    assert_eq!(found.len(), 1);
    let remix = &found[0];
    assert_eq!(remix.recording_mbid, SPIRIT_REMIX);
    assert_eq!(remix.artists, ["E&G", "Andromedha"]);
    assert_eq!(remix.artist_credit, "E&G vs. Andromedha");
    assert_eq!(remix.title, "Spirit (Aerofoil remix)");
    assert_eq!(remix.disambiguation, "");
    assert_eq!(remix.length_ms, Some(446_769));
    assert_eq!(remix.isrcs, ["GBKQU1450791"]);
    assert_eq!(remix.acoustid_score, None);
    assert_eq!(remix.source, CandidateSource::TextSearch);
    assert_eq!(
        remix.releases,
        [ReleaseInfo {
            mbid: "0c59bcac-fac3-461c-aef2-54039114dc8f".into(),
            title: "Spirit".into(),
            date: "2014-06-23".into(),
            country: "XW".into(),
            label: String::new(),
            catalog_number: String::new(),
            release_group_mbid: "a0fb1254-322b-4906-952a-fbe8ae93a45b".into(),
            release_group_type: "Single".into(),
            secondary_types: vec![],
        }]
    );
}

#[test]
fn a_lookup_merges_the_label_and_catalog_number_from_the_browse() {
    let transport = Arc::new(recorded());
    let (_, client) = client(transport.clone());
    let remix = client.lookup_recording(SPIRIT_REMIX, Some(0.97), CandidateSource::Fingerprint).unwrap();

    assert_eq!(remix.recording_mbid, SPIRIT_REMIX);
    assert_eq!(remix.artist_credit, "E&G vs. Andromedha");
    assert_eq!(remix.title, "Spirit (Aerofoil remix)");
    assert_eq!(remix.length_ms, Some(446_769));
    assert_eq!(remix.isrcs, ["GBKQU1450791"]);
    assert_eq!(remix.genres, Vec::<String>::new());
    assert_eq!(remix.acoustid_score, Some(0.97));
    assert_eq!(remix.source, CandidateSource::Fingerprint);
    assert_eq!(remix.releases.len(), 1);
    let release = &remix.releases[0];
    assert_eq!(release.title, "Spirit");
    assert_eq!(release.label, "Fraction Records");
    assert_eq!(release.catalog_number, "FRA224");
    assert_eq!(release.release_group_type, "Single");

    let requests = transport.requests();
    assert_eq!(requests.len(), 2, "one lookup, one release browse");
    assert!(requests[0].url.starts_with(&format!("https://musicbrainz.org/ws/2/recording/{SPIRIT_REMIX}?inc=")));
    assert!(requests[1].url.starts_with(&format!("https://musicbrainz.org/ws/2/release?recording={SPIRIT_REMIX}&")));
}

/// The recorded Spirit lookup, hand-edited by `edit` (its real `remixer`
/// relationship to Aerofoil is kept unless `edit` changes it).
fn edited_remix_lookup(edit: impl FnOnce(&mut serde_json::Value)) -> FakeTransport {
    let lookup = format!("/ws/2/recording/{SPIRIT_REMIX}?inc={LOOKUP_INC}&fmt=json");
    let mut body: serde_json::Value = serde_json::from_slice(&common::fixture(&format!(
        "musicbrainz_recording_{SPIRIT_REMIX}.json"
    )))
    .unwrap();
    edit(&mut body);
    let browse = format!("/ws/2/release?recording={SPIRIT_REMIX}&inc=labels+release-groups&limit=100&fmt=json");
    FakeTransport::new()
        .route_body(&lookup, &serde_json::to_vec(&body).unwrap())
        .route(&browse, &format!("musicbrainz_releases_{SPIRIT_REMIX}.json"))
}

#[test]
fn the_recorded_remix_names_its_remixer() {
    let (_, mb) = client(Arc::new(recorded()));
    let track = mb.lookup_recording(SPIRIT_REMIX, None, CandidateSource::TextSearch).unwrap();
    assert_eq!(track.remixers, ["Aerofoil"]);
    assert_eq!(track.disambiguation, "");
}

#[test]
fn a_remixer_is_reported_whatever_the_title_says() {
    // "Dub" in a title must not hide that this is a remix; and the title
    // and disambiguation stay exactly as MusicBrainz gives them.
    let transport = edited_remix_lookup(|body| {
        body["title"] = "Choctaw Dub".into();
        body["disambiguation"] = "DJ\u{2010}mix edit".into();
    });
    let (_, mb) = client(Arc::new(transport));
    let track = mb.lookup_recording(SPIRIT_REMIX, None, CandidateSource::TextSearch).unwrap();
    assert_eq!(track.title, "Choctaw Dub");
    assert_eq!(track.disambiguation, "DJ\u{2010}mix edit");
    assert_eq!(track.remixers, ["Aerofoil"]);
}

#[test]
fn remixers_prefer_the_credited_name_and_are_deduplicated() {
    let transport = edited_remix_lookup(|body| {
        let aerofoil = body["relations"][0].clone();
        let mut credited = aerofoil.clone();
        credited["target-credit"] = "Aerofoil (UK)".into();
        let mut producer = aerofoil.clone();
        producer["type"] = "producer".into();
        producer["artist"]["name"] = "Not A Remixer".into();
        body["relations"] = serde_json::json!([aerofoil, producer, credited, aerofoil]);
    });
    let (_, mb) = client(Arc::new(transport));
    let track = mb.lookup_recording(SPIRIT_REMIX, None, CandidateSource::TextSearch).unwrap();
    assert_eq!(track.remixers, ["Aerofoil", "Aerofoil (UK)"]);
}

#[test]
fn search_candidates_carry_no_remixers() {
    let (_, mb) = client(Arc::new(recorded()));
    let found = mb.search_recordings("E&G", "Spirit (Aerofoil Remix)", 25).unwrap();
    assert_eq!(found[0].remixers, Vec::<String>::new());
}

#[test]
fn an_extended_mix_keeps_its_mix_in_the_title() {
    let (_, client) = client(Arc::new(recorded()));
    let track = client.lookup_recording(MUSIC_IS_THE_KEY, None, CandidateSource::TextSearch).unwrap();
    assert_eq!(track.artists, ["Deepack"]);
    assert_eq!(track.artist_credit, "Deepack");
    assert_eq!(track.title, "Music Is the Key (extended mix)");
    assert_eq!(track.length_ms, Some(290_500));
    assert_eq!(track.isrcs, ["BEB682300276"]);
    let release = &track.releases[0];
    assert_eq!((release.label.as_str(), release.catalog_number.as_str()), ("Dirty Workz", "DWX1417"));
    assert_eq!((release.date.as_str(), release.release_group_type.as_str()), ("2023-05-10", "Single"));
}

#[test]
fn a_dj_mix_appearance_carries_its_secondary_types() {
    let (_, client) = client(Arc::new(recorded()));
    let track = client.lookup_recording(FRINGE_DJ_MIX, None, CandidateSource::TextSearch).unwrap();
    assert_eq!(track.artists, ["Mahmut Orhan", "Boral Kibil"]);
    assert_eq!(track.artist_credit, "Mahmut Orhan & Boral Kibil");
    assert_eq!(track.title, "Fringe");
    assert!(track.disambiguation.contains("DJ\u{2010}mix"), "{}", track.disambiguation);
    assert_eq!(track.isrcs, Vec::<String>::new());
    let release = &track.releases[0];
    assert_eq!(release.country, "FR");
    assert_eq!(release.label, "George V Records");
    assert_eq!(release.catalog_number, "3263132");
    assert_eq!(release.release_group_type, "Album");
    assert_eq!(release.secondary_types, ["Compilation", "DJ-mix"]);
}

#[test]
fn every_request_is_a_json_get_with_the_user_agent() {
    let transport = Arc::new(recorded());
    let (_, client) = client(transport.clone());
    client.search_recordings("E&G", "Spirit (Aerofoil Remix)", 25).unwrap();
    client.lookup_recording(SPIRIT_REMIX, None, CandidateSource::TextSearch).unwrap();
    let requests = transport.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_eq!(request.method, Method::Get);
        assert!(request.url.starts_with("https://musicbrainz.org/ws/2/"));
        assert!(request.url.ends_with("&fmt=json"), "{}", request.url);
        assert!(request.headers.contains(&("User-Agent".into(), UA.into())));
        assert!(request.headers.contains(&("Accept".into(), "application/json".into())));
    }
}

#[test]
fn requests_are_held_to_one_per_second() {
    let (clock, client) = client(Arc::new(recorded()));
    client.search_recordings("E&G", "Spirit (Aerofoil Remix)", 25).unwrap();
    client.lookup_recording(SPIRIT_REMIX, None, CandidateSource::TextSearch).unwrap();
    client.lookup_recording(MUSIC_IS_THE_KEY, None, CandidateSource::TextSearch).unwrap();
    // Five requests: four waits of at least a second.
    let sleeps = clock.sleeps();
    assert_eq!(sleeps.len(), 4);
    assert!(sleeps.iter().all(|s| *s >= Duration::from_secs(1)), "{sleeps:?}");
}

#[test]
fn genres_rank_by_votes_and_fall_back_to_tags() {
    // Hand-written: none of the recorded recordings has genres.
    let lookup = format!("/ws/2/recording/x?inc={LOOKUP_INC}&fmt=json");
    let browse = "/ws/2/release?recording=x&inc=labels+release-groups&limit=100&fmt=json";
    let transport = FakeTransport::new()
        .route_body(
            &lookup,
            br#"{"id":"x","title":"T","length":null,"genres":[{"name":"house","count":1},
                {"name":"trance","count":4},{"name":"electronic","count":4}],
                "tags":[{"name":"ignored","count":9}]}"#,
        )
        .route_body(browse, br#"{"releases":[],"release-count":0}"#);
    let (_, mb) = client(Arc::new(transport));
    let track = mb.lookup_recording("x", None, CandidateSource::TextSearch).unwrap();
    assert_eq!(track.genres, ["electronic", "trance", "house"]);
    assert_eq!(track.length_ms, None);

    let transport = FakeTransport::new()
        .route_body(&lookup, br#"{"id":"x","genres":[],"tags":[{"name":"techno","count":1},{"name":"acid","count":2}]}"#)
        .route_body(browse, br#"{"releases":[]}"#);
    let (_, mb) = client(Arc::new(transport));
    assert_eq!(mb.lookup_recording("x", None, CandidateSource::TextSearch).unwrap().genres, ["acid", "techno"]);
}

#[test]
fn errors_are_typed() {
    let (_, mb) = client(Arc::new(FakeTransport::new()));
    let error = mb.lookup_recording(SPIRIT_REMIX, None, CandidateSource::TextSearch).unwrap_err();
    assert!(matches!(error, IdentifyError::Status { service: "musicbrainz", status: 404 }), "{error:?}");

    let transport = FakeTransport::new().route_body("/ws/2/recording?query=recording%3A%22x%22&limit=1&fmt=json", b"<html>");
    let (_, mb) = client(Arc::new(transport));
    let error = mb.search_recordings("", "x", 1).unwrap_err();
    assert!(matches!(error, IdentifyError::Malformed { service: "musicbrainz", .. }), "{error:?}");
}

#[test]
fn the_client_debug_output_hides_the_user_agent() {
    let (_, client) = client(Arc::new(FakeTransport::new()));
    assert!(!format!("{client:?}").contains("example.invalid"));
}
