//! HTTP, behind a trait: the clients are tested against recorded responses
//! and CI makes no live call.

use std::fmt::Write as _;
use std::time::Duration;

use crate::error::{IdentifyError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

/// `Debug` is written by hand: the URL's query and the body can carry a key
/// (AcoustID's `client=` is in the body), so neither is ever printed.
#[derive(Clone, PartialEq, Eq)]
pub struct Request {
    pub method: Method,
    /// The full URL. It may carry a key in its query, so it is never
    /// logged or put into an error.
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// `(content type, bytes)` for a POST.
    pub body: Option<(String, Vec<u8>)>,
}

impl Request {
    #[must_use]
    pub fn get(url: impl Into<String>) -> Self {
        Self { method: Method::Get, url: url.into(), headers: Vec::new(), body: None }
    }

    #[must_use]
    pub fn post_form(url: impl Into<String>, form: &[(&str, &str)]) -> Self {
        Self {
            method: Method::Post,
            url: url.into(),
            headers: Vec::new(),
            body: Some(("application/x-www-form-urlencoded".to_owned(), encode_form(form).into_bytes())),
        }
    }

    #[must_use]
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Scheme, host and path only; header values (a Discogs token) and
        // the body are summarised, never shown.
        let path = self.url.split(['?', '#']).next().unwrap_or_default();
        f.debug_struct("Request")
            .field("method", &self.method)
            .field("url", &path)
            .field("headers", &self.headers.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>())
            .field("body_bytes", &self.body.as_ref().map(|(_, bytes)| bytes.len()))
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
    /// `Retry-After`, in seconds, when the service sent one.
    pub retry_after: Option<Duration>,
}

pub trait Transport: Send + Sync {
    /// Sends one request. `service` names the service in any error.
    fn send(&self, service: &'static str, request: &Request) -> Result<Response>;
}

/// `application/x-www-form-urlencoded`, and the same escaping for queries.
#[must_use]
pub fn encode_form(pairs: &[(&str, &str)]) -> String {
    pairs.iter().map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v))).collect::<Vec<_>>().join("&")
}

/// Percent-encodes everything but the RFC 3986 unreserved characters.
#[must_use]
pub fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// The live transport.
#[derive(Debug, Clone)]
pub struct HttpTransport {
    client: reqwest::blocking::Client,
}

impl HttpTransport {
    pub fn new() -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            // A redirect could carry a key-bearing request to another host.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| http_error("http", e))?;
        Ok(Self { client })
    }

    /// The Cover Art Archive transport, and only that: it follows up to five
    /// redirects, each only over https to `coverartarchive.org` or an
    /// `archive.org` host, because every image is served by a redirect to
    /// archive.org. Crate-private so no client holding a key or token can be
    /// given it; `coverart::CoverArtClient` builds its own.
    #[allow(dead_code, reason = "called by coverart::CoverArtClient, which lands next in Phase 2")]
    pub(crate) fn for_cover_art() -> Result<Self> {
        let policy = reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error("too many redirects")
            } else if redirect_allowed(attempt.url()) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        });
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(policy)
            .build()
            .map_err(|e| http_error("http", e))?;
        Ok(Self { client })
    }
}

/// Where a Cover Art Archive redirect may lead: https, and a host that is
/// `coverartarchive.org` or `archive.org` or one of its subdomains.
fn redirect_allowed(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.host_str().is_some_and(|host| {
            host == "coverartarchive.org" || host == "archive.org" || host.ends_with(".archive.org")
        })
}

/// A transport error without its URL (which may hold a key), but with its
/// cause chain, which does not.
fn http_error(service: &'static str, error: reqwest::Error) -> IdentifyError {
    let error = error.without_url();
    let mut reason = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        let _ = write!(reason, ": {cause}");
        source = cause.source();
    }
    IdentifyError::Http { service, reason }
}

impl Transport for HttpTransport {
    fn send(&self, service: &'static str, request: &Request) -> Result<Response> {
        let mut builder = match request.method {
            Method::Get => self.client.get(&request.url),
            Method::Post => self.client.post(&request.url),
        };
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if let Some((content_type, body)) = &request.body {
            builder = builder.header("Content-Type", content_type).body(body.clone());
        }
        let response = builder.send().map_err(|e| http_error(service, e))?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("Retry-After")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let body = read_capped(response, MAX_BODY_BYTES).map_err(|reason| IdentifyError::Http { service, reason })?;
        Ok(Response { status, body, retry_after })
    }
}

/// The most any response may hold. Every answer the services give is far
/// smaller (JSON well under 1 MiB, a cover at most a few); the cap stops a
/// broken or hostile server, or a redirect target, from filling memory.
pub const MAX_BODY_BYTES: u64 = 16 * 1024 * 1024;

/// Reads a body without ever buffering more than `cap` bytes: a body
/// longer than that is an error, not a truncation.
fn read_capped(source: impl std::io::Read, cap: u64) -> std::result::Result<Vec<u8>, String> {
    use std::io::Read as _;
    let mut body = Vec::new();
    source.take(cap + 1).read_to_end(&mut body).map_err(|e| format!("reading the response: {e}"))?;
    if body.len() as u64 > cap {
        return Err(format!("response larger than {cap} bytes"));
    }
    Ok(body)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_request_never_prints_its_body_query_or_header_values() {
        let request = Request::post_form("https://api.acoustid.org/v2/lookup?client=QUERYKEY", &[("client", "BODYKEY")])
            .header("Authorization", "Discogs token=HEADERKEY");
        let shown = format!("{request:?}");
        for secret in ["QUERYKEY", "BODYKEY", "HEADERKEY"] {
            assert!(!shown.contains(secret), "{shown}");
        }
        assert!(shown.contains("api.acoustid.org/v2/lookup") && shown.contains("Authorization"), "{shown}");
    }

    #[test]
    fn a_body_over_the_cap_is_refused_not_truncated() {
        assert_eq!(read_capped(&[7_u8; 10][..], 10).unwrap(), vec![7; 10]);
        let error = read_capped(&[7_u8; 11][..], 10).unwrap_err();
        assert!(error.contains("larger than 10"), "{error}");
        // An endless body stops at the cap instead of filling memory.
        assert!(read_capped(std::io::repeat(1), 1024).is_err());
    }

    #[test]
    fn cover_art_redirects_stay_on_https_archive_hosts() {
        let allowed = |url: &str| redirect_allowed(&reqwest::Url::parse(url).unwrap());
        assert!(allowed("https://coverartarchive.org/release/x/front-500"));
        assert!(allowed("https://ia800500.us.archive.org/1/items/mbid-x/mbid-x-1_thumb500.jpg"));
        assert!(allowed("https://archive.org/download/mbid-x/x.jpg"));
        assert!(!allowed("http://ia800500.us.archive.org/x.jpg"), "plain http");
        assert!(!allowed("https://archive.org.evil.example/x.jpg"));
        assert!(!allowed("https://notarchive.org/x.jpg"));
        assert!(!allowed("https://api.discogs.com/releases/1"));
    }

    #[test]
    fn form_values_are_percent_encoded() {
        assert_eq!(
            encode_form(&[("query", "artist:\"Deepack\" AND recording:The Music"), ("fmt", "json")]),
            "query=artist%3A%22Deepack%22%20AND%20recording%3AThe%20Music&fmt=json"
        );
        assert_eq!(percent_encode("Röyksopp & Co"), "R%C3%B6yksopp%20%26%20Co");
    }
}
