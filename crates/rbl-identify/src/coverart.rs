//! The Cover Art Archive client: front covers by release or release group.
//!
//! Cover Art Archive answers every image request with a redirect to
//! archive.org, so the live client ([`CoverArtClient::live`]) runs on the
//! crate-private Cover Art Archive transport, which follows https redirects
//! to `coverartarchive.org` and `archive.org` hosts only. This client holds
//! no secret: no key, no token, only a User-Agent.
//!
//! Every image, here or from Discogs, is checked before it is returned: it
//! must be a JPEG or a PNG by its magic bytes (the `Content-Type` is not
//! trusted), and between [`MIN_IMAGE_BYTES`] and [`MAX_IMAGE_BYTES`] long.

use std::sync::Arc;

use crate::clock::Clock;
use crate::error::{IdentifyError, Result};
use crate::ratelimit::{self, RateLimiter};
use crate::transport::{HttpTransport, Request, Transport};

const SERVICE: &str = "coverartarchive";

/// The service root.
pub const BASE_URL: &str = "https://coverartarchive.org";

/// The thumbnail size asked for: 500 px on the long side, which is enough
/// for embedded artwork and keeps the download small.
pub const FRONT_SIZE: &str = "front-500";

/// Smaller than this is not artwork: an error page or a placeholder.
pub const MIN_IMAGE_BYTES: usize = 1024;

/// Larger than this is refused rather than embedded in a tag.
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

const JPEG_MAGIC: &[u8] = &[0xFF, 0xD8, 0xFF];
const PNG_MAGIC: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// An image downloaded and checked: a JPEG or PNG of acceptable size.
#[derive(Clone, PartialEq, Eq)]
pub struct FetchedImage {
    pub bytes: Vec<u8>,
    /// `image/jpeg` or `image/png`, from the bytes themselves.
    pub mime: String,
    /// The URL it was requested from (before any redirect). Never carries a
    /// secret: neither service puts one in an image URL.
    pub url: String,
}

impl std::fmt::Debug for FetchedImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The length, not up to 10 MiB of bytes.
        f.debug_struct("FetchedImage")
            .field("bytes", &self.bytes.len())
            .field("mime", &self.mime)
            .field("url", &self.url)
            .finish()
    }
}

/// The image type by magic bytes alone: `image/jpeg` or `image/png`.
/// Anything else (GIF, WebP, an HTML error page) is `None`.
#[must_use]
pub fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(JPEG_MAGIC) {
        Some("image/jpeg")
    } else if bytes.starts_with(PNG_MAGIC) {
        Some("image/png")
    } else {
        None
    }
}

/// Checks a downloaded body's size and type and wraps it. Refusals are
/// [`IdentifyError::Malformed`] for `service`.
pub(crate) fn checked_image(service: &'static str, url: &str, bytes: Vec<u8>) -> Result<FetchedImage> {
    let malformed = |reason: String| IdentifyError::Malformed { service, reason };
    if bytes.len() < MIN_IMAGE_BYTES {
        return Err(malformed(format!("image of {} bytes is too small", bytes.len())));
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(malformed(format!("image of {} bytes is over the {MAX_IMAGE_BYTES}-byte limit", bytes.len())));
    }
    let mime = sniff_mime(&bytes).ok_or_else(|| malformed("not a JPEG or PNG image".to_owned()))?;
    Ok(FetchedImage { bytes, mime: mime.to_owned(), url: url.to_owned() })
}

/// True for a UUID in its canonical 8-4-4-4-12 hex form, as every MBID is.
#[must_use]
pub fn is_mbid(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// Fetches front covers, at most one request per second.
pub struct CoverArtClient {
    transport: Arc<dyn Transport>,
    limiter: Arc<RateLimiter>,
    clock: Arc<dyn Clock>,
    user_agent: String,
}

impl std::fmt::Debug for CoverArtClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No User-Agent: it may hold the user's contact address.
        f.debug_struct("CoverArtClient").field("limiter", &self.limiter).finish_non_exhaustive()
    }
}

impl CoverArtClient {
    /// The client the app uses: on the Cover Art Archive transport, which
    /// follows the redirects to archive.org that every image is served
    /// through. `limiter` must be the process's
    /// [`ratelimit::Limiters::coverart`].
    pub fn live(clock: Arc<dyn Clock>, limiter: Arc<RateLimiter>, user_agent: String) -> Result<Self> {
        Ok(Self::new(Arc::new(HttpTransport::for_cover_art()?), clock, limiter, user_agent))
    }

    /// A client on any transport, for tests. A live transport must follow
    /// Cover Art Archive's redirects, or every image comes back as a 307
    /// error; use [`CoverArtClient::live`].
    pub fn new(
        transport: Arc<dyn Transport>,
        clock: Arc<dyn Clock>,
        limiter: Arc<RateLimiter>,
        user_agent: String,
    ) -> Self {
        Self { transport, limiter, clock, user_agent }
    }

    /// The release's front cover at 500 px, or `None` when it has none
    /// (Cover Art Archive answers 404).
    pub fn front(&self, release_mbid: &str) -> Result<Option<FetchedImage>> {
        self.fetch("release", release_mbid)
    }

    /// The front cover Cover Art Archive picks for a release group, or
    /// `None` when no release in the group has one.
    pub fn front_of_group(&self, release_group_mbid: &str) -> Result<Option<FetchedImage>> {
        self.fetch("release-group", release_group_mbid)
    }

    fn fetch(&self, entity: &str, mbid: &str) -> Result<Option<FetchedImage>> {
        // Checked before it reaches a URL; the value itself is not echoed.
        if !is_mbid(mbid) {
            return Err(IdentifyError::Malformed { service: SERVICE, reason: format!("{entity} MBID is not a UUID") });
        }
        let url = format!("{BASE_URL}/{entity}/{mbid}/{FRONT_SIZE}");
        let request = Request::get(&url).header("User-Agent", &self.user_agent);
        match ratelimit::send(&*self.transport, &self.limiter, &*self.clock, SERVICE, &request) {
            Ok(response) => checked_image(SERVICE, &url, response.body).map(Some),
            Err(IdentifyError::Status { status: 404, .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn jpeg(len: usize) -> Vec<u8> {
        let mut bytes = JPEG_MAGIC.to_vec();
        bytes.resize(len, 0);
        bytes
    }

    #[test]
    fn jpeg_and_png_are_sniffed_by_their_magic() {
        assert_eq!(sniff_mime(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10]), Some("image/jpeg"));
        assert_eq!(sniff_mime(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0]), Some("image/png"));
    }

    #[test]
    fn anything_else_is_not_an_image() {
        for body in [&b"GIF89a"[..], b"RIFF\0\0\0\0WEBP", b"<!DOCTYPE html>", b"", &[0xFF, 0xD8], &PNG_MAGIC[..7]] {
            assert_eq!(sniff_mime(body), None, "{body:?}");
        }
    }

    #[test]
    fn sizes_outside_the_limits_are_refused() {
        assert!(checked_image("t", "u", jpeg(MIN_IMAGE_BYTES)).is_ok());
        assert!(checked_image("t", "u", jpeg(MAX_IMAGE_BYTES)).is_ok());
        for len in [MIN_IMAGE_BYTES - 1, MAX_IMAGE_BYTES + 1] {
            let error = checked_image("t", "u", jpeg(len)).unwrap_err();
            assert!(matches!(error, IdentifyError::Malformed { service: "t", .. }), "{error}");
        }
    }

    #[test]
    fn an_image_of_the_wrong_type_is_malformed() {
        let mut gif = b"GIF89a".to_vec();
        gif.resize(4096, 0);
        let error = checked_image("t", "u", gif).unwrap_err();
        assert!(matches!(error, IdentifyError::Malformed { .. }), "{error}");
    }

    #[test]
    fn a_checked_image_keeps_its_bytes_type_and_url() {
        let image = checked_image("t", "https://coverartarchive.org/x", jpeg(2048)).unwrap();
        assert_eq!((image.mime.as_str(), image.bytes.len(), image.url.as_str()), ("image/jpeg", 2048, "https://coverartarchive.org/x"));
        assert!(!format!("{image:?}").contains("255, 216"), "Debug prints the length, not the bytes");
    }

    #[test]
    fn only_canonical_uuids_are_mbids() {
        assert!(is_mbid("0c59bcac-fac3-461c-aef2-54039114dc8f"));
        assert!(is_mbid("0C59BCAC-FAC3-461C-AEF2-54039114DC8F"));
        for bad in [
            "",
            "0c59bcac-fac3-461c-aef2-54039114dc8",
            "0c59bcac-fac3-461c-aef2-54039114dc8f0",
            "0c59bcacXfac3-461c-aef2-54039114dc8f",
            "0c59bcac-fac3-461c-aef2-54039114dc8g",
            "0c59bcacfac3461caef254039114dc8f0000",
            "../../release/x/front-500?x=0000000000",
            "0c59bcac-fac3-461c-aef2-54039114dc/f",
        ] {
            assert!(!is_mbid(bad), "{bad}");
        }
    }
}
