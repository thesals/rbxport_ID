//! Where a sub-genre goes: appended to the comment rekordbox shows.
//!
//! The user's decision (overriding the spec's never-modify-comments rule
//! for this one purpose): Discogs styles are APPENDED to the comment, never
//! replacing or removing what it says. Segments are separated by
//! [`SEPARATOR`] (`"; "`, the same as the styles are joined with), so
//! `Purchased at Beatport.com` becomes
//! `Purchased at Beatport.com; Minimal Techno; Acid`. A style the comment
//! already holds as a segment (case aside) is not added again, which makes
//! appending idempotent.
//!
//! # Which frame
//!
//! In an `ID3v2` tag the comment is the FIRST `COMM` frame whose description
//! is empty, whatever its language; when there is none, a new one is added
//! (language `eng`, empty description). `COMM` frames with a description
//! (`iTunNORM`, `iTunSMPB`, `ID3v1 Comment`, ...) and the `ID3v1` trailer
//! are never read or written as the comment. Evidence, from the user's
//! library (read-only) against the files' own tags:
//!
//! - [OBS] 128 AIFF tracks whose ID3 chunk holds exactly one `COMM`, with an
//!   empty description in `eng`: rekordbox's `djmdContent.Commnt` equals its
//!   text on all 128.
//! - [OBS] 945 AIFF tracks whose only `COMM` is `COMM:ID3v1 Comment`
//!   (`Purchased at Beatport`, cut to 21 characters): `Commnt` equals it on
//!   none of them (382 hold `Purchased at Beatport.com`, the rest something
//!   else or nothing), so rekordbox does not take a described `COMM`.
//! - [ASSUME] the language is ignored (Beatport writes the bogus `Pur`);
//!   no track in the library could show it either way.
//! - [UNKNOWN] which of several description-less `COMM` frames rekordbox
//!   reads (the library has no such track with a comment); the first is
//!   taken, as tag readers generally do.
//! - [UNKNOWN] MP3 specifically: every MP3 in that library has a NULL
//!   `Commnt`, including 47 rekordbox-imported files with a plain `COMM`,
//!   so the MP3 import path could not be observed. AIFF's ID3 chunk is the
//!   same `ID3v2` format, and the rule is applied to both.
//!
//! In other formats the comment is the tag's own comment item (Vorbis
//! `COMMENT`, MP4 `©cmt`), as lofty maps `ItemKey::Comment`.

/// Between the existing text and each appended style.
pub const SEPARATOR: &str = "; ";

/// The segments of a comment, trimmed, empty ones dropped.
fn segments(text: &str) -> impl Iterator<Item = &str> {
    text.split(';').map(str::trim).filter(|s| !s.is_empty())
}

/// `current` with every segment of `addition` it does not already hold
/// appended, in order. `current` is kept exactly as it is; only when it is
/// blank (nothing but whitespace) does the result start with the first
/// appended segment.
#[must_use]
pub fn append(current: &str, addition: &str) -> String {
    let mut out = current.to_owned();
    for segment in segments(addition) {
        if segments(&out).any(|s| s.to_lowercase() == segment.to_lowercase()) {
            continue;
        }
        if out.trim().is_empty() {
            segment.clone_into(&mut out);
        } else {
            out.push_str(SEPARATOR);
            out.push_str(segment);
        }
    }
    out
}

/// The segments of `proposed` that `current` does not hold: what an append
/// adds, so the same addition can be made to the comment as the file holds
/// it at write time without copying any other text into it.
#[must_use]
pub fn added(current: &str, proposed: &str) -> Vec<String> {
    segments(proposed)
        .filter(|p| !segments(current).any(|c| c.to_lowercase() == p.to_lowercase()))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styles_are_appended_after_the_existing_text() {
        assert_eq!(
            append("Purchased at Beatport.com", "Minimal Techno; Acid"),
            "Purchased at Beatport.com; Minimal Techno; Acid"
        );
        assert_eq!(append("", "Tech House"), "Tech House");
        assert_eq!(append("   ", "Tech House"), "Tech House");
    }

    #[test]
    fn appending_is_idempotent_and_never_removes_text() {
        let once = append("Purchased at Beatport.com", "Minimal Techno; Acid");
        assert_eq!(append(&once, "Minimal Techno; Acid"), once);
        assert_eq!(append(&once, "acid"), once, "case aside");
        // A proposal holding the whole new comment appends nothing more.
        assert_eq!(append("Purchased at Beatport.com", &once), once);
        // Whatever is proposed, the existing text survives.
        let edited = append("Note: keep; this", "Deep House");
        assert!(edited.starts_with("Note: keep; this"));
        assert_eq!(append("x", ""), "x");
    }

    #[test]
    fn the_addition_is_what_the_current_text_lacks() {
        assert_eq!(added("Purchased at Beatport.com", "Purchased at Beatport.com; Acid; Techno"), ["Acid", "Techno"]);
        assert_eq!(added("A; B", "a; b"), Vec::<String>::new());
    }
}
