//! Re-records the MusicBrainz fixtures from the live service. Ignored, and
//! a no-op unless `RBL_IDENTIFY_RECORD=1`, so CI never calls out:
//!
//! ```sh
//! RBL_IDENTIFY_RECORD=1 cargo test -p rbl-identify --test record_musicbrainz -- --ignored --nocapture
//! ```
//!
//! It goes through the real client, so through its 1 req/s limiter, with a
//! test User-Agent that names no person. At most [`MAX_REQUESTS`] calls.
//! The replay tests in `musicbrainz.rs` assert on what these bodies say, so
//! re-recording may need those assertions updated.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rbl_identify::Result;
use rbl_identify::clock::SystemClock;
use rbl_identify::model::{Candidate, CandidateSource};
use rbl_identify::musicbrainz::{BASE_URL, LOOKUP_INC, MusicBrainzClient};
use rbl_identify::ratelimit::{self, Limiters};
use rbl_identify::transport::{HttpTransport, Request, Response, Transport};

const USER_AGENT: &str = "rbxport-identify-tests/0.1 ( https://github.com/ )";
const MAX_REQUESTS: usize = 20;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Saves every 200 body verbatim and records which URL it answered.
struct Recorder {
    inner: HttpTransport,
    search_label: Mutex<String>,
    index: Mutex<BTreeMap<String, String>>,
    sent: Mutex<usize>,
}

impl Recorder {
    fn file_name(&self, path_and_query: &str) -> String {
        let (path, query) = path_and_query.split_once('?').unwrap_or((path_and_query, ""));
        if path == "/ws/2/recording" {
            format!("musicbrainz_search_{}.json", self.search_label.lock().unwrap())
        } else if let Some(mbid) = path.strip_prefix("/ws/2/recording/") {
            format!("musicbrainz_recording_{mbid}.json")
        } else if path == "/ws/2/release" {
            let mbid = query.split('&').find_map(|p| p.strip_prefix("recording=")).unwrap();
            format!("musicbrainz_releases_{mbid}.json")
        } else {
            panic!("unexpected request {path}");
        }
    }
}

impl Transport for Recorder {
    fn send(&self, service: &'static str, request: &Request) -> Result<Response> {
        {
            let mut sent = self.sent.lock().unwrap();
            *sent += 1;
            assert!(*sent <= MAX_REQUESTS, "request budget spent");
        }
        let response = self.inner.send(service, request)?;
        let key = common::path_and_query(&request.url).to_owned();
        eprintln!("{} {key}", response.status);
        if response.status == 200 {
            let name = self.file_name(&key);
            std::fs::write(fixtures().join(&name), &response.body).unwrap();
            self.index.lock().unwrap().insert(key, name);
        }
        Ok(response)
    }
}

#[test]
#[ignore = "live: records fixtures from musicbrainz.org"]
fn record_musicbrainz_fixtures() {
    if std::env::var("RBL_IDENTIFY_RECORD").as_deref() != Ok("1") {
        eprintln!("set RBL_IDENTIFY_RECORD=1 to record");
        return;
    }
    let recorder = Arc::new(Recorder {
        inner: HttpTransport::new().unwrap(),
        search_label: Mutex::new(String::new()),
        index: Mutex::new(BTreeMap::new()),
        sent: Mutex::new(0),
    });
    let clock = Arc::new(SystemClock::new());
    let limiters = Limiters::new(clock.clone());
    let client =
        MusicBrainzClient::new(recorder.clone(), clock.clone(), limiters.musicbrainz.clone(), USER_AGENT.to_owned());

    let list = |name: &str| -> Vec<String> {
        std::env::var(name)
            .map(|v| v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect())
            .unwrap_or_default()
    };
    // `RBL_IDENTIFY_RECORD_LOOKUPS=mbid,mbid` records only those lookups
    // (recording and release browse); `RBL_IDENTIFY_RECORD_RECORDINGS`
    // only the recording half, for when just its `inc` changed.
    let lookups = list("RBL_IDENTIFY_RECORD_LOOKUPS");
    let recordings = list("RBL_IDENTIFY_RECORD_RECORDINGS");
    if lookups.is_empty() && recordings.is_empty() {
        record_searches_and_lookups(&client, &recorder);
    }
    for mbid in &lookups {
        client.lookup_recording(mbid, None, CandidateSource::TextSearch).unwrap();
    }
    for mbid in &recordings {
        let url = format!("{BASE_URL}recording/{mbid}?inc={LOOKUP_INC}&fmt=json");
        let request = Request::get(url).header("User-Agent", USER_AGENT).header("Accept", "application/json");
        ratelimit::send(&*recorder, &limiters.musicbrainz, &*clock, "musicbrainz", &request).unwrap();
    }
    write_index(&recorder);
}

fn record_searches_and_lookups(client: &MusicBrainzClient, recorder: &Recorder) {
    let search =|label: &str, artist: &str, title: &str| -> Vec<Candidate> {
        label.clone_into(&mut recorder.search_label.lock().unwrap());
        let found = client.search_recordings(artist, title, 25).unwrap();
        for c in &found {
            eprintln!("  {} | {} | {} | {} | {:?}", c.recording_mbid, c.artist_credit, c.title, c.disambiguation, c.length_ms);
        }
        found
    };
    let deepack = search("deepack_the_music", "Deepack", "The Music");
    let boral = search("boral_kibil_end_of_life", "Boral Kibil", "End of Life");
    let spirit = search("eg_spirit_aerofoil_remix", "E&G", "Spirit (Aerofoil Remix)");

    let mentions = |c: &Candidate, word: &str| {
        let word = word.to_lowercase();
        c.title.to_lowercase().contains(&word) || c.disambiguation.to_lowercase().contains(&word)
    };
    let mut lookups: Vec<String> = Vec::new();
    for word in ["extended", "radio"] {
        if let Some(c) = deepack.iter().find(|c| c.artist_credit.contains("Deepack") && mentions(c, word)) {
            lookups.push(c.recording_mbid.clone());
        }
    }
    if let Some(c) = boral.iter().find(|c| c.artist_credit.contains("Boral Kibil")) {
        lookups.push(c.recording_mbid.clone());
    }
    if let Some(c) = spirit.iter().find(|c| mentions(c, "aerofoil")) {
        lookups.push(c.recording_mbid.clone());
    }
    lookups.dedup();
    for mbid in &lookups {
        client.lookup_recording(mbid, None, CandidateSource::TextSearch).unwrap();
    }
}

fn write_index(recorder: &Recorder) {
    let index = recorder.index.lock().unwrap();
    let path = fixtures().join("musicbrainz_index.json");
    let mut merged: BTreeMap<String, String> =
        std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    merged.extend(index.iter().map(|(k, v)| (k.clone(), v.clone())));
    std::fs::write(&path, serde_json::to_string_pretty(&merged).unwrap() + "\n").unwrap();
    eprintln!("{} requests", recorder.sent.lock().unwrap());
}
