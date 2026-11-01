//! A fake transport that replays recorded bodies, shared by the
//! integration tests. No test here makes a live call.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use rbl_identify::Result;
use rbl_identify::transport::{Request, Response, Transport};

pub fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The URL without its scheme and host: `/ws/2/recording?query=...`.
pub fn path_and_query(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.find('/').map_or("/", |i| &rest[i..])
}

/// Answers each request with the body registered for its path and query,
/// and 404 for anything else. Keeps every request it saw.
#[derive(Default)]
pub struct FakeTransport {
    bodies: HashMap<String, Vec<u8>>,
    pub requests: Mutex<Vec<Request>>,
}

impl FakeTransport {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replies to `path_and_query` with a fixture file's bytes.
    #[must_use]
    pub fn route(mut self, path_and_query: &str, fixture_name: &str) -> Self {
        self.bodies.insert(path_and_query.to_owned(), fixture(fixture_name));
        self
    }

    /// Replies to `path_and_query` with `body`.
    #[must_use]
    pub fn route_body(mut self, path_and_query: &str, body: &[u8]) -> Self {
        self.bodies.insert(path_and_query.to_owned(), body.to_vec());
        self
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

impl Transport for FakeTransport {
    fn send(&self, _service: &'static str, request: &Request) -> Result<Response> {
        self.requests.lock().unwrap().push(request.clone());
        let key = path_and_query(&request.url);
        Ok(match self.bodies.get(key) {
            Some(body) => Response { status: 200, body: body.clone(), retry_after: None },
            None => Response { status: 404, body: format!("no fixture for {key}").into_bytes(), retry_after: None },
        })
    }
}
