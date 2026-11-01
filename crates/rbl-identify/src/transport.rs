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

#[derive(Debug, Clone, PartialEq, Eq)]
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
            .build()
            .map_err(|e| IdentifyError::Http { service: "http", reason: e.without_url().to_string() })?;
        Ok(Self { client })
    }
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
        // `without_url`: the URL can hold a key, and must not reach an error.
        let response = builder.send().map_err(|e| IdentifyError::Http { service, reason: e.without_url().to_string() })?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("Retry-After")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let body = response.bytes().map_err(|e| IdentifyError::Http { service, reason: e.without_url().to_string() })?.to_vec();
        Ok(Response { status, body, retry_after })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_values_are_percent_encoded() {
        assert_eq!(
            encode_form(&[("query", "artist:\"Deepack\" AND recording:The Music"), ("fmt", "json")]),
            "query=artist%3A%22Deepack%22%20AND%20recording%3AThe%20Music&fmt=json"
        );
        assert_eq!(percent_encode("Röyksopp & Co"), "R%C3%B6yksopp%20%26%20Co");
    }
}
