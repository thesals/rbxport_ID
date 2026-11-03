//! The decision: auto-apply, review or unidentified.

use crate::model::{CandidateSource, Decision, Scored};

/// A candidate at or above this may be applied unattended, when the other
/// auto-apply conditions hold.
pub const AUTO_APPLY_THRESHOLD: f64 = 0.92;
/// Below this nothing is proposed: the file is unidentified.
pub const REVIEW_THRESHOLD: f64 = 0.5;

/// Decides what may happen to a file given its best candidate.
///
/// Auto-apply needs all four: a fingerprint match (a text search or the
/// Discogs fallback is never proof enough, whatever its score), a score at
/// or above `threshold` (the user's setting; never below
/// [`AUTO_APPLY_THRESHOLD`], so it can only make auto-apply stricter), a
/// proposal that only fills empty fields
/// (`Proposal::fills_only_empty`, the one rule for that), and the user's
/// opt-in (off by default). Below 0.5, or with no
/// candidate, the file is unidentified rather than guessed at. Anything in
/// between goes to review.
pub fn decide(best: Option<&Scored>, fills_only_empty: bool, auto_apply_enabled: bool, threshold: f64) -> Decision {
    let Some(best) = best else {
        return Decision::Unidentified;
    };
    if best.score.is_nan() || best.score < REVIEW_THRESHOLD {
        return Decision::Unidentified;
    }
    let fingerprinted = best.candidate.source == CandidateSource::Fingerprint;
    // A NaN threshold falls back to the floor; anything lower is raised to it.
    let threshold = if threshold.is_nan() { AUTO_APPLY_THRESHOLD } else { threshold.max(AUTO_APPLY_THRESHOLD) };
    if fingerprinted && best.score >= threshold && fills_only_empty && auto_apply_enabled {
        Decision::AutoApply
    } else {
        Decision::Review
    }
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
        assert_eq!(decide(None, true, true, AUTO_APPLY_THRESHOLD), Decision::Unidentified);
        assert_eq!(decide(Some(&scored(0.0)), true, true, AUTO_APPLY_THRESHOLD), Decision::Unidentified);
        assert_eq!(decide(Some(&scored(0.4999)), true, true, AUTO_APPLY_THRESHOLD), Decision::Unidentified);
        assert_eq!(decide(Some(&scored(f64::NAN)), true, true, AUTO_APPLY_THRESHOLD), Decision::Unidentified);
        assert_eq!(decide(Some(&scored(0.5)), true, true, AUTO_APPLY_THRESHOLD), Decision::Review);
        assert_eq!(decide(Some(&scored(0.9199)), true, true, AUTO_APPLY_THRESHOLD), Decision::Review);
        assert_eq!(decide(Some(&scored(0.92)), true, true, AUTO_APPLY_THRESHOLD), Decision::AutoApply);
        assert_eq!(decide(Some(&scored(1.0)), true, true, AUTO_APPLY_THRESHOLD), Decision::AutoApply);
    }

    #[test]
    fn auto_apply_needs_empty_fields_and_opt_in() {
        let best = scored(0.99);
        assert_eq!(decide(Some(&best), false, true, AUTO_APPLY_THRESHOLD), Decision::Review);
        assert_eq!(decide(Some(&best), true, false, AUTO_APPLY_THRESHOLD), Decision::Review);
        assert_eq!(decide(Some(&best), false, false, AUTO_APPLY_THRESHOLD), Decision::Review);
    }

    #[test]
    fn the_threshold_setting_can_only_make_auto_apply_stricter() {
        assert_eq!(decide(Some(&scored(0.95)), true, true, 0.96), Decision::Review);
        assert_eq!(decide(Some(&scored(0.96)), true, true, 0.96), Decision::AutoApply);
        // Below the floor, or not a number: the floor.
        assert_eq!(decide(Some(&scored(0.91)), true, true, 0.5), Decision::Review);
        assert_eq!(decide(Some(&scored(0.92)), true, true, 0.0), Decision::AutoApply);
        assert_eq!(decide(Some(&scored(0.91)), true, true, f64::NAN), Decision::Review);
    }

    #[test]
    fn only_a_fingerprint_match_may_auto_apply() {
        for source in [CandidateSource::TextSearch, CandidateSource::DiscogsText] {
            let mut best = scored(0.99);
            best.candidate.source = source;
            assert_eq!(decide(Some(&best), true, true, AUTO_APPLY_THRESHOLD), Decision::Review, "{source:?}");
        }
    }

}
