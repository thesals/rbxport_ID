//! Scoring a candidate against the file: AcoustID score, duration, text
//! similarity and mix agreement.
//!
//! Each factor is in `0..=1` and the total is their weighted sum, with hard
//! rules on top: a duration more than 10 s off rejects the candidate, a mix
//! conflict caps it below the review threshold, and a candidate with no
//! fingerprint behind it, one whose duration cannot be compared or one whose
//! mix cannot be compared is capped below the auto-apply floor
//! ([`crate::decide::AUTO_APPLY_FLOOR`]). Those caps are a second guard: the
//! decision also takes only fingerprint matches.

use crate::mixname::{agree, parse_candidate, parse_title, MixAgreement, MixKind, ParsedTitle};
use crate::model::{Candidate, ExistingTags, ScoreBreakdown, Scored};
use crate::normalize::{artist_similarity, similarity, strip_featuring};

/// How much each factor contributes. They sum to 1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Weights {
    pub acoustid: f64,
    pub duration: f64,
    pub text: f64,
    pub mix: f64,
}

pub const DEFAULT_WEIGHTS: Weights = Weights {
    acoustid: 0.45,
    duration: 0.20,
    text: 0.25,
    mix: 0.10,
};

/// Within this many seconds the duration scores 1.0.
const DURATION_EXACT_SECS: f64 = 2.0;
/// Beyond this many seconds the candidate is rejected outright.
const DURATION_REJECT_SECS: f64 = 10.0;
/// A mix conflict caps the total here, under the 0.5 review threshold.
const MIX_CONFLICT_CAP: f64 = 0.49;
/// The ceiling for a candidate without an AcoustID score: below the
/// auto-apply floor, whatever its source says.
const TEXT_ONLY_CAP: f64 = 0.89;
/// The ceiling when the duration cannot be compared (no length for the
/// candidate, or none for the file): below the auto-apply floor, since the
/// duration is one of the checks that tells versions apart. A perfect
/// fingerprint match without one would otherwise land right on 0.90.
const UNKNOWN_DURATION_CAP: f64 = 0.89;
/// The ceiling when the mixes cannot be compared: below the auto-apply
/// floor ([`crate::decide::AUTO_APPLY_FLOOR`]), so it is never applied
/// unattended.
const UNKNOWN_MIX_CAP: f64 = 0.89;
/// Score for a factor with nothing to compare.
const NEUTRAL: f64 = 0.5;

/// Scores one candidate against the file's tags and duration.
///
/// - **duration**: unknown on either side → 0.5; |Δ| ≤ 2 s → 1.0; falling
///   linearly to 0.0 at 10 s; beyond 10 s the candidate is rejected with a
///   total of 0.0.
/// - **text**: 0.4 × [`artist_similarity`] + 0.6 × title [`similarity`],
///   on titles with featuring clauses and mix descriptors removed. Featured
///   artists found in either title join that side's artist credit, so
///   `Festa Feat The Groove` by `Mastiksoul` matches a `Mastiksoul feat.
///   The Groove` credit. With only one of artist or title tagged, that one
///   alone; with neither, 0.5.
/// - **mix**: [`agree`] on the file's and the candidate's mix: Same 1.0,
///   Compatible 0.8, Unknown 0.5, Conflict 0.0. A conflict also caps the
///   total at 0.49, so a remix can never be proposed for the original, and
///   an unknown agreement (a radio edit against an untagged mix, say) caps
///   it at 0.89, under the auto-apply floor, so it can be reviewed but never
///   auto-applied.
/// - **acoustid**: the AcoustID score as is.
///
/// Design decision: a text-search candidate (no AcoustID score) is scored
/// over the other three factors with their weights renormalised, and its
/// total is capped at 0.89, under the auto-apply floor. Matching text and
/// duration alone is not proof enough to change tags unattended, so such a
/// candidate can reach review but never be auto-applied (auto-apply also
/// takes fingerprint matches only). Likewise a candidate whose duration
/// cannot be compared is capped at 0.89.
///
/// The duration rejection takes precedence over the mix cap; a cap or
/// rejection that changed the total is explained in `breakdown.rejected`.
pub fn score(existing: &ExistingTags, file_duration_secs: f64, candidate: Candidate) -> Scored {
    let weights = DEFAULT_WEIGHTS;
    let (duration, duration_delta) = duration_score(file_duration_secs, candidate.length_ms);
    let file_title = parse_title(&existing.title);
    let cand_title = candidate_mix(&candidate);
    let text = text_score(existing, &file_title, &candidate, &cand_title);
    let agreement = agree(&file_title.mix, &cand_title.mix);
    let mix = match agreement {
        MixAgreement::Same => 1.0,
        MixAgreement::Compatible => 0.8,
        MixAgreement::Unknown => NEUTRAL,
        MixAgreement::Conflict => 0.0,
    };
    let acoustid = candidate.acoustid_score.map(unit);

    let others = weights.duration * duration + weights.text * text + weights.mix * mix;
    let mut total = acoustid.map_or_else(
        || others / (weights.duration + weights.text + weights.mix),
        |a| weights.acoustid * a + others,
    );
    total = unit(total);

    let mut rejected = None;
    if let Some(delta) = duration_delta.filter(|d| *d > DURATION_REJECT_SECS) {
        total = 0.0;
        rejected = Some(format!("duration differs by {delta:.1} s"));
    } else if agreement == MixAgreement::Conflict {
        total = total.min(MIX_CONFLICT_CAP);
        rejected = Some(format!(
            "mix conflict: {} vs {}",
            mix_label(&file_title),
            mix_label(&cand_title)
        ));
    } else if acoustid.is_none() && total > TEXT_ONLY_CAP {
        total = TEXT_ONLY_CAP;
        rejected = Some("text search only: capped at 0.89".to_owned());
    } else if duration_delta.is_none() && total > UNKNOWN_DURATION_CAP {
        total = UNKNOWN_DURATION_CAP;
        rejected = Some("duration unknown: capped at 0.89".to_owned());
    } else if agreement == MixAgreement::Unknown && total > UNKNOWN_MIX_CAP {
        total = UNKNOWN_MIX_CAP;
        rejected = Some(format!(
            "mix unknown: {} vs {}: capped at 0.89",
            mix_label(&file_title),
            mix_label(&cand_title)
        ));
    }

    Scored {
        candidate,
        score: total,
        breakdown: ScoreBreakdown {
            acoustid,
            duration,
            text,
            mix,
            rejected,
        },
    }
}

/// Scores every candidate and orders them best first.
///
/// Ties are broken by recording MBID, ascending, so the order never depends
/// on the order the services answered in. When a recording appears more
/// than once (a fingerprint and a text-search hit, say), the higher-scoring
/// copy is kept; on an exact tie, the earlier one.
pub fn rank(existing: &ExistingTags, file_duration_secs: f64, candidates: Vec<Candidate>) -> Vec<Scored> {
    let mut scored: Vec<Scored> = candidates
        .into_iter()
        .map(|c| score(existing, file_duration_secs, c))
        .collect();
    // Stable: equal copies of one recording keep their input order.
    scored.sort_by(|a, b| {
        a.candidate
            .recording_mbid
            .cmp(&b.candidate.recording_mbid)
            .then(b.score.total_cmp(&a.score))
    });
    scored.dedup_by(|later, kept| later.candidate.recording_mbid == kept.candidate.recording_mbid);
    scored.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.candidate.recording_mbid.cmp(&b.candidate.recording_mbid))
    });
    scored
}

/// The duration factor and, when both durations are known, |Δ| in seconds.
fn duration_score(file_secs: f64, length_ms: Option<u32>) -> (f64, Option<f64>) {
    let Some(ms) = length_ms else {
        return (NEUTRAL, None);
    };
    if !file_secs.is_finite() || file_secs <= 0.0 {
        return (NEUTRAL, None);
    }
    let delta = (file_secs - f64::from(ms) / 1000.0).abs();
    let factor = if delta <= DURATION_EXACT_SECS {
        1.0
    } else if delta > DURATION_REJECT_SECS {
        0.0
    } else {
        1.0 - (delta - DURATION_EXACT_SECS) / (DURATION_REJECT_SECS - DURATION_EXACT_SECS)
    };
    (factor, Some(delta))
}

fn text_score(existing: &ExistingTags, file_title: &ParsedTitle, candidate: &Candidate, cand_title: &ParsedTitle) -> f64 {
    let (file_base, file_feat) = strip_featuring(&file_title.base);
    let (cand_base, cand_feat) = strip_featuring(&cand_title.base);
    let has_artist = !existing.artist.trim().is_empty();
    let has_title = !file_base.is_empty();
    let artist = || {
        artist_similarity(
            &with_featured(&existing.artist, &file_feat),
            &with_featured(&candidate.artist_credit, &cand_feat),
        )
    };
    let title = || similarity(&file_base, &cand_base);
    match (has_artist, has_title) {
        (true, true) => 0.4 * artist() + 0.6 * title(),
        (true, false) => artist(),
        (false, true) => title(),
        (false, false) => NEUTRAL,
    }
}

/// A candidate's title and mix exactly as scoring sees them: its title and
/// disambiguation parsed, then its remixers applied. The proposal and the
/// evaluation use this too, so none of them can disagree about whether a
/// candidate is a remix.
#[must_use]
pub fn candidate_mix(candidate: &Candidate) -> ParsedTitle {
    with_remixers(parse_candidate(&candidate.title, &candidate.disambiguation), &candidate.remixers)
}

/// A recording with MusicBrainz remixer relationships is a remix even when
/// its title does not say so: its mix becomes `Remix(<remixers joined by
/// " & ">)`, unless the title or disambiguation already names a version
/// (remix, named dub, edit, VIP), which is kept as more specific. The base
/// title is left alone.
fn with_remixers(mut parsed: ParsedTitle, remixers: &[String]) -> ParsedTitle {
    let names: Vec<&str> = remixers.iter().map(|r| r.trim()).filter(|r| !r.is_empty()).collect();
    let named_version = matches!(
        parsed.mix,
        MixKind::Remix(_) | MixKind::NamedDub(_) | MixKind::Edit(_) | MixKind::Vip
    );
    if names.is_empty() || named_version {
        return parsed;
    }
    let name = names.join(" & ");
    parsed.mix_text = format!("{name} Remix");
    parsed.mix = MixKind::Remix(name);
    parsed
}

/// An artist credit with names featured in the title appended.
fn with_featured(credit: &str, featured: &[String]) -> String {
    if featured.is_empty() {
        return credit.to_owned();
    }
    format!("{credit}, {}", featured.join(", "))
}

fn mix_label(parsed: &ParsedTitle) -> &str {
    if parsed.mix_text.is_empty() {
        "no mix"
    } else {
        &parsed.mix_text
    }
}

/// Clamps to `0..=1`; anything not a number counts as 0.
fn unit(x: f64) -> f64 {
    if x.is_nan() {
        0.0
    } else {
        x.clamp(0.0, 1.0)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::CandidateSource;

    fn tags(artist: &str, title: &str) -> ExistingTags {
        ExistingTags {
            artist: artist.to_owned(),
            title: title.to_owned(),
            ..ExistingTags::default()
        }
    }

    fn cand(mbid: &str, artist: &str, title: &str, length_ms: Option<u32>, acoustid: Option<f64>) -> Candidate {
        Candidate {
            recording_mbid: mbid.to_owned(),
            artists: vec![artist.to_owned()],
            artist_credit: artist.to_owned(),
            title: title.to_owned(),
            disambiguation: String::new(),
            remixers: Vec::new(),
            length_ms,
            isrcs: Vec::new(),
            genres: Vec::new(),
            releases: Vec::new(),
            acoustid_score: acoustid,
            source: if acoustid.is_some() {
                CandidateSource::Fingerprint
            } else {
                CandidateSource::TextSearch
            },
        }
    }

    const FILE_SECS: f64 = 300.0;

    fn music() -> ExistingTags {
        tags("Armin van Buuren", "The Music (Extended Mix)")
    }

    /// A good candidate whose recording lasts `ms`, against a 300 s file.
    fn with_length(ms: u32) -> Scored {
        score(&music(), FILE_SECS, cand("m", "Armin van Buuren", "The Music (Extended Mix)", Some(ms), Some(0.95)))
    }

    #[test]
    fn duration_boundaries() {
        let at2 = with_length(302_000);
        assert!((at2.breakdown.duration - 1.0).abs() < 1e-9);
        let just_over = with_length(302_010);
        assert!(just_over.breakdown.duration < 1.0 && just_over.breakdown.duration > 0.99);
        let at10 = with_length(310_000);
        assert!(at10.breakdown.duration.abs() < 1e-9);
        assert!(at10.breakdown.rejected.is_none());
        assert!(at10.score > 0.5);
        let over = with_length(310_010);
        assert!(over.score.abs() < f64::EPSILON);
        assert_eq!(over.breakdown.rejected.as_deref(), Some("duration differs by 10.0 s"));
        // Shorter counts the same as longer.
        assert!((with_length(294_000).breakdown.duration - 0.5).abs() < 1e-9);
        assert!(with_length(289_500).score.abs() < f64::EPSILON);
    }

    #[test]
    fn unknown_duration_is_neutral() {
        let s = score(&music(), FILE_SECS, cand("m", "Armin van Buuren", "The Music (Extended Mix)", None, Some(0.95)));
        assert!((s.breakdown.duration - 0.5).abs() < f64::EPSILON);
        let s = score(&music(), 0.0, cand("m", "Armin van Buuren", "The Music (Extended Mix)", Some(1), Some(0.95)));
        assert!((s.breakdown.duration - 0.5).abs() < f64::EPSILON);
        assert!(s.breakdown.rejected.is_none());
    }

    #[test]
    fn a_perfect_fingerprint_match_without_a_duration_is_capped_below_auto_apply() {
        // Everything else perfect: 0.45 + 0.20 * 0.5 + 0.25 + 0.10 would be
        // exactly the floor.
        for (file_secs, length) in [(FILE_SECS, None), (0.0, Some(300_000))] {
            let s = score(&music(), file_secs, cand("m", "Armin van Buuren", "The Music (Extended Mix)", length, Some(1.0)));
            assert!((s.score - 0.89).abs() < f64::EPSILON, "{s:?}");
            assert!(s.score < crate::decide::AUTO_APPLY_FLOOR);
            assert_eq!(s.breakdown.rejected.as_deref(), Some("duration unknown: capped at 0.89"));
        }
        // With the duration known it may reach auto-apply.
        let s = score(&music(), FILE_SECS, cand("m", "Armin van Buuren", "The Music (Extended Mix)", Some(300_000), Some(1.0)));
        assert!(s.score >= crate::decide::AUTO_APPLY_FLOOR, "{s:?}");
    }

    #[test]
    fn strong_fingerprint_match_reaches_auto_apply() {
        let s = score(
            &music(),
            FILE_SECS,
            cand("m", "Armin van Buuren", "The Music (Extended Mix)", Some(300_500), Some(0.97)),
        );
        assert!(s.score >= 0.92, "{s:?}");
        assert!(s.breakdown.rejected.is_none());
        assert_eq!(s.breakdown.acoustid, Some(0.97));
        assert!((s.breakdown.mix - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn remix_candidate_for_an_original_mix_file_is_capped() {
        let file = tags("Sander van Doorn", "End of Life (Original Mix)");
        let s = score(
            &file,
            FILE_SECS,
            cand("m", "Sander van Doorn", "End of Life (TC Remix)", Some(300_000), Some(1.0)),
        );
        assert!(s.score <= 0.49, "{s:?}");
        assert!(s.breakdown.mix.abs() < f64::EPSILON);
        assert_eq!(
            s.breakdown.rejected.as_deref(),
            Some("mix conflict: Original Mix vs TC Remix")
        );
        // Through the disambiguation as well.
        let mut c = cand("m", "Sander van Doorn", "End of Life", Some(300_000), Some(1.0));
        c.disambiguation = "TC remix".to_owned();
        assert!(score(&file, FILE_SECS, c).score <= 0.49);
    }

    #[test]
    fn mix_factor_values() {
        let file = music();
        let mix_of = |title: &str| score(&file, FILE_SECS, cand("m", "Armin van Buuren", title, Some(300_000), Some(0.9))).breakdown.mix;
        assert!((mix_of("The Music (Extended Mix)") - 1.0).abs() < f64::EPSILON);
        assert!((mix_of("The Music (Original Mix)") - 0.8).abs() < f64::EPSILON);
        assert!((mix_of("The Music") - 0.8).abs() < f64::EPSILON);
        assert!((mix_of("The Music (Club Mix)") - 0.5).abs() < f64::EPSILON);
        assert!(mix_of("The Music (Radio Edit)").abs() < f64::EPSILON);
    }

    fn remixed(title: &str, remixers: &[&str]) -> Candidate {
        let mut c = cand("m", "Artist", title, Some(300_000), Some(1.0));
        c.remixers = remixers.iter().map(|r| (*r).to_owned()).collect();
        c
    }

    #[test]
    fn remixer_relationships_make_a_remix() {
        // The title does not say remix, MusicBrainz does.
        let s = score(&tags("Artist", "Choctaw Dub (Original Mix)"), FILE_SECS, remixed("Choctaw Dub", &["X"]));
        assert!(s.score <= 0.49, "{s:?}");
        assert_eq!(s.breakdown.rejected.as_deref(), Some("mix conflict: Original Mix vs X Remix"));
        // A plain-titled remix matches the remix file.
        let remix_file = tags("Artist", "Spirit (Aerofoil Remix)");
        let s = score(&remix_file, FILE_SECS, remixed("Spirit", &["Aerofoil"]));
        assert!((s.breakdown.mix - 1.0).abs() < f64::EPSILON, "{s:?}");
        // A title that already names the remix keeps it.
        let titled = score(&remix_file, FILE_SECS, remixed("Spirit (Aerofoil Remix)", &["Aerofoil"]));
        let plain = score(&remix_file, FILE_SECS, remixed("Spirit (Aerofoil Remix)", &[]));
        assert!((titled.score - plain.score).abs() < f64::EPSILON);
        assert_eq!(titled.breakdown, plain.breakdown);
        // ... even when the relationship credits someone else.
        let s = score(&remix_file, FILE_SECS, remixed("Spirit (Aerofoil Remix)", &["Other Person"]));
        assert!((s.breakdown.mix - 1.0).abs() < f64::EPSILON);
        // Several remixers join into one name.
        let pair = tags("Artist", "Spirit (A & B Remix)");
        let s = score(&pair, FILE_SECS, remixed("Spirit", &["A", "B"]));
        assert!((s.breakdown.mix - 1.0).abs() < f64::EPSILON, "{s:?}");
        // A remixer relationship beats a generic cut in the title.
        let s = score(&tags("Artist", "Spirit (Extended Mix)"), FILE_SECS, remixed("Spirit (Extended Mix)", &["X"]));
        assert!(s.score <= 0.49, "{s:?}");
        // A length cut of a remix is still the remix.
        let s = score(&tags("Artist", "Spirit (Original Mix)"), FILE_SECS, remixed("Spirit (Radio Edit)", &["X"]));
        assert!(s.score <= 0.49, "{s:?}");
    }

    #[test]
    fn a_remix_behind_a_non_mix_group_is_still_a_remix() {
        let original = tags("Artist", "Spirit (Original Mix)");
        for title in [
            "Spirit (Aerofoil Remix) (2014 Remaster)",
            "Spirit (Aerofoil Remix) [Free Download]",
            "Spirit (Aerofoil Remix) (Live)",
            "Spirit (Aerofoil Remix) [Bonus Track]",
            "Spirit - Aerofoil Remix - 2014 Remaster",
        ] {
            let s = score(&original, FILE_SECS, remixed(title, &[]));
            assert!(s.score <= 0.49, "candidate {title}: {s:?}");
            // And the other way round: the file is the buried remix.
            let s = score(&tags("Artist", title), FILE_SECS, remixed("Spirit (Original Mix)", &[]));
            assert!(s.score <= 0.49, "file {title}: {s:?}");
        }
    }

    #[test]
    fn album_version_reviews_but_never_auto_applies() {
        let s = score(&music(), FILE_SECS, remixed("The Music (Album Version)", &[]));
        assert!(s.score > 0.5 && s.score < crate::decide::AUTO_APPLY_FLOOR, "{s:?}");
        let radio = tags("Artist", "The Music (Radio Edit)");
        let s = score(&radio, FILE_SECS, remixed("The Music (Album Version)", &[]));
        assert!(s.score <= 0.49, "{s:?}");
    }

    #[test]
    fn unknown_mix_is_capped_below_auto_apply() {
        // A radio edit against a title with no mix: Unknown.
        let file = tags("Armin van Buuren", "The Music");
        let s = score(
            &file,
            FILE_SECS,
            cand("m", "Armin van Buuren", "The Music (Radio Edit)", Some(300_000), Some(1.0)),
        );
        assert!((s.score - 0.89).abs() < f64::EPSILON, "{s:?}");
        assert_eq!(
            s.breakdown.rejected.as_deref(),
            Some("mix unknown: no mix vs Radio Edit: capped at 0.89")
        );
        // Club against Extended likewise.
        let s = score(
            &music(),
            FILE_SECS,
            cand("m", "Armin van Buuren", "The Music (Club Mix)", Some(300_000), Some(1.0)),
        );
        assert!(s.score < crate::decide::AUTO_APPLY_FLOOR, "{s:?}");
    }

    #[test]
    fn other_versions_never_pass_as_the_original() {
        let file = tags("Sander van Doorn", "End of Life (Original Mix)");
        for title in [
            "End of Life (TC Refix)",
            "End of Life (Yan Oxygen Mix)",
            "End of Life (Vocal Mix)",
            "End of Life (TC Remix Radio Edit)",
            "End of Life - TC Remix - Radio Edit",
            "End of Life (Chris Deepak Dub)",
        ] {
            let s = score(&file, FILE_SECS, cand("m", "Sander van Doorn", title, Some(300_000), Some(1.0)));
            assert!(s.score <= 0.49, "{title}: {s:?}");
        }
        for disambiguation in ["remix by X", "X remix, radio edit", "extended remix by X", "2008 edit"] {
            let mut c = cand("m", "Sander van Doorn", "End of Life", Some(300_000), Some(1.0));
            c.disambiguation = disambiguation.to_owned();
            let s = score(&file, FILE_SECS, c);
            assert!(s.score <= 0.49, "{disambiguation}: {s:?}");
        }
        // A remixer's dub is not their remix.
        let remix_file = tags("Sander van Doorn", "End of Life (Chris Deepak Remix)");
        let s = score(
            &remix_file,
            FILE_SECS,
            cand("m", "Sander van Doorn", "End of Life (Chris Deepak Dub)", Some(300_000), Some(1.0)),
        );
        assert!(s.score <= 0.49, "{s:?}");
        // The same remix, however its name is dotted, still matches.
        let tc_file = tags("Sander van Doorn", "End of Life (T.C. Remix)");
        let s = score(
            &tc_file,
            FILE_SECS,
            cand("m", "Sander van Doorn", "End of Life (TC Remix)", Some(300_000), Some(0.97)),
        );
        assert!(s.score >= 0.92, "{s:?}");
    }

    #[test]
    fn text_only_candidate_is_capped_below_auto_apply() {
        let s = score(
            &music(),
            FILE_SECS,
            cand("m", "Armin van Buuren", "The Music (Extended Mix)", Some(300_000), None),
        );
        assert!((s.score - 0.89).abs() < f64::EPSILON, "{s:?}");
        assert!(s.score < crate::decide::AUTO_APPLY_FLOOR, "{s:?}");
        assert!(s.breakdown.acoustid.is_none());
        assert_eq!(s.breakdown.rejected.as_deref(), Some("text search only: capped at 0.89"));
        // Below the cap nothing is noted.
        let weak = score(
            &music(),
            FILE_SECS,
            cand("m", "Armin van Buuren", "The Music (Extended Mix)", Some(306_000), None),
        );
        assert!(weak.score < 0.89 && weak.score > 0.5, "{weak:?}");
        assert!(weak.breakdown.rejected.is_none());
    }

    #[test]
    fn empty_tags_give_neutral_text() {
        let s = score(
            &ExistingTags::default(),
            FILE_SECS,
            cand("m", "Anyone", "Anything (Extended Mix)", Some(300_000), Some(0.9)),
        );
        assert!((s.breakdown.text - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn text_uses_what_is_tagged() {
        let c = || cand("m", "Mastiksoul feat. The Groove", "Festa (Original Mix)", Some(300_000), Some(0.9));
        // Featured artist from the file's title joins its credit.
        let full = score(&tags("Mastiksoul", "Festa Feat The Groove"), FILE_SECS, c());
        assert!((full.breakdown.text - 1.0).abs() < 1e-9, "{full:?}");
        let artist_only = score(&tags("Mastiksoul", ""), FILE_SECS, c());
        assert!(artist_only.breakdown.text > 0.6 && artist_only.breakdown.text < 1.0);
        let title_only = score(&tags("", "Festa"), FILE_SECS, c());
        assert!((title_only.breakdown.text - 1.0).abs() < 1e-9);
        let wrong = score(&tags("Deadmau5", "Strobe"), FILE_SECS, c());
        assert!(wrong.breakdown.text < 0.3, "{wrong:?}");
    }

    #[test]
    fn scores_stay_in_range() {
        let s = score(&music(), FILE_SECS, cand("m", "x", "y", Some(300_000), Some(f64::NAN)));
        assert!((0.0..=1.0).contains(&s.score));
        let s = score(&music(), FILE_SECS, cand("m", "x", "y", Some(300_000), Some(7.0)));
        assert!((0.0..=1.0).contains(&s.score));
    }

    #[test]
    fn rank_orders_breaks_ties_and_dedupes() {
        let file = music();
        let good = |mbid: &str| cand(mbid, "Armin van Buuren", "The Music (Extended Mix)", Some(300_000), Some(0.95));
        let ranked = rank(
            &file,
            FILE_SECS,
            vec![
                cand("a-weak", "Someone", "Else", Some(300_000), Some(0.2)),
                good("c-tie"),
                good("b-tie"),
                // The same recording again, found by text search: lower.
                cand("b-tie", "Armin van Buuren", "The Music (Extended Mix)", Some(300_000), None),
                cand("d-remix", "Armin van Buuren", "The Music (TC Remix)", Some(300_000), Some(0.99)),
            ],
        );
        let order: Vec<&str> = ranked.iter().map(|s| s.candidate.recording_mbid.as_str()).collect();
        assert_eq!(order, ["b-tie", "c-tie", "d-remix", "a-weak"]);
        // The fingerprint copy of b-tie survived, not the text-search one.
        assert_eq!(ranked[0].candidate.source, CandidateSource::Fingerprint);
        assert_eq!(rank(&file, FILE_SECS, Vec::new()).len(), 0);
    }
}
