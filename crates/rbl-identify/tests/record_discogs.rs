//! Re-records the Discogs and Cover Art Archive fixtures from the live
//! services. Ignored, and a no-op unless `RBL_IDENTIFY_RECORD=1`, so CI never
//! calls out:
//!
//! ```sh
//! RBL_IDENTIFY_RECORD=1 cargo test -p rbl-identify --test record_discogs -- --ignored --nocapture
//! ```
//!
//! Discogs needs the user's token, read from `.env.identify` at the
//! workspace root (or the file `RBL_IDENTIFY_ENV` names). Without
//! `RBL_IDENTIFY_RECORD_RELEASES` it records the searches; with
//! `RBL_IDENTIFY_RECORD_RELEASES=id,id` only those release lookups, and with
//! `RBL_IDENTIFY_RECORD_IMAGE=uri` one image download.
//!
//! Everything goes through the real clients and limiters, with a test
//! User-Agent that names no person, within [`DISCOGS_BUDGET`] Discogs
//! requests and one Cover Art Archive request per subject (four). Before a Discogs body is saved, it is
//! checked not to contain the token, and the `community`, `contributors`,
//! `submitter` and `user_data` objects (other users' names, the token
//! owner's collection flags) are removed. An image is saved only if it is
//! under [`MAX_SAVED_IMAGE`].

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rbl_identify::Result;
use rbl_identify::clock::SystemClock;
use rbl_identify::config::{Config, Secret};
use rbl_identify::coverart::CoverArtClient;
use rbl_identify::discogs::{DiscogsClient, DiscogsQuery};
use rbl_identify::ratelimit::Limiters;
use rbl_identify::transport::{HttpTransport, Request, Response, Transport};

const USER_AGENT: &str = "rbxport-identify-tests/0.1 ( https://github.com/ )";
const DISCOGS_BUDGET: usize = 15;
const MAX_SAVED_IMAGE: usize = 100 * 1024;

/// Keys removed from every Discogs body before it is saved.
const STRIPPED_KEYS: &[&str] = &["community", "contributors", "submitter", "user_data"];

/// Cover Art Archive subjects from the MusicBrainz fixtures: E&G vs.
/// Andromedha "Spirit" (the Aerofoil remix single), which has no artwork,
/// and Deepack "Music Is the Key", which has a front cover.
const COVERART_SUBJECTS: &[(&str, &str)] = &[
    ("release", "0c59bcac-fac3-461c-aef2-54039114dc8f"),
    ("release-group", "a0fb1254-322b-4906-952a-fbe8ae93a45b"),
    ("release", "c3132648-11c5-4476-b152-979253d9241a"),
    ("release-group", "fdde1ef4-0e2e-451f-a6a2-8f43e633bf3d"),
];

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn recording() -> bool {
    if std::env::var("RBL_IDENTIFY_RECORD").as_deref() == Ok("1") {
        true
    } else {
        eprintln!("set RBL_IDENTIFY_RECORD=1 to record");
        false
    }
}

fn strip(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for key in STRIPPED_KEYS {
                map.remove(*key);
            }
            map.values_mut().for_each(strip);
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(strip),
        _ => {}
    }
}

/// Saves each 200 body under a name derived from its URL, and records the
/// URL's path and query in an index.
struct Recorder {
    inner: HttpTransport,
    /// Names the next search's file.
    label: Mutex<String>,
    /// Never printed; only searched for in bodies.
    token: Option<Secret>,
    budget: usize,
    sent: Mutex<usize>,
    index: Mutex<BTreeMap<String, String>>,
}

impl Recorder {
    fn new(inner: HttpTransport, token: Option<Secret>, budget: usize) -> Self {
        Self {
            inner,
            label: Mutex::new(String::new()),
            token,
            budget,
            sent: Mutex::new(0),
            index: Mutex::new(BTreeMap::new()),
        }
    }

    fn save(&self, url: &str, body: &[u8]) -> Option<String> {
        if let Some(token) = &self.token {
            let needle = token.expose().as_bytes();
            assert!(!body.windows(needle.len()).any(|w| w == needle), "a body holds the token; not saved");
        }
        let host = url.split_once("://").map_or(url, |(_, r)| r).split('/').next().unwrap();
        let key = common::path_and_query(url);
        let path = key.split('?').next().unwrap();
        let (name, bytes) = match host {
            "api.discogs.com" => {
                let name = if path == "/database/search" {
                    format!("discogs_search_{}.json", self.label.lock().unwrap())
                } else if let Some(id) = path.strip_prefix("/releases/") {
                    format!("discogs_release_{id}.json")
                } else {
                    panic!("unexpected request {path}");
                };
                let mut value: serde_json::Value = serde_json::from_slice(body).unwrap();
                strip(&mut value);
                (name, (serde_json::to_string_pretty(&value).unwrap() + "\n").into_bytes())
            }
            "i.discogs.com" => ("discogs_image.jpg".to_owned(), body.to_vec()),
            _ => panic!("unexpected host"),
        };
        if host == "i.discogs.com" && bytes.len() >= MAX_SAVED_IMAGE {
            eprintln!("  not saved: {} bytes is over the fixture limit", bytes.len());
            return None;
        }
        std::fs::write(fixtures().join(&name), &bytes).unwrap();
        self.index.lock().unwrap().insert(key.to_owned(), name.clone());
        Some(name)
    }

    fn write_index(&self, file: &str) {
        let path = fixtures().join(file);
        let mut merged: BTreeMap<String, String> =
            std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        merged.extend(self.index.lock().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())));
        std::fs::write(&path, serde_json::to_string_pretty(&merged).unwrap() + "\n").unwrap();
        eprintln!("{} requests", self.sent.lock().unwrap());
    }
}

impl Transport for Recorder {
    fn send(&self, service: &'static str, request: &Request) -> Result<Response> {
        {
            let mut sent = self.sent.lock().unwrap();
            *sent += 1;
            assert!(*sent <= self.budget, "request budget spent");
        }
        let response = self.inner.send(service, request)?;
        eprintln!("{} {} {} bytes", response.status, common::path_and_query(&request.url), response.body.len());
        if response.status == 200 {
            if let Some(name) = self.save(&request.url, &response.body) {
                eprintln!("  saved {name}");
            }
        }
        Ok(response)
    }
}

#[test]
#[ignore = "live: records fixtures from api.discogs.com"]
fn record_discogs_fixtures() {
    if !recording() {
        return;
    }
    let env = std::env::var("RBL_IDENTIFY_ENV")
        .map_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.env.identify"), PathBuf::from);
    let config = Config::load(&env).unwrap();
    let token = config.discogs_token.expect("DISCOGS_TOKEN is not set");
    let recorder = Arc::new(Recorder::new(HttpTransport::new().unwrap(), Some(token.clone()), DISCOGS_BUDGET));
    let clock = Arc::new(SystemClock::new());
    let limiters = Limiters::new(clock.clone());
    let client = DiscogsClient::new(recorder.clone(), clock, limiters.discogs, token, USER_AGENT.to_owned());

    let releases: Vec<u64> = std::env::var("RBL_IDENTIFY_RECORD_RELEASES")
        .map(|v| v.split(',').filter_map(|s| s.trim().parse().ok()).collect())
        .unwrap_or_default();
    let image = std::env::var("RBL_IDENTIFY_RECORD_IMAGE").ok().filter(|s| !s.is_empty());
    if releases.is_empty() && image.is_none() {
        for (label, artist, track) in [
            ("recondite_nadsat", "Recondite", "Nadsat"),
            ("floe_hysteresis", "Floe", "Hysteresis"),
            ("alex_guesta_voodoo", "Alex Guesta", "Voodoo"),
            ("deepack_the_music", "Deepack", "The Music"),
        ] {
            label.clone_into(&mut recorder.label.lock().unwrap());
            let query =
                DiscogsQuery { artist: artist.into(), track: track.into(), label: None, catno: None, per_page: 10 };
            for hit in client.search(&query).unwrap() {
                eprintln!(
                    "  {} | {} | {:?} | {:?} | {} | {:?} | {:?}",
                    hit.release_id, hit.title, hit.year, hit.labels, hit.catno, hit.styles, hit.formats
                );
            }
        }
    }
    for id in releases {
        let release = client.release(id).unwrap();
        eprintln!("  {} | {:?} | {:?} | {:?}", release.title, release.artists, release.labels, release.styles);
        for track in &release.tracklist {
            eprintln!("    {} {} {:?} {:?}", track.position, track.title, track.duration_secs, track.extra_artists);
        }
        for image in &release.images {
            eprintln!("    image primary={} {}x{} {}", image.primary, image.width, image.height, image.uri);
        }
    }
    if let Some(uri) = image {
        let fetched = client.image(&uri).unwrap();
        eprintln!("  image {} {} bytes", fetched.mime, fetched.bytes.len());
    }
    recorder.write_index("discogs_index.json");
}

#[test]
#[ignore = "live: records fixtures from coverartarchive.org"]
fn record_coverart_fixtures() {
    if !recording() {
        return;
    }
    // The live client, so its own redirect-limited transport; the recorder
    // cannot wrap that, so it saves what the client returns. One request
    // per subject; `RBL_IDENTIFY_RECORD_COVERART=entity:mbid,...` replaces
    // the default subjects.
    let clock = Arc::new(SystemClock::new());
    let limiters = Limiters::new(clock.clone());
    let client = CoverArtClient::live(clock, limiters.coverart, USER_AGENT.to_owned()).unwrap();
    let chosen: Vec<(String, String)> = std::env::var("RBL_IDENTIFY_RECORD_COVERART").map_or_else(
        |_| COVERART_SUBJECTS.iter().map(|(e, m)| ((*e).to_owned(), (*m).to_owned())).collect(),
        |v| v.split(',').filter_map(|s| s.trim().split_once(':')).map(|(e, m)| (e.to_owned(), m.to_owned())).collect(),
    );
    let path = fixtures().join("coverart_index.json");
    let mut index: BTreeMap<String, String> =
        std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    for (entity, mbid) in &chosen {
        let (entity, mbid) = (entity.as_str(), mbid.as_str());
        let fetched =
            if entity == "release" { client.front(mbid).unwrap() } else { client.front_of_group(mbid).unwrap() };
        let Some(image) = fetched else {
            eprintln!("{entity} {mbid}: no front cover");
            continue;
        };
        eprintln!("{entity} {mbid}: {} {} bytes", image.mime, image.bytes.len());
        if image.bytes.len() >= MAX_SAVED_IMAGE {
            eprintln!("  not saved: over the fixture limit");
            continue;
        }
        let name = format!("coverart_{entity}_{mbid}_front-500.jpg");
        std::fs::write(fixtures().join(&name), &image.bytes).unwrap();
        index.insert(common::path_and_query(&image.url).to_owned(), name);
    }
    std::fs::write(&path, serde_json::to_string_pretty(&index).unwrap() + "
").unwrap();
}
