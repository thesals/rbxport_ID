//! The Discogs client against hand-written bodies in Discogs' response
//! shape (see `fixtures/README.md`), replayed by URL path and query.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::FakeTransport;
use rbl_identify::IdentifyError;
use rbl_identify::clock::ManualClock;
use rbl_identify::config::Secret;
use rbl_identify::discogs::{DiscogsClient, DiscogsHit, DiscogsLabel, DiscogsQuery, DiscogsTrack};
use rbl_identify::ratelimit::Limiters;
use rbl_identify::transport::{Method, Request};

/// A made-up token and User-Agent: test sources never hold real ones.
const TOKEN: &str = "TESTTOKENtesttoken0123456789abcdefABCDEF";
const UA: &str = "rbxport-identify-tests/0.1 ( https://example.invalid/ )";

const SEARCH: &str = "/database/search?type=release&artist=Example%20Artist&track=Example%20Track&per_page=25";
const SEARCH_NARROWED: &str = "/database/search?type=release&artist=Example%20Artist&track=Example%20Track\
                               &label=Example%20Records&catno=EX%20001&per_page=10";
const SEARCH_EMPTY: &str = "/database/search?type=release&artist=Deepack&track=The%20Music&per_page=25";
const COVER_JPEG: &str = "coverart_release_c3132648-11c5-4476-b152-979253d9241a_front-500.jpg";
const FRONT_URI: &str = "https://i.discogs.com/example/rs:fit/g:sm/q:90/h:500/w:500/front.jpeg";

fn fixtures() -> FakeTransport {
    FakeTransport::new()
        .route(SEARCH, "discogs_search_example.json")
        .route(SEARCH_NARROWED, "discogs_search_example.json")
        .route(SEARCH_EMPTY, "discogs_search_empty.json")
        .route("/releases/9000001", "discogs_release_example.json")
        .route("/example/rs:fit/g:sm/q:90/h:500/w:500/front.jpeg", COVER_JPEG)
}

fn client(transport: Arc<FakeTransport>) -> (Arc<ManualClock>, DiscogsClient) {
    let clock = Arc::new(ManualClock::new());
    let limiters = Limiters::new(clock.clone());
    let client = DiscogsClient::new(transport, clock.clone(), limiters.discogs, Secret::new(TOKEN), UA.to_owned());
    (clock, client)
}

fn query(artist: &str, track: &str) -> DiscogsQuery {
    DiscogsQuery { artist: artist.into(), track: track.into(), label: None, catno: None, per_page: 100 }
}

fn header<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    request.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
}

#[test]
fn a_search_reads_release_hits_and_skips_masters() {
    let (_, client) = client(Arc::new(fixtures()));
    let hits = client.search(&query("Example Artist", "Example Track")).unwrap();
    assert_eq!(
        hits[0],
        DiscogsHit {
            release_id: 9_000_001,
            title: "Example Artist (2) - Example Track".into(),
            year: Some(2014),
            labels: vec!["Example Records".into(), "Example Distribution".into()],
            catno: "EX001".into(),
            styles: vec!["Progressive House".into(), "Tech House".into()],
            genres: vec!["Electronic".into()],
            formats: vec!["File".into(), "MP3".into(), "WAV".into(), "Single".into()],
            country: "UK".into(),
        }
    );
    // Nulls read as empty, an empty year as unknown, a numeric year as one.
    assert_eq!((hits[1].year, hits[1].labels.len(), hits[1].styles.len()), (None, 0, 0));
    assert_eq!(hits[1].catno, "none");
    let ids: Vec<u64> = hits.iter().map(|h| h.release_id).collect();
    assert_eq!(ids, [9_000_001, 9_000_002, 9_000_003], "the master is not a release");
    assert_eq!((hits[2].year, hits[2].country.as_str()), (Some(2015), ""));
}

#[test]
fn a_search_with_no_hits_is_empty_not_an_error() {
    let (_, client) = client(Arc::new(fixtures()));
    assert_eq!(client.search(&query("Deepack", "The Music")).unwrap(), []);
}

#[test]
fn a_search_is_narrowed_by_label_and_catalog_number() {
    let transport = Arc::new(fixtures());
    let (_, client) = client(transport.clone());
    let narrowed = DiscogsQuery {
        artist: " Example Artist ".into(),
        track: "Example Track".into(),
        label: Some("Example Records".into()),
        catno: Some("EX 001".into()),
        per_page: 10,
    };
    assert_eq!(client.search(&narrowed).unwrap().len(), 3);
    // A blank label or catno is left out, and per_page is at most 25.
    let blank = DiscogsQuery { label: Some("  ".into()), catno: Some(String::new()), ..query("Example Artist", "Example Track") };
    assert_eq!(client.search(&blank).unwrap().len(), 3);
    let urls: Vec<String> = transport.requests().into_iter().map(|r| r.url).collect();
    assert_eq!(urls, [format!("https://api.discogs.com{SEARCH_NARROWED}"), format!("https://api.discogs.com{SEARCH}")]);
}

#[test]
fn every_api_request_carries_the_user_agent_and_the_token_in_a_header_only() {
    let transport = Arc::new(fixtures());
    let (_, client) = client(transport.clone());
    client.search(&query("Example Artist", "Example Track")).unwrap();
    client.release(9_000_001).unwrap();
    let requests = transport.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(request.method, Method::Get);
        assert!(request.url.starts_with("https://api.discogs.com/"), "{}", request.url);
        assert!(!request.url.contains(TOKEN), "the token is never in a URL");
        assert_eq!(header(request, "User-Agent"), Some(UA));
        assert_eq!(header(request, "Authorization"), Some(format!("Discogs token={TOKEN}").as_str()));
        assert!(!format!("{request:?}").contains(TOKEN));
    }
    let shown = format!("{client:?}");
    assert!(!shown.contains(TOKEN) && !shown.contains("example.invalid"), "{shown}");
}

#[test]
fn a_release_is_read_with_its_tracks_labels_and_images() {
    let (_, client) = client(Arc::new(fixtures()));
    let release = client.release(9_000_001).unwrap();
    assert_eq!(release.id, 9_000_001);
    assert_eq!(release.title, "Example Track");
    assert_eq!((release.year, release.released.as_str()), (Some(2014), "2014-06-02"));
    assert_eq!(release.artists, ["Example Artist", "Second Example"]);
    assert_eq!(
        release.labels,
        [
            DiscogsLabel { name: "Example Records".into(), catno: "EX001".into() },
            // Placeholders are kept as Discogs gives them.
            DiscogsLabel { name: "Not On Label (Example Artist Self-released)".into(), catno: "none".into() },
        ]
    );
    assert_eq!(release.styles, ["Progressive House", "Tech House"]);
    assert_eq!(release.genres, ["Electronic"]);

    let track = |position: &str, title: &str, duration_secs: Option<u32>| DiscogsTrack {
        position: position.into(),
        title: title.into(),
        duration_secs,
        artists: vec![],
        extra_artists: vec![],
    };
    let mut remix = track("2", "Example Track (Other Example Remix)", Some(425));
    remix.extra_artists = vec![
        ("Other Example".into(), "Remix".into()),
        ("Example Engineer".into(), "Mastered By [Digital]".into()),
    ];
    let mut part_two = track("3b", "Part Two", None);
    part_two.artists = vec!["Guest Example".into()];
    assert_eq!(
        release.tracklist,
        [
            track("1", "Example Track (Original Mix)", Some(402)),
            remix,
            track("3a", "Part One", Some(3723)),
            part_two,
            track("4", "Example Track (Dub)", None),
        ]
    );

    let primary: Vec<_> = release.images.iter().filter(|i| i.primary).collect();
    assert_eq!(release.images.len(), 2);
    assert_eq!(primary.len(), 1);
    assert_eq!((primary[0].uri.as_str(), primary[0].width, primary[0].height), (FRONT_URI, 500, 500));
}

#[test]
fn an_image_is_fetched_from_the_image_host_without_the_token() {
    let transport = Arc::new(fixtures());
    let (_, client) = client(transport.clone());
    let image = client.image(FRONT_URI).unwrap();
    assert_eq!(image.mime, "image/jpeg");
    assert_eq!(image.url, FRONT_URI);
    assert_eq!(image.bytes, common::fixture(COVER_JPEG));
    let requests = transport.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(header(&requests[0], "User-Agent"), Some(UA));
    assert_eq!(header(&requests[0], "Authorization"), None, "images are public; the token stays on the API host");
}

#[test]
fn an_image_anywhere_else_is_refused_before_any_request() {
    let transport = Arc::new(fixtures());
    let (_, client) = client(transport.clone());
    for uri in [
        "http://i.discogs.com/example/rs:fit/g:sm/q:90/h:500/w:500/front.jpeg",
        "https://img.discogs.com/example/front.jpeg",
        "https://i.discogs.com.example.invalid/front.jpeg",
        "https://i.discogs.com@example.invalid/front.jpeg",
        "https://i.discogs.com:444/front.jpeg",
        "https://coverartarchive.org/release/x/front-500",
        "",
    ] {
        let error = client.image(uri).unwrap_err();
        assert!(matches!(error, IdentifyError::Malformed { service: "discogs", .. }), "{uri}: {error}");
    }
    assert_eq!(transport.requests().len(), 0, "no request was sent");
}

#[test]
fn an_image_that_is_not_a_picture_is_refused() {
    let page = b"<!DOCTYPE html><html>".repeat(100);
    let transport = Arc::new(fixtures().route_body("/example/not-an-image.jpeg", &page).route_body("/example/tiny.jpeg", &[0xFF, 0xD8, 0xFF, 0xE0]));
    let (_, client) = client(transport);
    for uri in ["https://i.discogs.com/example/not-an-image.jpeg", "https://i.discogs.com/example/tiny.jpeg"] {
        let error = client.image(uri).unwrap_err();
        assert!(matches!(error, IdentifyError::Malformed { service: "discogs", .. }), "{uri}: {error}");
    }
}

#[test]
fn a_refused_token_is_a_status_error_that_does_not_name_the_token() {
    let body = br#"{"message": "You must authenticate to access this resource."}"#;
    let transport = Arc::new(FakeTransport::new().route_status("/releases/1", 401, body));
    let (_, client) = client(transport);
    let error = client.release(1).unwrap_err();
    assert!(matches!(error, IdentifyError::Status { service: "discogs", status: 401 }), "{error}");
    assert!(!error.to_string().contains(TOKEN) && !format!("{error:?}").contains(TOKEN));
}

#[test]
fn a_missing_release_is_a_404_and_garbage_is_malformed() {
    let transport = Arc::new(fixtures().route_body("/releases/2", b"<html>busy</html>"));
    let (_, client) = client(transport);
    assert!(matches!(client.release(3).unwrap_err(), IdentifyError::Status { status: 404, .. }));
    assert!(matches!(client.release(2).unwrap_err(), IdentifyError::Malformed { service: "discogs", .. }));
}

#[test]
fn sixty_requests_a_minute_then_the_client_waits() {
    let (clock, client) = client(Arc::new(fixtures()));
    for _ in 0..61 {
        client.release(9_000_001).unwrap();
    }
    assert_eq!(clock.sleeps(), [Duration::from_millis(60_050)]);
}
