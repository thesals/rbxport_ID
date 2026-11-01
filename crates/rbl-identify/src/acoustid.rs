//! The AcoustID lookup client: a fingerprint and a duration in, the
//! MusicBrainz recordings AcoustID links to it out.
//!
//! The API key travels only in the POST body to `api.acoustid.org`. It is
//! never put into a URL, a log line or an error.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::clock::Clock;
use crate::config::Secret;
use crate::error::{IdentifyError, Result};
use crate::ratelimit::{self, Rate, RateLimiter};
use crate::transport::{Request, Transport};

const SERVICE: &str = "acoustid";

/// The lookup endpoint.
pub const LOOKUP_URL: &str = "https://api.acoustid.org/v2/lookup";

/// The metadata asked for. The documentation's examples write it
/// `meta=recordings+releasegroups+compress` in a query string, where `+`
/// is a space; in a form body the separator is likewise a space.
pub const LOOKUP_META: &str = "recordings releasegroups compress";

/// One AcoustID track (a fingerprint cluster) matching the fingerprint.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AcoustIdMatch {
    /// The AcoustID track ID.
    pub id: String,
    /// How well the fingerprint matched, 0..=1.
    pub score: f64,
    /// The MusicBrainz recordings linked to the track. Empty when AcoustID
    /// knows the fingerprint but nobody linked it to a recording.
    pub recordings: Vec<AcoustIdRecording>,
}

/// A MusicBrainz recording as AcoustID summarises it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AcoustIdRecording {
    pub mbid: String,
    pub title: String,
    /// The artist names joined with their join phrases.
    pub artist_credit: String,
    pub artists: Vec<String>,
    pub duration_secs: Option<f64>,
    pub release_groups: Vec<AcoustIdReleaseGroup>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AcoustIdReleaseGroup {
    pub mbid: String,
    pub title: String,
    /// The primary type (`Album`, `Single`, ...); empty when unset.
    pub kind: String,
    pub secondary_types: Vec<String>,
}

/// Looks fingerprints up on AcoustID, at most 3 requests per second.
pub struct AcoustIdClient {
    transport: Arc<dyn Transport>,
    limiter: RateLimiter,
    clock: Arc<dyn Clock>,
    key: Secret,
    endpoint: String,
}

impl std::fmt::Debug for AcoustIdClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcoustIdClient").field("endpoint", &self.endpoint).finish_non_exhaustive()
    }
}

impl AcoustIdClient {
    pub fn new(transport: Arc<dyn Transport>, clock: Arc<dyn Clock>, key: Secret) -> Self {
        let limiter = RateLimiter::new(Rate::ACOUSTID, clock.clone());
        Self { transport, limiter, clock, key, endpoint: LOOKUP_URL.to_owned() }
    }

    /// Points the client at another lookup URL, for tests against a local
    /// server. Production code has no reason to call this: the key goes
    /// wherever the endpoint is.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Looks up a compressed, base64 fingerprint of a file `duration_secs`
    /// long. Results come back in AcoustID's order (best score first).
    pub fn lookup(&self, fingerprint: &str, duration_secs: u32) -> Result<Vec<AcoustIdMatch>> {
        let duration = duration_secs.to_string();
        let request = Request::post_form(
            &self.endpoint,
            &[
                ("client", self.key.expose()),
                ("duration", &duration),
                ("fingerprint", fingerprint),
                ("meta", LOOKUP_META),
                ("format", "json"),
            ],
        )
        .header("Accept", "application/json");
        let response = ratelimit::send(&*self.transport, &self.limiter, &*self.clock, SERVICE, &request)?;
        parse_lookup(&response.body, self.key.expose())
    }
}

/// Parses a lookup response body. `key` is scrubbed from any message the
/// service sends back, in case it ever echoes the request.
pub fn parse_lookup(body: &[u8], key: &str) -> Result<Vec<AcoustIdMatch>> {
    let response: WireResponse = serde_json::from_slice(body)
        .map_err(|e| IdentifyError::Malformed { service: SERVICE, reason: e.to_string() })?;
    match response.status.as_str() {
        "ok" => Ok(response.results.into_iter().map(WireResult::into_match).collect()),
        "error" => {
            let message = response.error.map(|e| e.message).filter(|m| !m.is_empty());
            let mut message = message.unwrap_or_else(|| "unspecified error".to_owned());
            if !key.is_empty() {
                message = message.replace(key, "<redacted>");
            }
            Err(IdentifyError::Service { service: SERVICE, message })
        }
        other => Err(IdentifyError::Malformed { service: SERVICE, reason: format!("unknown status {other:?}") }),
    }
}

#[derive(Deserialize)]
struct WireResponse {
    #[serde(default)]
    status: String,
    #[serde(default)]
    results: Vec<WireResult>,
    #[serde(default)]
    error: Option<WireError>,
}

#[derive(Deserialize)]
struct WireError {
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct WireResult {
    #[serde(default)]
    id: String,
    #[serde(default)]
    score: f64,
    #[serde(default)]
    recordings: Vec<WireRecording>,
}

#[derive(Deserialize)]
struct WireRecording {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    artists: Vec<WireArtist>,
    #[serde(default)]
    releasegroups: Vec<WireReleaseGroup>,
}

#[derive(Deserialize)]
struct WireArtist {
    #[serde(default)]
    name: String,
    #[serde(default)]
    joinphrase: String,
}

#[derive(Deserialize)]
struct WireReleaseGroup {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    secondarytypes: Vec<String>,
}

impl WireResult {
    fn into_match(self) -> AcoustIdMatch {
        AcoustIdMatch {
            id: self.id,
            score: self.score,
            recordings: self.recordings.into_iter().map(WireRecording::into_recording).collect(),
        }
    }
}

impl WireRecording {
    fn into_recording(self) -> AcoustIdRecording {
        let artist_credit = self.artists.iter().flat_map(|a| [a.name.as_str(), a.joinphrase.as_str()]).collect();
        AcoustIdRecording {
            mbid: self.id,
            title: self.title,
            artist_credit,
            artists: self.artists.into_iter().map(|a| a.name).collect(),
            duration_secs: self.duration,
            release_groups: self
                .releasegroups
                .into_iter()
                .map(|g| AcoustIdReleaseGroup {
                    mbid: g.id,
                    title: g.title,
                    kind: g.kind,
                    secondary_types: g.secondarytypes,
                })
                .collect(),
        }
    }
}
