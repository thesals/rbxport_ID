//! The decision: auto-apply, review or unidentified.

use crate::model::{Decision, ExistingTags, Scored};

/// A candidate at or above this may be applied unattended, when the other
/// auto-apply conditions hold.
pub const AUTO_APPLY_THRESHOLD: f64 = 0.92;
/// Below this nothing is proposed: the file is unidentified.
pub const REVIEW_THRESHOLD: f64 = 0.5;

/// Decides what may happen to a file given its best candidate.
///
/// Auto-apply needs all three: score ≥ 0.92, a change that only fills empty
/// fields, and the user's opt-in (off by default). Below 0.5, or with no
/// candidate, the file is unidentified rather than guessed at. Anything in
/// between goes to review.
pub fn decide(best: Option<&Scored>, fills_only_empty: bool, auto_apply_enabled: bool) -> Decision {
    let Some(best) = best else {
        return Decision::Unidentified;
    };
    if best.score.is_nan() || best.score < REVIEW_THRESHOLD {
        return Decision::Unidentified;
    }
    if best.score >= AUTO_APPLY_THRESHOLD && fills_only_empty && auto_apply_enabled {
        Decision::AutoApply
    } else {
        Decision::Review
    }
}

/// True when every field the proposal changes is empty in the file now, so
/// applying it overwrites nothing. A whitespace-only tag counts as empty.
pub fn fills_only_empty(existing: &ExistingTags, proposed: &ExistingTags) -> bool {
    let text_ok = |now: &str, new: &str| now == new || now.trim().is_empty();
    text_ok(&existing.artist, &proposed.artist)
        && text_ok(&existing.title, &proposed.title)
        && text_ok(&existing.album, &proposed.album)
        && text_ok(&existing.label, &proposed.label)
        && text_ok(&existing.isrc, &proposed.isrc)
        && text_ok(&existing.genre, &proposed.genre)
        && (existing.year == proposed.year || existing.year.is_none())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::{Candidate, CandidateSource, ScoreBreakdown};

    fn scored(score: f64) -> Scored {
        Scored {
            candidate: Candidate {
                recording_mbid: "m".to_owned(),
                artists: Vec::new(),
                artist_credit: String::new(),
                title: String::new(),
                disambiguation: String::new(),
                remixers: Vec::new(),
                length_ms: None,
                isrcs: Vec::new(),
                genres: Vec::new(),
                releases: Vec::new(),
                acoustid_score: Some(score),
                source: CandidateSource::Fingerprint,
            },
            score,
            breakdown: ScoreBreakdown::default(),
        }
    }

    #[test]
    fn thresholds() {
        assert_eq!(decide(None, true, true), Decision::Unidentified);
        assert_eq!(decide(Some(&scored(0.0)), true, true), Decision::Unidentified);
        assert_eq!(decide(Some(&scored(0.4999)), true, true), Decision::Unidentified);
        assert_eq!(decide(Some(&scored(f64::NAN)), true, true), Decision::Unidentified);
        assert_eq!(decide(Some(&scored(0.5)), true, true), Decision::Review);
        assert_eq!(decide(Some(&scored(0.9199)), true, true), Decision::Review);
        assert_eq!(decide(Some(&scored(0.92)), true, true), Decision::AutoApply);
        assert_eq!(decide(Some(&scored(1.0)), true, true), Decision::AutoApply);
    }

    #[test]
    fn auto_apply_needs_empty_fields_and_opt_in() {
        let best = scored(0.99);
        assert_eq!(decide(Some(&best), false, true), Decision::Review);
        assert_eq!(decide(Some(&best), true, false), Decision::Review);
        assert_eq!(decide(Some(&best), false, false), Decision::Review);
    }

    #[test]
    fn fills_only_empty_fields() {
        let existing = ExistingTags {
            artist: "Armin van Buuren".to_owned(),
            title: "The Music (Extended Mix)".to_owned(),
            genre: "  ".to_owned(),
            ..ExistingTags::default()
        };
        // No change at all.
        assert!(fills_only_empty(&existing, &existing));
        // Filling empty fields.
        let filled = ExistingTags {
            album: "The Music".to_owned(),
            label: "Armind".to_owned(),
            year: Some(2024),
            isrc: "NLF712400001".to_owned(),
            genre: "Trance".to_owned(),
            ..existing.clone()
        };
        assert!(fills_only_empty(&existing, &filled));
        // Changing a filled field, even only its case.
        let retitled = ExistingTags {
            title: "The Music (extended mix)".to_owned(),
            ..filled.clone()
        };
        assert!(!fills_only_empty(&existing, &retitled));
        // Clearing a filled field.
        let cleared = ExistingTags {
            artist: String::new(),
            ..existing.clone()
        };
        assert!(!fills_only_empty(&existing, &cleared));
        // Changing a set year.
        let dated = ExistingTags {
            year: Some(2023),
            ..ExistingTags::default()
        };
        let redated = ExistingTags {
            year: Some(2024),
            ..ExistingTags::default()
        };
        assert!(!fills_only_empty(&dated, &redated));
        assert!(fills_only_empty(&ExistingTags::default(), &redated));
    }
}
