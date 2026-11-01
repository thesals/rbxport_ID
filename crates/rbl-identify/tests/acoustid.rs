//! The AcoustID client against hand-written responses in the documented
//! shape (see `fixtures/README.md`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;

use common::FakeTransport;
use rbl_identify::IdentifyError;
use rbl_identify::acoustid::{AcoustIdClient, AcoustIdReleaseGroup, LOOKUP_URL};
use rbl_identify::clock::ManualClock;
use rbl_identify::config::Secret;
use rbl_identify::ratelimit::Limiters;
use rbl_identify::transport::Method;

const KEY: &str = "fake-key-Xy12";

fn client(transport: Arc<FakeTransport>) -> AcoustIdClient {
    let clock = Arc::new(ManualClock::new());
    let limiters = Limiters::new(clock.clone());
    AcoustIdClient::new(transport, clock, limiters.acoustid, Secret::new(KEY))
}

#[test]
fn a_documented_response_parses() {
    let transport = Arc::new(FakeTransport::new().route("/v2/lookup", "acoustid_lookup_documented.json"));
    let matches = client(transport).lookup("AQABz0qUkZ", 641).unwrap();

    assert_eq!(matches.len(), 2);
    let best = &matches[0];
    assert_eq!(best.id, "9ff43b6a-4f16-427c-93c2-92307ca505e0");
    assert!((best.score - 1.0).abs() < f64::EPSILON);
    assert_eq!(best.recordings.len(), 2);

    let m83 = &best.recordings[0];
    assert_eq!(m83.mbid, "cd2e7c47-16f5-46c6-a37c-a1eb7bf599ff");
    assert_eq!(m83.title, "Lower Your Eyelids to Die With the Sun");
    assert_eq!(m83.artists, ["M83"]);
    assert_eq!(m83.artist_credit, "M83");
    assert_eq!(m83.duration_secs, Some(639.0));
    assert_eq!(
        m83.release_groups,
        [
            AcoustIdReleaseGroup {
                mbid: "ddaa2d4d-314e-3e7c-b1d0-f6d207f5aa2f".into(),
                title: "Before the Dawn Heals Us".into(),
                kind: "Album".into(),
                secondary_types: vec![],
            },
            AcoustIdReleaseGroup {
                mbid: "00000000-0000-4000-8000-000000000001".into(),
                title: "Hand-written compilation".into(),
                kind: "Album".into(),
                secondary_types: vec!["Compilation".into()],
            },
        ]
    );

    let collaboration = &best.recordings[1];
    assert_eq!(collaboration.artists, ["First Artist", "Second Artist"]);
    assert_eq!(collaboration.artist_credit, "First Artist & Second Artist");
    assert_eq!(collaboration.duration_secs, Some(412.5));
    assert_eq!(collaboration.release_groups.len(), 0);
}

#[test]
fn a_match_without_recordings_is_kept() {
    let transport = Arc::new(FakeTransport::new().route("/v2/lookup", "acoustid_lookup_documented.json"));
    let matches = client(transport).lookup("AQABz0qUkZ", 641).unwrap();
    let unlinked = &matches[1];
    assert_eq!(unlinked.id, "00000000-0000-4000-8000-000000000005");
    assert!((unlinked.score - 0.62).abs() < 1e-9);
    assert_eq!(unlinked.recordings.len(), 0);
}

#[test]
fn an_empty_result_list_is_not_an_error() {
    let transport = Arc::new(FakeTransport::new().route_body("/v2/lookup", br#"{"status":"ok","results":[]}"#));
    assert_eq!(client(transport).lookup("AQABz0qUkZ", 641).unwrap().len(), 0);
}

#[test]
fn the_request_is_a_form_post_to_the_lookup_url() {
    let transport = Arc::new(FakeTransport::new().route("/v2/lookup", "acoustid_lookup_documented.json"));
    client(transport.clone()).lookup("AQAB-z_0", 641).unwrap();

    let requests = transport.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, Method::Post);
    assert_eq!(request.url, LOOKUP_URL);
    assert_eq!(LOOKUP_URL, "https://api.acoustid.org/v2/lookup");
    assert!(!request.url.contains(KEY), "the key stays out of the URL");
    let (content_type, body) = request.body.as_ref().unwrap();
    assert_eq!(content_type, "application/x-www-form-urlencoded");
    assert_eq!(
        String::from_utf8(body.clone()).unwrap(),
        format!(
            "client={KEY}&duration=641&fingerprint=AQAB-z_0&meta=recordings%20releasegroups%20compress&format=json"
        )
    );
}

#[test]
fn a_service_error_becomes_a_service_error_without_the_key() {
    let transport = Arc::new(FakeTransport::new().route("/v2/lookup", "acoustid_lookup_error.json"));
    let error = client(transport).lookup("AQABz0qUkZ", 641).unwrap_err();
    assert!(
        matches!(&error, IdentifyError::Service { service: "acoustid", message } if message == "invalid API key"),
        "{error:?}"
    );
    assert!(!error.to_string().contains(KEY));
    assert!(!format!("{error:?}").contains(KEY));
}

#[test]
fn a_key_echoed_by_the_service_is_scrubbed() {
    let body = format!(r#"{{"status":"error","error":{{"code":4,"message":"invalid API key {KEY}"}}}}"#);
    let transport = Arc::new(FakeTransport::new().route_body("/v2/lookup", body.as_bytes()));
    let error = client(transport).lookup("AQABz0qUkZ", 641).unwrap_err();
    assert!(!error.to_string().contains(KEY), "{error}");
    assert!(!format!("{error:?}").contains(KEY));
}

#[test]
fn http_and_parse_failures_do_not_carry_the_key() {
    // No route: the fake answers 404.
    let error = client(Arc::new(FakeTransport::new())).lookup("AQABz0qUkZ", 641).unwrap_err();
    assert!(matches!(error, IdentifyError::Status { service: "acoustid", status: 404 }));
    assert!(!format!("{error} {error:?}").contains(KEY));

    let transport = Arc::new(FakeTransport::new().route_body("/v2/lookup", b"<html>not json</html>"));
    let error = client(transport).lookup("AQABz0qUkZ", 641).unwrap_err();
    assert!(matches!(error, IdentifyError::Malformed { service: "acoustid", .. }));
    assert!(!format!("{error} {error:?}").contains(KEY));
}

#[test]
fn the_client_debug_output_hides_the_key() {
    let client = client(Arc::new(FakeTransport::new()));
    assert!(!format!("{client:?}").contains(KEY));
}
