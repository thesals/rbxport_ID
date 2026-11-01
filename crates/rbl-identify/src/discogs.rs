//! The Discogs client: release search and release details (labels, catalog
//! numbers, styles, tracklist, images), and image downloads.
//!
//! Discogs allows 60 authenticated requests per minute; every request waits
//! on the shared Discogs limiter and backs off on 429 through
//! [`ratelimit::send`]. Discogs also requires a User-Agent on every request.
//!
//! The personal token goes only to `api.discogs.com`, only in the
//! `Authorization` header: never in a URL (which a log or error might
//! show), never to the image host. Neither the token nor the User-Agent
//! appears in this client's `Debug` output.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Deserializer};

use crate::clock::Clock;
use crate::config::Secret;
use crate::coverart::{FetchedImage, checked_image};
use crate::error::{IdentifyError, Result};
use crate::ratelimit::{self, RateLimiter};
use crate::transport::{Request, Transport, encode_form};

const SERVICE: &str = "discogs";

/// The API root. The token is sent only to this host.
pub const BASE_URL: &str = "https://api.discogs.com";
const API_HOST: &str = "api.discogs.com";
/// Said once Discogs answers 401: the token is not a personal access
/// token (a consumer key looks similar but does not authenticate).
const TOKEN_REFUSED: &str =
    "Discogs refused the token; use a personal access token (Discogs → Settings → Developers → Generate token)";
/// Where Discogs serves release images. Public: no token goes here.
const IMAGE_HOST: &str = "i.discogs.com";

/// Discogs' largest search page.
pub const MAX_PER_PAGE: u32 = 25;

/// What to search for. `track` is the title without its mix name, as the
/// caller gives it; `label` and `catno` narrow the search when known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscogsQuery {
    pub artist: String,
    pub track: String,
    pub label: Option<String>,
    pub catno: Option<String>,
    /// Clamped to 1..=[`MAX_PER_PAGE`].
    pub per_page: u32,
}

/// One release in search results, as Discogs summarises it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscogsHit {
    pub release_id: u64,
    /// `Artist - Release`, as Discogs gives it.
    pub title: String,
    pub year: Option<u16>,
    pub labels: Vec<String>,
    pub catno: String,
    pub styles: Vec<String>,
    pub genres: Vec<String>,
    pub formats: Vec<String>,
    pub country: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscogsRelease {
    pub id: u64,
    pub title: String,
    /// `None` when Discogs does not know it (it sends 0).
    pub year: Option<u16>,
    /// `2014-06-02`, `2014-06-00` or `2014`, as given.
    pub released: String,
    /// Artist names, with Discogs' ` (2)` disambiguation suffix removed.
    pub artists: Vec<String>,
    pub labels: Vec<DiscogsLabel>,
    pub styles: Vec<String>,
    pub genres: Vec<String>,
    /// Tracks only: headings and index entries are dropped, an index's
    /// sub-tracks flattened into the list.
    pub tracklist: Vec<DiscogsTrack>,
    pub images: Vec<DiscogsImage>,
}

/// A label credit. Kept exactly as Discogs gives it, placeholders included:
/// a self-release is labelled `Not On Label` (often with the artist's name
/// in parentheses), and a release without a catalog number has catno
/// `none`. Interpreting those is the caller's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscogsLabel {
    pub name: String,
    pub catno: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscogsTrack {
    /// `A1`, `3`, `1-04`, as given; may be empty.
    pub position: String,
    pub title: String,
    pub duration_secs: Option<u32>,
    /// Track-level artists (compilations), suffix removed; empty when the
    /// release's artists apply.
    pub artists: Vec<String>,
    /// `(name, role)`, e.g. `("Aerofoil", "Remix")`; suffix removed from the
    /// name, the role as given (it may carry a `[detail]`).
    pub extra_artists: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscogsImage {
    /// Discogs marks one image `primary`, the rest `secondary`.
    pub primary: bool,
    pub uri: String,
    pub width: u32,
    pub height: u32,
}

/// Searches and reads releases, at most 60 requests per minute.
pub struct DiscogsClient {
    transport: Arc<dyn Transport>,
    limiter: Arc<RateLimiter>,
    clock: Arc<dyn Clock>,
    token: Secret,
    user_agent: String,
    /// Set once Discogs refuses the token (401): a bad token would fail
    /// every request, so after the first the client stops asking.
    refused: AtomicBool,
}

impl std::fmt::Debug for DiscogsClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Neither the token nor the User-Agent.
        f.debug_struct("DiscogsClient").field("limiter", &self.limiter).finish_non_exhaustive()
    }
}

impl DiscogsClient {
    /// `transport` must not follow redirects ([`HttpTransport::new`]): a
    /// redirect could carry the token to another host. `limiter` must be
    /// the process's [`ratelimit::Limiters::discogs`]. `user_agent` is sent
    /// on every request, as Discogs requires.
    ///
    /// [`HttpTransport::new`]: crate::transport::HttpTransport::new
    pub fn new(
        transport: Arc<dyn Transport>,
        clock: Arc<dyn Clock>,
        limiter: Arc<RateLimiter>,
        token: Secret,
        user_agent: String,
    ) -> Self {
        Self { transport, limiter, clock, token, user_agent, refused: AtomicBool::new(false) }
    }

    /// Searches releases by artist and track title (and label and catalog
    /// number when given), in Discogs' relevance order.
    pub fn search(&self, query: &DiscogsQuery) -> Result<Vec<DiscogsHit>> {
        let per_page = query.per_page.clamp(1, MAX_PER_PAGE).to_string();
        let mut params: Vec<(&str, &str)> =
            vec![("type", "release"), ("artist", query.artist.trim()), ("track", query.track.trim())];
        if let Some(label) = query.label.as_deref().map(str::trim).filter(|l| !l.is_empty()) {
            params.push(("label", label));
        }
        if let Some(catno) = query.catno.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            params.push(("catno", catno));
        }
        params.push(("per_page", &per_page));
        let body = self.api_get(&format!("{BASE_URL}/database/search?{}", encode_form(&params)))?;
        let page: WireSearch = parse(&body)?;
        Ok(page.results.into_iter().filter(|r| r.kind.is_empty() || r.kind == "release").map(WireHit::into_hit).collect())
    }

    /// Reads one release.
    pub fn release(&self, id: u64) -> Result<DiscogsRelease> {
        let body = self.api_get(&format!("{BASE_URL}/releases/{id}"))?;
        let release: WireRelease = parse(&body)?;
        Ok(release.into_release())
    }

    /// Downloads a release image from a URI that a release gave
    /// ([`DiscogsImage::uri`]). Only `https` on `i.discogs.com` (no token)
    /// or `api.discogs.com` (with it) is accepted; any other URL is refused
    /// before a request is made.
    pub fn image(&self, uri: &str) -> Result<FetchedImage> {
        let request = match https_host(uri) {
            Some(IMAGE_HOST) => Request::get(uri).header("User-Agent", &self.user_agent),
            Some(API_HOST) => self.api_request(uri),
            _ => {
                return Err(IdentifyError::Malformed {
                    service: SERVICE,
                    reason: format!("image URL is not on {IMAGE_HOST}"),
                });
            }
        };
        let response = ratelimit::send(&*self.transport, &self.limiter, &*self.clock, SERVICE, &request)?;
        checked_image(SERVICE, uri, response.body)
    }

    /// A request to the API host: the only kind that carries the token.
    fn api_request(&self, url: &str) -> Request {
        Request::get(url)
            .header("User-Agent", &self.user_agent)
            .header("Accept", "application/vnd.discogs.v2.discogs+json")
            .header("Authorization", &format!("Discogs token={}", self.token.expose()))
    }

    fn api_get(&self, url: &str) -> Result<Vec<u8>> {
        debug_assert_eq!(https_host(url), Some(API_HOST));
        if self.is_refused() {
            return Err(IdentifyError::Service { service: SERVICE, message: TOKEN_REFUSED.to_owned() });
        }
        let request = self.api_request(url);
        match ratelimit::send(&*self.transport, &self.limiter, &*self.clock, SERVICE, &request) {
            // 401 only: a 403 can be about one resource, not the token.
            Err(IdentifyError::Status { status: 401, .. }) => {
                self.refused.store(true, Ordering::Relaxed);
                Err(IdentifyError::Service { service: SERVICE, message: TOKEN_REFUSED.to_owned() })
            }
            other => Ok(other?.body),
        }
    }

    /// Whether Discogs has refused the token in this run. Callers skip
    /// Discogs entirely once it has.
    pub fn is_refused(&self) -> bool {
        self.refused.load(Ordering::Relaxed)
    }
}

/// The host of an `https://` URL with no user info and no port; `None` for
/// anything else, so a URL like `https://i.discogs.com@evil.example/` or
/// `https://i.discogs.com:8443/` matches no allowed host.
fn https_host(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://")?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    (!host.is_empty() && !host.contains(['@', ':', '\\'])).then_some(host)
}

/// A name with Discogs' numeric disambiguation suffix removed:
/// `Floe (2)` → `Floe`. Other parentheses (`Röyksopp (Live)`) stay.
pub fn strip_artist_suffix(name: &str) -> &str {
    let name = name.trim();
    let Some(open) = name.rfind(" (") else { return name };
    match name[open + 2..].strip_suffix(')') {
        Some(number) if !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) => &name[..open],
        _ => name,
    }
}

/// A Discogs duration, `6:42` or `1:02:03`, in seconds. Empty or unreadable
/// is `None`.
pub fn parse_duration(text: &str) -> Option<u32> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let parts: Vec<u32> = text.split(':').map(|p| p.trim().parse::<u32>().ok()).collect::<Option<_>>()?;
    let (hours, minutes, seconds) = match parts.as_slice() {
        [m, s] => (0, *m, *s),
        [h, m, s] if *m < 60 => (*h, *m, *s),
        _ => return None,
    };
    if seconds >= 60 {
        return None;
    }
    hours.checked_mul(3600)?.checked_add(minutes.checked_mul(60)?)?.checked_add(seconds)
}

fn parse<'a, T: Deserialize<'a>>(body: &'a [u8]) -> Result<T> {
    serde_json::from_slice(body).map_err(|e| IdentifyError::Malformed { service: SERVICE, reason: e.to_string() })
}

/// Reads `null` as the type's default.
fn nullable<'de, D: Deserializer<'de>, T: Default + Deserialize<'de>>(deserializer: D) -> std::result::Result<T, D::Error> {
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// A year as Discogs sends it: a string in search results (`"2014"`), a
/// number in a release (`2014`), `0` or empty when unknown.
fn year<'de, D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Option<u16>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Wire {
        Number(u64),
        Text(String),
    }
    let year = match Option::<Wire>::deserialize(deserializer)? {
        Some(Wire::Number(n)) => u16::try_from(n).ok(),
        Some(Wire::Text(text)) => text.trim().parse::<u16>().ok(),
        None => None,
    };
    Ok(year.filter(|y| *y != 0))
}

fn names(artists: Vec<WireArtist>) -> Vec<String> {
    artists.into_iter().map(|a| strip_artist_suffix(&a.name).to_owned()).filter(|n| !n.is_empty()).collect()
}

#[derive(Deserialize)]
struct WireSearch {
    #[serde(default, deserialize_with = "nullable")]
    results: Vec<WireHit>,
}

#[derive(Deserialize)]
struct WireHit {
    #[serde(default, deserialize_with = "nullable")]
    id: u64,
    #[serde(default, rename = "type", deserialize_with = "nullable")]
    kind: String,
    #[serde(default, deserialize_with = "nullable")]
    title: String,
    #[serde(default, deserialize_with = "year")]
    year: Option<u16>,
    #[serde(default, deserialize_with = "nullable")]
    label: Vec<String>,
    #[serde(default, deserialize_with = "nullable")]
    catno: String,
    #[serde(default, deserialize_with = "nullable")]
    style: Vec<String>,
    #[serde(default, deserialize_with = "nullable")]
    genre: Vec<String>,
    #[serde(default, deserialize_with = "nullable")]
    format: Vec<String>,
    #[serde(default, deserialize_with = "nullable")]
    country: String,
}

impl WireHit {
    fn into_hit(self) -> DiscogsHit {
        DiscogsHit {
            release_id: self.id,
            title: self.title,
            year: self.year,
            labels: self.label,
            catno: self.catno,
            styles: self.style,
            genres: self.genre,
            formats: self.format,
            country: self.country,
        }
    }
}

#[derive(Deserialize)]
struct WireRelease {
    #[serde(default, deserialize_with = "nullable")]
    id: u64,
    #[serde(default, deserialize_with = "nullable")]
    title: String,
    #[serde(default, deserialize_with = "year")]
    year: Option<u16>,
    #[serde(default, deserialize_with = "nullable")]
    released: String,
    #[serde(default, deserialize_with = "nullable")]
    artists: Vec<WireArtist>,
    #[serde(default, deserialize_with = "nullable")]
    labels: Vec<WireLabel>,
    #[serde(default, deserialize_with = "nullable")]
    styles: Vec<String>,
    #[serde(default, deserialize_with = "nullable")]
    genres: Vec<String>,
    #[serde(default, deserialize_with = "nullable")]
    tracklist: Vec<WireTrack>,
    #[serde(default, deserialize_with = "nullable")]
    images: Vec<WireImage>,
}

impl WireRelease {
    fn into_release(self) -> DiscogsRelease {
        let mut tracklist = Vec::with_capacity(self.tracklist.len());
        flatten_tracks(self.tracklist, &mut tracklist);
        DiscogsRelease {
            id: self.id,
            title: self.title,
            year: self.year,
            released: self.released,
            artists: names(self.artists),
            labels: self.labels.into_iter().map(|l| DiscogsLabel { name: l.name, catno: l.catno }).collect(),
            styles: self.styles,
            genres: self.genres,
            tracklist,
            images: self
                .images
                .into_iter()
                .map(|i| DiscogsImage { primary: i.kind == "primary", uri: i.uri, width: i.width, height: i.height })
                .collect(),
        }
    }
}

/// Appends the playable tracks of `entries` to `out`: a heading is dropped,
/// an entry with sub-tracks (an index track, or a medley Discogs splits) is
/// replaced by them, and an index without sub-tracks is dropped.
fn flatten_tracks(entries: Vec<WireTrack>, out: &mut Vec<DiscogsTrack>) {
    for entry in entries {
        if entry.kind == "heading" {
            continue;
        }
        if !entry.sub_tracks.is_empty() {
            flatten_tracks(entry.sub_tracks, out);
            continue;
        }
        if entry.kind == "index" {
            continue;
        }
        out.push(DiscogsTrack {
            position: entry.position,
            title: entry.title,
            duration_secs: parse_duration(&entry.duration),
            artists: names(entry.artists),
            extra_artists: entry
                .extraartists
                .into_iter()
                .map(|a| (strip_artist_suffix(&a.name).to_owned(), a.role))
                .filter(|(name, _)| !name.is_empty())
                .collect(),
        });
    }
}

#[derive(Deserialize)]
struct WireArtist {
    #[serde(default, deserialize_with = "nullable")]
    name: String,
    #[serde(default, deserialize_with = "nullable")]
    role: String,
}

#[derive(Deserialize)]
struct WireLabel {
    #[serde(default, deserialize_with = "nullable")]
    name: String,
    #[serde(default, deserialize_with = "nullable")]
    catno: String,
}

#[derive(Deserialize)]
struct WireTrack {
    #[serde(default, deserialize_with = "nullable")]
    position: String,
    /// `track`, `heading` or `index`.
    #[serde(default, rename = "type_", deserialize_with = "nullable")]
    kind: String,
    #[serde(default, deserialize_with = "nullable")]
    title: String,
    #[serde(default, deserialize_with = "nullable")]
    duration: String,
    #[serde(default, deserialize_with = "nullable")]
    artists: Vec<WireArtist>,
    #[serde(default, deserialize_with = "nullable")]
    extraartists: Vec<WireArtist>,
    #[serde(default, deserialize_with = "nullable")]
    sub_tracks: Vec<WireTrack>,
}

#[derive(Deserialize)]
struct WireImage {
    /// `primary` or `secondary`.
    #[serde(default, rename = "type", deserialize_with = "nullable")]
    kind: String,
    #[serde(default, deserialize_with = "nullable")]
    uri: String,
    #[serde(default, deserialize_with = "nullable")]
    width: u32,
    #[serde(default, deserialize_with = "nullable")]
    height: u32,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn durations_are_read_as_seconds() {
        assert_eq!(parse_duration("6:42"), Some(402));
        assert_eq!(parse_duration("1:02:03"), Some(3723));
        assert_eq!(parse_duration("0:59"), Some(59));
        assert_eq!(parse_duration("74:10"), Some(4450), "a long mix may count minutes past 60");
        for unreadable in ["", "  ", "6", "6:", "6:60", "1:60:00", "6:4x", "-1:00", "1:2:3:4"] {
            assert_eq!(parse_duration(unreadable), None, "{unreadable:?}");
        }
    }

    #[test]
    fn the_numeric_disambiguation_suffix_is_removed() {
        assert_eq!(strip_artist_suffix("Floe (2)"), "Floe");
        assert_eq!(strip_artist_suffix("Deepack (12)"), "Deepack");
        assert_eq!(strip_artist_suffix("Recondite"), "Recondite");
        assert_eq!(strip_artist_suffix("Röyksopp (Live)"), "Röyksopp (Live)");
        assert_eq!(strip_artist_suffix("Artist ()"), "Artist ()");
        assert_eq!(strip_artist_suffix("(2)"), "(2)");
        assert_eq!(strip_artist_suffix("Band (2) (3)"), "Band (2)");
    }

    #[test]
    fn years_come_as_text_or_numbers_and_zero_is_unknown() {
        #[derive(Deserialize)]
        struct Y {
            #[serde(default, deserialize_with = "year")]
            year: Option<u16>,
        }
        let read = |json: &str| serde_json::from_str::<Y>(json).unwrap().year;
        assert_eq!(read(r#"{"year":"2014"}"#), Some(2014));
        assert_eq!(read(r#"{"year":2014}"#), Some(2014));
        assert_eq!(read(r#"{"year":0}"#), None);
        assert_eq!(read(r#"{"year":""}"#), None);
        assert_eq!(read(r#"{"year":null}"#), None);
        assert_eq!(read("{}"), None);
    }

    #[test]
    fn headings_and_indexes_are_dropped_and_sub_tracks_flattened() {
        let release: WireRelease = serde_json::from_str(
            r#"{"id": 1, "year": 0, "tracklist": [
                {"position": "", "type_": "heading", "title": "Side A", "duration": ""},
                {"position": "A1", "type_": "track", "title": "One", "duration": "6:42",
                 "artists": [{"name": "Floe (2)"}],
                 "extraartists": [{"name": "Aerofoil", "role": "Remix"}]},
                {"position": "", "type_": "index", "title": "Suite", "duration": "",
                 "sub_tracks": [{"position": "A2a", "type_": "track", "title": "Two", "duration": null},
                                {"position": "A2b", "type_": "track", "title": "Three"}]},
                {"position": "", "type_": "index", "title": "Empty index"}
            ]}"#,
        )
        .unwrap();
        let release = release.into_release();
        assert_eq!(release.year, None);
        let titles: Vec<&str> = release.tracklist.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["One", "Two", "Three"]);
        let one = &release.tracklist[0];
        assert_eq!(one.duration_secs, Some(402));
        assert_eq!(one.artists, ["Floe"]);
        assert_eq!(one.extra_artists, [("Aerofoil".to_owned(), "Remix".to_owned())]);
        assert_eq!(release.tracklist[1].duration_secs, None);
    }

    #[test]
    fn only_discogs_https_hosts_are_recognised() {
        assert_eq!(https_host("https://i.discogs.com/abc/rs:fit/w:600/x.jpg"), Some("i.discogs.com"));
        assert_eq!(https_host("https://api.discogs.com/releases/1"), Some("api.discogs.com"));
        assert_eq!(https_host("https://api.discogs.com?x"), Some("api.discogs.com"));
        for refused in [
            "http://i.discogs.com/x.jpg",
            "https://i.discogs.com@evil.example/x.jpg",
            "https://i.discogs.com:8443/x.jpg",
            "https://",
            "i.discogs.com/x.jpg",
        ] {
            assert!(!matches!(https_host(refused), Some(IMAGE_HOST | API_HOST)), "{refused}");
        }
        assert_eq!(https_host("https://i.discogs.com.evil.example/x"), Some("i.discogs.com.evil.example"));
    }
}
