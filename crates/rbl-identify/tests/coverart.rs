//! The Cover Art Archive client against an image recorded from
//! coverartarchive.org (see `fixtures/README.md`), replayed by URL path.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use common::FakeTransport;
use rbl_identify::IdentifyError;
use rbl_identify::clock::ManualClock;
use rbl_identify::coverart::CoverArtClient;
use rbl_identify::ratelimit::Limiters;

const UA: &str = "rbxport-identify-tests/0.1 ( https://example.invalid/ )";

/// Deepack "Music Is the Key" and its release group: a front cover.
const MUSIC_IS_THE_KEY: &str = "c3132648-11c5-4476-b152-979253d9241a";
const MUSIC_IS_THE_KEY_GROUP: &str = "fdde1ef4-0e2e-451f-a6a2-8f43e633bf3d";
/// E&G vs. Andromedha "Spirit" and its group: no artwork (404, observed
/// live on 2026-11-01; the fake transport answers 404 for it too).
const SPIRIT: &str = "0c59bcac-fac3-461c-aef2-54039114dc8f";
const SPIRIT_GROUP: &str = "a0fb1254-322b-4906-952a-fbe8ae93a45b";
const COVER_JPEG: &str = "coverart_release_c3132648-11c5-4476-b152-979253d9241a_front-500.jpg";

fn recorded() -> FakeTransport {
    let index: BTreeMap<String, String> = serde_json::from_slice(&common::fixture("coverart_index.json")).unwrap();
    index.iter().fold(FakeTransport::new(), |transport, (url, file)| transport.route(url, file))
}

fn client(transport: Arc<FakeTransport>) -> (Arc<ManualClock>, CoverArtClient) {
    let clock = Arc::new(ManualClock::new());
    let limiters = Limiters::new(clock.clone());
    let client = CoverArtClient::new(transport, clock.clone(), limiters.coverart, UA.to_owned());
    (clock, client)
}

#[test]
fn a_release_front_cover_is_fetched_and_sniffed() {
    let transport = Arc::new(recorded());
    let (_, client) = client(transport.clone());
    let image = client.front(MUSIC_IS_THE_KEY).unwrap().unwrap();
    assert_eq!(image.mime, "image/jpeg");
    assert_eq!(image.url, format!("https://coverartarchive.org/release/{MUSIC_IS_THE_KEY}/front-500"));
    assert_eq!(image.bytes, common::fixture(COVER_JPEG));
    let requests = transport.requests();
    assert_eq!(requests.len(), 1);
    // A User-Agent and nothing else: this client holds no secret.
    let names: Vec<&str> = requests[0].headers.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["User-Agent"]);
    assert_eq!(requests[0].headers[0].1, UA);
    assert!(!format!("{client:?}").contains("example.invalid"));
}

#[test]
fn a_release_group_front_cover_is_fetched() {
    let (_, client) = client(Arc::new(recorded()));
    let image = client.front_of_group(MUSIC_IS_THE_KEY_GROUP).unwrap().unwrap();
    assert_eq!(image.url, format!("https://coverartarchive.org/release-group/{MUSIC_IS_THE_KEY_GROUP}/front-500"));
    assert_eq!(image.bytes, common::fixture(COVER_JPEG));
}

#[test]
fn no_artwork_is_none_not_an_error() {
    let (_, client) = client(Arc::new(recorded()));
    assert_eq!(client.front(SPIRIT).unwrap(), None);
    assert_eq!(client.front_of_group(SPIRIT_GROUP).unwrap(), None);
}

#[test]
fn a_malformed_mbid_is_refused_before_any_request() {
    let transport = Arc::new(recorded());
    let (_, client) = client(transport.clone());
    for mbid in ["", "not-a-uuid", "../release/c3132648-11c5-4476-b152-979253d9241", "c3132648-11c5-4476-b152-979253d9241a?x"] {
        assert!(matches!(client.front(mbid), Err(IdentifyError::Malformed { .. })), "{mbid}");
        assert!(matches!(client.front_of_group(mbid), Err(IdentifyError::Malformed { .. })), "{mbid}");
    }
    assert_eq!(transport.requests().len(), 0, "no request was sent");
}

#[test]
fn a_body_that_is_not_an_image_is_refused() {
    let page = b"<html><body>Internal Server Error</body></html>".repeat(40);
    let path = format!("/release/{SPIRIT}/front-500");
    let (_, client) = client(Arc::new(FakeTransport::new().route_body(&path, &page)));
    assert!(matches!(client.front(SPIRIT), Err(IdentifyError::Malformed { service: "coverartarchive", .. })));
}

#[test]
fn other_failures_are_errors() {
    // A redirect the transport would not follow arrives as a 3xx.
    let path = format!("/release/{SPIRIT}/front-500");
    let (_, client) = client(Arc::new(FakeTransport::new().route_status(&path, 307, b"")));
    assert!(matches!(client.front(SPIRIT), Err(IdentifyError::Status { status: 307, .. })));
}

#[test]
fn requests_are_spaced_a_second_apart() {
    let (clock, client) = client(Arc::new(recorded()));
    client.front(MUSIC_IS_THE_KEY).unwrap();
    client.front_of_group(MUSIC_IS_THE_KEY_GROUP).unwrap();
    client.front(SPIRIT).unwrap();
    assert_eq!(clock.sleeps(), [Duration::from_millis(1050), Duration::from_millis(1050)]);
}
