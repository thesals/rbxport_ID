//! The MusicBrainz client: text search for recordings, and recording
//! lookups with the labels and catalog numbers of their releases.
//!
//! MusicBrainz asks for at most one request per second and a meaningful
//! User-Agent on every request; both are enforced here. The User-Agent can
//! hold the user's contact address, so it is kept out of `Debug` output.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Deserializer};

use crate::clock::Clock;
use crate::error::{IdentifyError, Result};
use crate::model::{Candidate, CandidateSource, ReleaseInfo};
use crate::ratelimit::{self, RateLimiter};
use crate::transport::{Request, Transport, percent_encode};

const SERVICE: &str = "musicbrainz";

/// The web service root.
pub const BASE_URL: &str = "https://musicbrainz.org/ws/2/";

/// What a recording lookup includes. `artists` gives the recording's
/// artist credit; `release-groups` adds each release's group to `releases`;
/// `artist-rels` brings the remixer relationship, which is often the only
/// sign of a remix.
pub const LOOKUP_INC: &str = "artists+releases+release-groups+isrcs+genres+tags+artist-rels";

/// What the release browse includes: the label and catalog number that a
/// recording lookup cannot carry.
pub const BROWSE_INC: &str = "labels+release-groups";

/// The release browse's page size (the service maximum).
pub const BROWSE_LIMIT: u32 = 100;

/// MusicBrainz's placeholder label for self-released records.
const NO_LABEL: &str = "[no label]";
/// MusicBrainz's placeholder catalog number for releases without one.
const NO_CATALOG_NUMBER: &str = "[none]";

/// Searches and looks up recordings, at most one request per second.
pub struct MusicBrainzClient {
    transport: Arc<dyn Transport>,
    limiter: Arc<RateLimiter>,
    clock: Arc<dyn Clock>,
    user_agent: String,
}

impl std::fmt::Debug for MusicBrainzClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No User-Agent: it may hold the user's contact address.
        f.debug_struct("MusicBrainzClient").field("limiter", &self.limiter).finish_non_exhaustive()
    }
}

impl MusicBrainzClient {
    /// `user_agent` is sent on every request, as MusicBrainz requires:
    /// `Application/version ( contact )`. `limiter` must be the process's
    /// MusicBrainz limiter ([`ratelimit::Limiters::musicbrainz`]): MusicBrainz
    /// counts requests per IP, so every client shares one budget.
    pub fn new(
        transport: Arc<dyn Transport>,
        clock: Arc<dyn Clock>,
        limiter: Arc<RateLimiter>,
        user_agent: String,
    ) -> Self {
        Self { transport, limiter, clock, user_agent }
    }

    /// Searches recordings by artist and title; candidates come back in
    /// MusicBrainz's relevance order. Labels and catalog numbers are not in
    /// search results, so the releases carry neither.
    pub fn search_recordings(&self, artist: &str, title: &str, limit: u32) -> Result<Vec<Candidate>> {
        let query = search_query(artist, title);
        let url = format!(
            "{BASE_URL}recording?query={}&limit={}&fmt=json",
            percent_encode(&query),
            limit.clamp(1, 100)
        );
        let body = self.get(&url)?;
        let page: WireSearch = parse(&body)?;
        Ok(page.recordings.into_iter().map(|r| r.into_candidate(None, CandidateSource::TextSearch)).collect())
    }

    /// Looks a recording up by MBID, then browses its releases for labels
    /// and catalog numbers: two requests. `remixers` comes from the
    /// recording's remixer relationships; the title and disambiguation are
    /// exactly as MusicBrainz gives them.
    pub fn lookup_recording(
        &self,
        mbid: &str,
        acoustid_score: Option<f64>,
        source: CandidateSource,
    ) -> Result<Candidate> {
        let id = percent_encode(mbid);
        let body = self.get(&format!("{BASE_URL}recording/{id}?inc={LOOKUP_INC}&fmt=json"))?;
        let recording: WireRecording = parse(&body)?;
        let body = self.get(&format!(
            "{BASE_URL}release?recording={id}&inc={BROWSE_INC}&limit={BROWSE_LIMIT}&fmt=json"
        ))?;
        let browse: WireBrowse = parse(&body)?;
        let remixers = remixers(&recording.relations);
        let mut candidate = recording.into_candidate(acoustid_score, source);
        candidate.remixers = remixers;
        merge_releases(&mut candidate.releases, browse.releases);
        Ok(candidate)
    }

    fn get(&self, url: &str) -> Result<Vec<u8>> {
        let request =
            Request::get(url).header("User-Agent", &self.user_agent).header("Accept", "application/json");
        Ok(ratelimit::send(&*self.transport, &self.limiter, &*self.clock, SERVICE, &request)?.body)
    }
}

/// The Lucene query for an artist and title: each a quoted phrase in its
/// field, an empty one left out.
pub fn search_query(artist: &str, title: &str) -> String {
    let artist = artist.trim();
    let title = title.trim();
    let mut parts = Vec::with_capacity(2);
    if !artist.is_empty() {
        parts.push(format!("artist:\"{}\"", lucene_escape(artist)));
    }
    if !title.is_empty() {
        parts.push(format!("recording:\"{}\"", lucene_escape(title)));
    }
    parts.join(" AND ")
}

/// Escapes Lucene's special characters with a backslash. `&&` and `||`
/// are escaped character by character, which Lucene reads the same way.
pub fn lucene_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(
            c,
            '+' | '-' | '&' | '|' | '!' | '(' | ')' | '{' | '}' | '[' | ']' | '^' | '"' | '~' | '*' | '?' | ':' | '\\' | '/'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn parse<'a, T: Deserialize<'a>>(body: &'a [u8]) -> Result<T> {
    serde_json::from_slice(body).map_err(|e| IdentifyError::Malformed { service: SERVICE, reason: e.to_string() })
}

/// Adds the browse's label and catalog number to the releases the lookup
/// returned, and appends any release the lookup left out.
fn merge_releases(releases: &mut Vec<ReleaseInfo>, browsed: Vec<WireRelease>) {
    let mut by_id: HashMap<String, ReleaseInfo> =
        browsed.into_iter().map(|r| (r.id.clone(), r.into_release())).collect();
    for release in releases.iter_mut() {
        if let Some(found) = by_id.remove(&release.mbid) {
            release.label = found.label;
            release.catalog_number = found.catalog_number;
            if release.release_group_mbid.is_empty() {
                release.release_group_mbid = found.release_group_mbid;
                release.release_group_type = found.release_group_type;
                release.secondary_types = found.secondary_types;
            }
        }
    }
    let mut extra: Vec<ReleaseInfo> = by_id.into_values().collect();
    extra.sort_by(|a, b| a.date.cmp(&b.date).then_with(|| a.mbid.cmp(&b.mbid)));
    releases.extend(extra);
}

/// The names credited as remixer, in the order MusicBrainz lists them.
fn remixers(relations: &[WireRelation]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for relation in relations.iter().filter(|r| r.kind == "remixer") {
        let Some(artist) = &relation.artist else { continue };
        let name = if relation.target_credit.is_empty() { &artist.name } else { &relation.target_credit };
        if !name.is_empty() && !names.contains(name) {
            names.push(name.clone());
        }
    }
    names
}

/// MusicBrainz writes `null` for many absent values; read those as the
/// type's default.
fn nullable<'de, D: Deserializer<'de>, T: Default + Deserialize<'de>>(deserializer: D) -> std::result::Result<T, D::Error> {
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Deserialize)]
struct WireSearch {
    #[serde(default, deserialize_with = "nullable")]
    recordings: Vec<WireRecording>,
}

#[derive(Deserialize)]
struct WireBrowse {
    #[serde(default, deserialize_with = "nullable")]
    releases: Vec<WireRelease>,
}

#[derive(Deserialize)]
struct WireRecording {
    #[serde(default, deserialize_with = "nullable")]
    id: String,
    #[serde(default, deserialize_with = "nullable")]
    title: String,
    #[serde(default, deserialize_with = "nullable")]
    disambiguation: String,
    #[serde(default)]
    length: Option<u32>,
    #[serde(default, rename = "artist-credit", deserialize_with = "nullable")]
    artist_credit: Vec<WireCredit>,
    #[serde(default, deserialize_with = "nullable")]
    isrcs: Vec<String>,
    #[serde(default, deserialize_with = "nullable")]
    releases: Vec<WireRelease>,
    #[serde(default, deserialize_with = "nullable")]
    genres: Vec<WireTag>,
    #[serde(default, deserialize_with = "nullable")]
    tags: Vec<WireTag>,
    /// Present only with `inc=artist-rels`.
    #[serde(default, deserialize_with = "nullable")]
    relations: Vec<WireRelation>,
}

#[derive(Deserialize)]
struct WireRelation {
    #[serde(default, rename = "type", deserialize_with = "nullable")]
    kind: String,
    /// The artist's name as credited on this relationship, when it differs.
    #[serde(default, rename = "target-credit", deserialize_with = "nullable")]
    target_credit: String,
    #[serde(default)]
    artist: Option<WireArtist>,
}

#[derive(Deserialize)]
struct WireArtist {
    #[serde(default, deserialize_with = "nullable")]
    name: String,
}

#[derive(Deserialize)]
struct WireCredit {
    /// The name as credited, which can differ from the artist's own name.
    #[serde(default, deserialize_with = "nullable")]
    name: String,
    #[serde(default, deserialize_with = "nullable")]
    joinphrase: String,
}

#[derive(Deserialize)]
struct WireTag {
    #[serde(default, deserialize_with = "nullable")]
    name: String,
    #[serde(default)]
    count: i64,
}

#[derive(Deserialize)]
struct WireRelease {
    #[serde(default, deserialize_with = "nullable")]
    id: String,
    #[serde(default, deserialize_with = "nullable")]
    title: String,
    #[serde(default, deserialize_with = "nullable")]
    date: String,
    #[serde(default, deserialize_with = "nullable")]
    country: String,
    #[serde(default, rename = "release-group")]
    release_group: Option<WireReleaseGroup>,
    #[serde(default, rename = "label-info", deserialize_with = "nullable")]
    label_info: Vec<WireLabelInfo>,
}

#[derive(Deserialize)]
struct WireReleaseGroup {
    #[serde(default, deserialize_with = "nullable")]
    id: String,
    #[serde(default, rename = "primary-type", deserialize_with = "nullable")]
    primary_type: String,
    #[serde(default, rename = "secondary-types", deserialize_with = "nullable")]
    secondary_types: Vec<String>,
}

#[derive(Deserialize)]
struct WireLabelInfo {
    #[serde(default, rename = "catalog-number", deserialize_with = "nullable")]
    catalog_number: String,
    #[serde(default)]
    label: Option<WireLabel>,
}

#[derive(Deserialize)]
struct WireLabel {
    #[serde(default, deserialize_with = "nullable")]
    name: String,
}

impl WireRecording {
    fn into_candidate(self, acoustid_score: Option<f64>, source: CandidateSource) -> Candidate {
        let artist_credit =
            self.artist_credit.iter().flat_map(|c| [c.name.as_str(), c.joinphrase.as_str()]).collect();
        let genres = if self.genres.is_empty() { self.tags } else { self.genres };
        Candidate {
            recording_mbid: self.id,
            artists: self.artist_credit.into_iter().map(|c| c.name).collect(),
            artist_credit,
            title: self.title,
            disambiguation: self.disambiguation,
            // Search results carry no relationships; a lookup fills this.
            remixers: Vec::new(),
            length_ms: self.length,
            isrcs: self.isrcs,
            genres: ranked(genres),
            releases: self.releases.into_iter().map(WireRelease::into_release).collect(),
            acoustid_score,
            source,
        }
    }
}

/// Tag or genre names, most votes first, then by name.
fn ranked(mut tags: Vec<WireTag>) -> Vec<String> {
    tags.retain(|t| !t.name.is_empty());
    tags.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
    tags.into_iter().map(|t| t.name).collect()
}

impl WireRelease {
    fn into_release(self) -> ReleaseInfo {
        let real_label = |info: &&WireLabelInfo| {
            info.label.as_ref().is_some_and(|l| !l.name.is_empty() && l.name != NO_LABEL)
        };
        let real_catalog = |number: &str| !number.is_empty() && number != NO_CATALOG_NUMBER;
        // The first entry naming a real label, with its own catalog number
        // when it has one; else the first real catalog number on any entry.
        let labelled = self.label_info.iter().find(real_label);
        let label = labelled.and_then(|i| i.label.as_ref()).map(|l| l.name.clone()).unwrap_or_default();
        let catalog_number = labelled
            .map(|i| i.catalog_number.as_str())
            .filter(|n| real_catalog(n))
            .or_else(|| self.label_info.iter().map(|i| i.catalog_number.as_str()).find(|n| real_catalog(n)))
            .unwrap_or_default()
            .to_owned();
        let group = self.release_group.unwrap_or(WireReleaseGroup {
            id: String::new(),
            primary_type: String::new(),
            secondary_types: Vec::new(),
        });
        ReleaseInfo {
            mbid: self.id,
            title: self.title,
            date: self.date,
            country: self.country,
            label,
            catalog_number,
            release_group_mbid: group.id,
            release_group_type: group.primary_type,
            secondary_types: group.secondary_types,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn lucene_specials_are_escaped() {
        assert_eq!(lucene_escape(r#"a+b-c&&d||e!(f){g}[h]^"i"~j*k?l:m\n/o"#), r#"a\+b\-c\&\&d\|\|e\!\(f\)\{g\}\[h\]\^\"i\"\~j\*k\?l\:m\\n\/o"#);
        assert_eq!(lucene_escape("E&G"), r"E\&G");
    }

    #[test]
    fn the_query_quotes_each_field() {
        assert_eq!(search_query("Deepack", "The Music"), r#"artist:"Deepack" AND recording:"The Music""#);
        assert_eq!(
            search_query("E&G", "Spirit (Aerofoil Remix)"),
            r#"artist:"E\&G" AND recording:"Spirit \(Aerofoil Remix\)""#
        );
        assert_eq!(search_query("", " The Music "), r#"recording:"The Music""#);
    }

    #[test]
    fn nulls_read_as_empty() {
        let release: WireRelease = serde_json::from_str(
            r#"{"id":"r","title":"T","date":null,"country":null,"release-group":null,
                "label-info":[{"catalog-number":null,"label":null},
                              {"catalog-number":"[none]","label":{"name":"[no label]"}},
                              {"catalog-number":"CAT 1","label":{"name":null}}]}"#,
        )
        .unwrap();
        let release = release.into_release();
        assert_eq!(release.date, "");
        assert_eq!(release.country, "");
        assert_eq!(release.label, "", "placeholder labels are not labels");
        assert_eq!(release.catalog_number, "CAT 1");
    }

    #[test]
    fn genres_rank_by_votes_and_fall_back_to_tags() {
        let recording: WireRecording = serde_json::from_str(
            r#"{"id":"x","tags":[{"name":"house","count":1},{"name":"trance","count":3}],"genres":[]}"#,
        )
        .unwrap();
        let candidate = recording.into_candidate(None, CandidateSource::TextSearch);
        assert_eq!(candidate.genres, ["trance", "house"]);
    }
}
