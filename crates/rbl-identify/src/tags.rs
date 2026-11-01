//! Reading what a file already says: its tags and its true duration.
//! Read-only; tag writing is Phase 4.

use std::path::Path;

use lofty::config::ParseOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::prelude::ItemKey;
use lofty::probe::Probe;

use crate::error::{IdentifyError, Result};
use crate::model::ExistingTags;

/// A file's tags and the duration of its whole audio stream.
#[derive(Debug, Clone, PartialEq)]
pub struct FileInfo {
    pub tags: ExistingTags,
    /// The whole file, in seconds: AcoustID's `duration` and the duration
    /// factor compare against this, not against the fingerprinted part.
    pub duration_secs: f64,
}

/// Reads the primary tag (or the first there is) and the stream duration.
/// Unlike the library import, an absent title stays empty: identification
/// must know a tag is missing rather than see the filename in its place.
pub fn read(path: &Path) -> Result<FileInfo> {
    // Cover art skipped: it is most of a tag by size and nothing here uses it.
    let tagged = Probe::open(path)
        .map(|probe| probe.options(ParseOptions::new().read_cover_art(false)))
        .and_then(Probe::read)
        .map_err(|e| IdentifyError::Io(std::io::Error::other(format!("{}: {e}", path.display()))))?;
    let duration_secs = tagged.properties().duration().as_secs_f64();
    let mut tags = ExistingTags::default();
    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        let text = |key: &ItemKey| tag.get_string(key).unwrap_or_default().trim().to_owned();
        tags.artist = text(&ItemKey::TrackArtist);
        tags.title = text(&ItemKey::TrackTitle);
        tags.album = text(&ItemKey::AlbumTitle);
        tags.label = text(&ItemKey::Label);
        tags.genre = text(&ItemKey::Genre);
        tags.isrc = text(&ItemKey::Isrc);
        tags.year = tag
            .get_string(&ItemKey::RecordingDate)
            .and_then(|v| v.get(..4).and_then(|y| y.parse().ok()))
            .or_else(|| tag.get_string(&ItemKey::Year).and_then(|v| v.trim().parse().ok()));
    }
    Ok(FileInfo { tags, duration_secs })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_is_an_error_naming_it() {
        let error = read(Path::new("no/such/track.mp3")).unwrap_err().to_string();
        assert!(error.contains("track.mp3"), "{error}");
    }
}
