//! The dry-run evaluation: judge each result against a ground truth built
//! from the files' existing tags, and write the per-file CSV and a summary.
//!
//! A top candidate is **correct** when its artist and base title match the
//! ground truth and the two mix names agree (same, or compatible such as
//! Original vs Extended). The acceptance rule "zero wrong auto-applies" is
//! measured on every candidate at or above the auto-apply threshold,
//! whether or not auto-apply was enabled or would have filled only empty
//! fields: a wrong match that strong must never exist.
//!
//! Discogs-fallback candidates ([`CandidateSource::DiscogsText`]) are judged
//! like any other, and also counted on their own, with how often the
//! release's label agrees with the ground truth's.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use serde::Serialize;

use crate::decide;
use crate::error::{IdentifyError, Result};
use crate::fallback;
use crate::mixname::{self, MixAgreement};
use crate::model::{Candidate, CandidateSource, Decision};
use crate::normalize;
use crate::score;
use crate::pipeline::TrackResult;

/// Artist overlap needed for a match: credits are written differently
/// ("A, B" vs "A & B feat. C") more often than titles are.
const ARTIST_MATCH: f64 = 0.7;
const TITLE_MATCH: f64 = 0.85;

/// One ground-truth row: what the track is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroundTruth {
    pub artist: String,
    /// The full title, mix included.
    pub title: String,
    /// The optional `label` column; empty when absent.
    pub label: String,
    /// `mainstream` and `expect_unidentified`: yes/no, `None` when the cell
    /// (or the column) is empty.
    pub mainstream: Option<bool>,
    pub expect_unidentified: Option<bool>,
}

fn yes_no(cell: &str) -> Option<bool> {
    match cell.trim().to_ascii_lowercase().as_str() {
        "yes" | "y" | "true" | "1" => Some(true),
        "no" | "n" | "false" | "0" => Some(false),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Verdict {
    Correct,
    Wrong,
    Unidentified,
    NoGroundTruth,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Self::Correct => "correct",
            Self::Wrong => "wrong",
            Self::Unidentified => "unidentified",
            Self::NoGroundTruth => "no_ground_truth",
        }
    }
}

/// Reads `ground_truth.csv`, keyed by `file`: the required `file`, `artist`
/// and `title` columns, and the optional `label`, `mainstream` and
/// `expect_unidentified` ones.
pub fn load_ground_truth(path: &Path) -> Result<HashMap<String, GroundTruth>> {
    let text = std::fs::read_to_string(path)?;
    let rows = parse_csv(text.trim_start_matches('\u{feff}'));
    let mut rows = rows.into_iter();
    let header = rows.next().ok_or_else(|| malformed("empty file"))?;
    let optional = |name: &str| header.iter().position(|h| h == name);
    let column = |name: &str| optional(name).ok_or_else(|| malformed(&format!("no `{name}` column")));
    let (file, artist, title) = (column("file")?, column("artist")?, column("title")?);
    let (mainstream, expect_unidentified) = (optional("mainstream"), optional("expect_unidentified"));
    let label = optional("label");
    let cell = |row: &[String], i: usize| row.get(i).cloned().unwrap_or_default();
    let flag = |row: &[String], i: Option<usize>| i.and_then(|i| yes_no(&cell(row, i)));
    Ok(rows
        .filter(|row| row.iter().any(|c| !c.is_empty()))
        .map(|row| {
            let truth = GroundTruth {
                artist: cell(&row, artist),
                title: cell(&row, title),
                label: label.map(|i| cell(&row, i)).unwrap_or_default(),
                mainstream: flag(&row, mainstream),
                expect_unidentified: flag(&row, expect_unidentified),
            };
            (cell(&row, file), truth)
        })
        .collect())
}

fn malformed(reason: &str) -> IdentifyError {
    IdentifyError::Malformed { service: "ground truth", reason: reason.to_owned() }
}

/// Whether the result's top candidate is the ground-truth track.
#[must_use]
pub fn judge(result: &TrackResult, truth: Option<&GroundTruth>) -> Verdict {
    let Some(truth) = truth else { return Verdict::NoGroundTruth };
    if result.decision == Decision::Unidentified {
        return Verdict::Unidentified;
    }
    let Some(best) = result.best() else { return Verdict::Unidentified };
    if is_match(truth, &best.candidate) {
        Verdict::Correct
    } else {
        Verdict::Wrong
    }
}

/// Artist, base title and mix all agree, the candidate's mix taken as
/// scoring sees it (remixers included).
#[must_use]
pub fn is_match(truth: &GroundTruth, candidate: &Candidate) -> bool {
    let ours = mixname::parse_title(&truth.title);
    let theirs = score::candidate_mix(candidate);
    let (our_base, _) = normalize::strip_featuring(&ours.base);
    let (their_base, _) = normalize::strip_featuring(&theirs.base);
    normalize::artist_similarity(&truth.artist, &candidate.artist_credit) >= ARTIST_MATCH
        && normalize::similarity(&our_base, &their_base) >= TITLE_MATCH
        && matches!(mixname::agree(&ours.mix, &theirs.mix), MixAgreement::Same | MixAgreement::Compatible)
}

/// The run's totals.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Summary {
    pub tracks: usize,
    pub failed: usize,
    pub auto_apply: usize,
    pub review: usize,
    pub unidentified: usize,
    pub correct: usize,
    pub wrong: usize,
    /// Top candidates at or above the auto-apply threshold.
    pub strong: usize,
    /// …of which wrong. Acceptance requires zero.
    pub strong_wrong: usize,
    /// Tracks whose winning candidate came from the fingerprint.
    pub by_fingerprint: usize,
    /// Tracks AcoustID matched at all, and those it linked to a recording.
    pub acoustid_matched: usize,
    pub acoustid_with_recordings: usize,
    /// Rows marked `mainstream=yes`, and those with a correct top candidate
    /// (acceptance: ≥ 80%).
    pub mainstream: usize,
    pub mainstream_correct: usize,
    /// Rows marked `expect_unidentified=yes`, and those that got a decision
    /// other than unidentified anyway (acceptance: none).
    pub expect_unidentified: usize,
    pub expect_unidentified_proposed: usize,
    /// Tracks sent to review by the Discogs fallback, and of those the
    /// correct and wrong ones (counted in `correct` and `wrong` too).
    pub discogs_fallback: usize,
    pub discogs_fallback_correct: usize,
    pub discogs_fallback_wrong: usize,
    /// Fallback tracks whose ground truth names a label, and those whose
    /// Discogs release has the same label.
    pub discogs_fallback_with_label: usize,
    pub discogs_fallback_label_agrees: usize,
}

impl Summary {
    /// A file that could not be read. It still counts toward the
    /// mainstream denominator (as not correct), so failures cannot make
    /// the ≥ 80% check look better.
    pub fn add_failure(&mut self, truth: Option<&GroundTruth>) {
        self.tracks += 1;
        self.failed += 1;
        if truth.and_then(|t| t.mainstream) == Some(true) {
            self.mainstream += 1;
        }
    }

    pub fn add(&mut self, result: &TrackResult, verdict: Verdict, truth: Option<&GroundTruth>) {
        self.tracks += 1;
        if truth.and_then(|t| t.mainstream) == Some(true) {
            self.mainstream += 1;
            if verdict == Verdict::Correct {
                self.mainstream_correct += 1;
            }
        }
        if truth.and_then(|t| t.expect_unidentified) == Some(true) {
            self.expect_unidentified += 1;
            if result.decision != Decision::Unidentified {
                self.expect_unidentified_proposed += 1;
            }
        }
        match result.decision {
            Decision::AutoApply => self.auto_apply += 1,
            Decision::Review => self.review += 1,
            Decision::Unidentified => self.unidentified += 1,
        }
        match verdict {
            Verdict::Correct => self.correct += 1,
            Verdict::Wrong => self.wrong += 1,
            Verdict::Unidentified | Verdict::NoGroundTruth => {}
        }
        if result.best().is_some_and(|b| b.score >= decide::AUTO_APPLY_THRESHOLD) {
            self.strong += 1;
            if verdict == Verdict::Wrong {
                self.strong_wrong += 1;
            }
        }
        if result.best().is_some_and(|b| b.score >= decide::REVIEW_THRESHOLD && b.candidate.source == CandidateSource::Fingerprint) {
            self.by_fingerprint += 1;
        }
        if is_discogs_fallback(result) {
            self.discogs_fallback += 1;
            match verdict {
                Verdict::Correct => self.discogs_fallback_correct += 1,
                Verdict::Wrong => self.discogs_fallback_wrong += 1,
                Verdict::Unidentified | Verdict::NoGroundTruth => {}
            }
            if let Some(label) = truth.map(|t| t.label.trim()).filter(|l| !l.is_empty()) {
                self.discogs_fallback_with_label += 1;
                if result.enrichment.as_ref().is_some_and(|e| fallback::same_label(&e.label, label)) {
                    self.discogs_fallback_label_agrees += 1;
                }
            }
        }
        if result.fingerprint.matches > 0 {
            self.acoustid_matched += 1;
        }
        if result.fingerprint.recordings > 0 {
            self.acoustid_with_recordings += 1;
        }
    }

    /// A short human-readable account.
    #[must_use]
    pub fn render(&self) -> String {
        let identified = self.correct + self.wrong;
        let pct = |n: usize, of: usize| if of == 0 { 0.0 } else { 100.0 * f64::from(u32::try_from(n).unwrap_or(u32::MAX)) / f64::from(u32::try_from(of).unwrap_or(u32::MAX)) };
        let mut out = String::new();
        let _ = writeln!(out, "tracks {}  (failed to read: {})", self.tracks, self.failed);
        let _ = writeln!(out, "decisions: auto-apply {}, review {}, unidentified {}", self.auto_apply, self.review, self.unidentified);
        let _ = writeln!(
            out,
            "top candidate vs tags: correct {} / wrong {} of {} identified ({:.0}% correct); {:.0}% of all tracks",
            self.correct,
            self.wrong,
            identified,
            pct(self.correct, identified),
            pct(self.correct, self.tracks)
        );
        let _ = writeln!(out, "score >= {}: {} candidates, {} wrong (must be 0)", decide::AUTO_APPLY_THRESHOLD, self.strong, self.strong_wrong);
        if self.mainstream == 0 {
            let _ = writeln!(out, "mainstream correct: n/a (column not filled)");
        } else {
            let _ = writeln!(
                out,
                "mainstream correct: {} of {} ({:.0}%, must be >= 80%)",
                self.mainstream_correct,
                self.mainstream,
                pct(self.mainstream_correct, self.mainstream)
            );
        }
        if self.expect_unidentified == 0 {
            let _ = writeln!(out, "expected-unidentified given a candidate: n/a (column not filled)");
        } else {
            let _ = writeln!(
                out,
                "expected-unidentified given a candidate: {} of {} (must be 0)",
                self.expect_unidentified_proposed, self.expect_unidentified
            );
        }
        let _ = writeln!(
            out,
            "AcoustID: matched {} tracks, {} with a MusicBrainz recording; winner from the fingerprint on {}",
            self.acoustid_matched, self.acoustid_with_recordings, self.by_fingerprint
        );
        let _ = writeln!(
            out,
            "Discogs fallback (review only): {} tracks, correct {} / wrong {} vs tags; label agrees with ground truth on {} of {} with a label",
            self.discogs_fallback,
            self.discogs_fallback_correct,
            self.discogs_fallback_wrong,
            self.discogs_fallback_label_agrees,
            self.discogs_fallback_with_label
        );
        out
    }
}

/// Whether the track went to review on a Discogs-fallback candidate.
fn is_discogs_fallback(result: &TrackResult) -> bool {
    result.decision != Decision::Unidentified
        && result.best().is_some_and(|b| b.score >= decide::REVIEW_THRESHOLD && b.candidate.source == CandidateSource::DiscogsText)
}

/// The per-file CSV: the spec's columns first (file, top candidate, score,
/// decision, ground-truth match), then the detail behind them: the top
/// candidate's source (`Fingerprint`, `TextSearch`, `DiscogsText`), the
/// fields the proposal would change, and what Discogs added (release ID,
/// label, catalog number, year, styles as the sub-genre). Files that could
/// not be read get a `Failed` row with the error, so every file is listed.
#[must_use]
pub fn csv(rows: &[(String, &TrackResult, Verdict)], failures: &[(String, String)]) -> String {
    let header = [
        "file", "top_candidate", "score", "decision", "ground_truth_match", "existing_artist", "existing_title", "top_source",
        "top_mbid", "acoustid", "duration", "text", "mix", "rejected", "acoustid_matches", "acoustid_best", "acoustid_recordings",
        "runner_up", "runner_up_score", "proposed_fields", "discogs_release_id", "discogs_label", "discogs_catno",
        "discogs_year", "sub_genre", "warnings",
    ];
    let mut out = String::from("\u{feff}");
    push_row(&mut out, header.iter().map(|h| (*h).to_owned()));
    for (file, result, verdict) in rows {
        let best = result.best();
        let runner_up = result.candidates.get(1);
        let enrichment = result.enrichment.as_ref();
        let fmt = |v: f64| format!("{v:.3}");
        push_row(
            &mut out,
            [
                file.clone(),
                best.map(|b| format!("{} - {}", b.candidate.artist_credit, b.candidate.title)).unwrap_or_default(),
                best.map(|b| fmt(b.score)).unwrap_or_default(),
                format!("{:?}", result.decision),
                verdict.as_str().to_owned(),
                result.existing.artist.clone(),
                result.existing.title.clone(),
                best.map(|b| format!("{:?}", b.candidate.source)).unwrap_or_default(),
                best.map(|b| b.candidate.recording_mbid.clone()).unwrap_or_default(),
                best.and_then(|b| b.breakdown.acoustid).map(fmt).unwrap_or_default(),
                best.map(|b| fmt(b.breakdown.duration)).unwrap_or_default(),
                best.map(|b| fmt(b.breakdown.text)).unwrap_or_default(),
                best.map(|b| fmt(b.breakdown.mix)).unwrap_or_default(),
                best.and_then(|b| b.breakdown.rejected.clone()).unwrap_or_default(),
                result.fingerprint.matches.to_string(),
                fmt(result.fingerprint.best_score),
                result.fingerprint.recordings.to_string(),
                runner_up.map(|r| format!("{} - {}", r.candidate.artist_credit, r.candidate.title)).unwrap_or_default(),
                runner_up.map(|r| fmt(r.score)).unwrap_or_default(),
                result
                    .proposal
                    .as_ref()
                    .map(|p| p.changes.iter().map(|c| c.field.as_str()).collect::<Vec<_>>().join("; "))
                    .unwrap_or_default(),
                enrichment.and_then(|e| e.discogs_release_id).map(|id| id.to_string()).unwrap_or_default(),
                enrichment.map(|e| e.label.clone()).unwrap_or_default(),
                enrichment.map(|e| e.catalog_number.clone()).unwrap_or_default(),
                enrichment.and_then(|e| e.year).map(|y| y.to_string()).unwrap_or_default(),
                enrichment.map(|e| e.styles.join("; ")).unwrap_or_default(),
                result.warnings.join(" | "),
            ],
        );
    }
    for (file, error) in failures {
        let mut cells = vec![String::new(); header.len()];
        cells[0].clone_from(file);
        "Failed".clone_into(&mut cells[3]);
        if let Some(last) = cells.last_mut() {
            last.clone_from(error);
        }
        push_row(&mut out, cells);
    }
    out
}

fn push_row(out: &mut String, cells: impl IntoIterator<Item = String>) {
    let line: Vec<String> = cells.into_iter().map(|c| quote(&c)).collect();
    out.push_str(&line.join(","));
    out.push_str("\r\n");
}

/// RFC 4180: quote when the cell holds a comma, quote or line break. Tag
/// text comes from the files, so a cell Excel would run as a formula
/// (`=`, `+`, `-`, `@`) is prefixed with `'`.
fn quote(cell: &str) -> String {
    let cell = if cell.starts_with(['=', '+', '-', '@']) { format!("'{cell}") } else { cell.to_owned() };
    if cell.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", cell.replace('"', "\"\""))
    } else {
        cell
    }
}

/// RFC 4180 rows: quoted cells may hold commas, doubled quotes and line
/// breaks; CRLF and LF both end a row.
#[must_use]
pub fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match (quoted, c) {
            (true, '"') if chars.peek() == Some(&'"') => {
                chars.next();
                cell.push('"');
            }
            (true, '"') => quoted = false,
            (false, '"') => quoted = true,
            (false, ',') => row.push(std::mem::take(&mut cell)),
            (false, '\r') => {}
            (false, '\n') => {
                row.push(std::mem::take(&mut cell));
                rows.push(std::mem::take(&mut row));
            }
            // Anything else, and anything at all inside quotes, is cell text.
            (_, c) => cell.push(c),
        }
    }
    if !cell.is_empty() || !row.is_empty() {
        row.push(cell);
        rows.push(row);
    }
    rows
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn csv_round_trips_awkward_cells() {
        let cells = ["plain", "a, b", "say \"hi\"", "two\nlines", ""];
        let mut out = String::new();
        push_row(&mut out, cells.iter().map(|c| (*c).to_owned()));
        push_row(&mut out, ["x".to_owned()]);
        assert_eq!(parse_csv(&out), vec![cells.iter().map(|c| (*c).to_owned()).collect::<Vec<_>>(), vec!["x".to_owned()]]);
    }

    #[test]
    fn a_last_row_without_a_newline_is_kept() {
        assert_eq!(parse_csv("a,b\nc,d"), vec![vec!["a", "b"], vec!["c", "d"]]);
    }

    #[test]
    fn ground_truth_is_read_by_column_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gt.csv");
        std::fs::write(&path, "\u{feff}file,x,artist,title\r\none.mp3,,Deepack,\"The Music (Extended Mix)\"\r\n").unwrap();
        let truth = load_ground_truth(&path).unwrap();
        assert_eq!(truth["one.mp3"], GroundTruth { artist: "Deepack".into(), title: "The Music (Extended Mix)".into(), ..GroundTruth::default() });
        std::fs::write(&path, "file,artist\r\n").unwrap();
        assert!(load_ground_truth(&path).unwrap_err().to_string().contains("title"));
    }

    #[test]
    fn a_remix_is_not_the_original_and_a_credit_spelling_is_forgiven() {
        let truth = GroundTruth { artist: "Deepack".into(), title: "The Music (Extended Mix)".into(), ..GroundTruth::default() };
        assert!(is_match(&truth, &cand("Deepack", "The Music (Extended Mix)", "", &[])));
        assert!(is_match(&truth, &cand("Deepack", "The Music", "extended mix", &[])));
        assert!(!is_match(&truth, &cand("Deepack", "The Music (Radio Edit)", "", &[])));
        assert!(!is_match(&truth, &cand("Deepack", "The Music (Someone Remix)", "", &[])));
        assert!(!is_match(&truth, &cand("Someone Else", "The Music (Extended Mix)", "", &[])));
        let festa = GroundTruth { artist: "Mastiksoul, The Groove, Sing".into(), title: "Festa Feat The Groove (Original Mix)".into(), ..GroundTruth::default() };
        assert!(is_match(&festa, &cand("Mastiksoul feat. The Groove & Sing", "Festa", "", &[])));
    }

    fn cand(artist: &str, title: &str, disambiguation: &str, remixers: &[&str]) -> Candidate {
        Candidate {
            recording_mbid: "m".into(),
            artists: vec![artist.into()],
            artist_credit: artist.into(),
            title: title.into(),
            disambiguation: disambiguation.into(),
            remixers: remixers.iter().map(|r| (*r).to_owned()).collect(),
            length_ms: None,
            isrcs: Vec::new(),
            genres: Vec::new(),
            releases: Vec::new(),
            acoustid_score: Some(1.0),
            source: CandidateSource::Fingerprint,
        }
    }

    #[test]
    fn judging_sees_remixers_as_scoring_does() {
        let original = GroundTruth { artist: "E&G".into(), title: "Spirit (Original Mix)".into(), ..GroundTruth::default() };
        let remix = GroundTruth { artist: "E&G".into(), title: "Spirit (Aerofoil Remix)".into(), ..GroundTruth::default() };
        let plain_titled_remix = cand("E&G", "Spirit", "", &["Aerofoil"]);
        assert!(!is_match(&original, &plain_titled_remix), "a remix known only by its remixer is not the original");
        assert!(is_match(&remix, &plain_titled_remix));
    }

    #[test]
    fn the_optional_columns_are_read_as_yes_or_no() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gt.csv");
        std::fs::write(&path, "file,artist,title,mainstream,expect_unidentified\r\na.mp3,A,T,yes,\r\nb.mp3,B,U,no,Yes\r\n").unwrap();
        let truth = load_ground_truth(&path).unwrap();
        assert_eq!((truth["a.mp3"].mainstream, truth["a.mp3"].expect_unidentified), (Some(true), None));
        assert_eq!((truth["b.mp3"].mainstream, truth["b.mp3"].expect_unidentified), (Some(false), Some(true)));
    }

    #[test]
    fn unfilled_acceptance_columns_report_not_applicable() {
        let text = Summary::default().render();
        assert!(text.contains("mainstream correct: n/a"), "{text}");
        assert!(text.contains("expected-unidentified given a candidate: n/a"), "{text}");
    }

    #[test]
    fn a_failed_mainstream_file_counts_against_the_check() {
        let mut summary = Summary::default();
        summary.add_failure(Some(&GroundTruth { mainstream: Some(true), ..GroundTruth::default() }));
        summary.add_failure(None);
        assert_eq!((summary.tracks, summary.failed, summary.mainstream, summary.mainstream_correct), (2, 2, 1, 0));
        assert!(summary.render().contains("mainstream correct: 0 of 1"), "{}", summary.render());
    }

    fn fallback_result(discogs_label: &str) -> TrackResult {
        use crate::model::{Enrichment, ExistingTags, Field, FieldChange, Proposal, ScoreBreakdown, Scored, TagSnapshot};
        let mut candidate = cand("Example Artist", "Fallback Track (Extended Mix)", "", &[]);
        candidate.recording_mbid = String::new();
        candidate.acoustid_score = None;
        candidate.source = CandidateSource::DiscogsText;
        let fill = |field: Field, proposed: &str| FieldChange { field, current: String::new(), proposed: proposed.into() };
        TrackResult {
            path: "x.mp3".into(),
            existing: ExistingTags::default(),
            duration_secs: 390.0,
            fingerprint: crate::pipeline::FingerprintOutcome::default(),
            candidates: vec![Scored { candidate, score: 0.85, breakdown: ScoreBreakdown::default() }],
            decision: Decision::Review,
            snapshot: TagSnapshot::default(),
            proposal: Some(Proposal::new(
                vec![fill(Field::CatalogNumber, "=FB001"), fill(Field::SubGenre, "Tech House; Minimal")],
                None,
                false,
            )),
            enrichment: Some(Enrichment {
                discogs_release_id: Some(9_000_102),
                label: discogs_label.into(),
                catalog_number: "=FB001".into(),
                year: Some(2019),
                styles: vec!["Tech House".into(), "Minimal".into()],
                ..Enrichment::default()
            }),
            artwork: None,
            warnings: vec!["MusicBrainz search: down".into()],
        }
    }

    #[test]
    fn the_csv_carries_the_discogs_columns_and_the_proposed_fields() {
        let result = fallback_result("Fallback Records");
        let out = csv(&[("x.mp3".into(), &result, Verdict::Correct)], &[]);
        let rows = parse_csv(out.trim_start_matches('\u{feff}'));
        let column = |name: &str| rows[0].iter().position(|h| h == name).unwrap();
        let row = &rows[1];
        assert_eq!(row.len(), rows[0].len());
        assert_eq!(row[column("top_source")], "DiscogsText");
        assert_eq!(row[column("proposed_fields")], "CatalogNumber; SubGenre");
        assert_eq!(row[column("discogs_release_id")], "9000102");
        assert_eq!(row[column("discogs_label")], "Fallback Records");
        assert_eq!(row[column("discogs_catno")], "'=FB001", "formula cells are still neutralised");
        assert_eq!(row[column("discogs_year")], "2019");
        assert_eq!(row[column("sub_genre")], "Tech House; Minimal");
        assert_eq!(rows[0].last().map(String::as_str), Some("warnings"), "warnings stay last for failure rows");
        assert_eq!(row.last().map(String::as_str), Some("MusicBrainz search: down"));
    }

    #[test]
    fn discogs_fallback_tracks_are_counted_on_their_own() {
        let truth = GroundTruth {
            artist: "Example Artist".into(),
            title: "Fallback Track (Extended Mix)".into(),
            label: "Fallback Records".into(),
            ..GroundTruth::default()
        };
        let mut summary = Summary::default();
        let same = fallback_result("Fallback Records");
        summary.add(&same, judge(&same, Some(&truth)), Some(&truth));
        let other = fallback_result("Other Records");
        summary.add(&other, judge(&other, Some(&truth)), Some(&truth));
        let no_label = GroundTruth { label: String::new(), ..truth.clone() };
        summary.add(&same, judge(&same, Some(&no_label)), Some(&no_label));
        assert_eq!(
            (summary.discogs_fallback, summary.discogs_fallback_correct, summary.discogs_fallback_wrong),
            (3, 3, 0)
        );
        assert_eq!((summary.discogs_fallback_with_label, summary.discogs_fallback_label_agrees), (2, 1));
        assert_eq!((summary.correct, summary.by_fingerprint), (3, 0));
        assert!(summary.render().contains("Discogs fallback (review only): 3 tracks, correct 3 / wrong 0"), "{}", summary.render());
        // An unidentified track with a weak Discogs candidate is not counted.
        let mut weak = fallback_result("Fallback Records");
        weak.decision = Decision::Unidentified;
        weak.candidates[0].score = 0.4;
        let mut summary = Summary::default();
        summary.add(&weak, judge(&weak, Some(&truth)), Some(&truth));
        assert_eq!(summary.discogs_fallback, 0);
    }

    #[test]
    fn the_label_column_is_optional() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gt.csv");
        std::fs::write(&path, "file,artist,title,label\r\na.mp3,A,T,Fraction Records\r\n").unwrap();
        assert_eq!(load_ground_truth(&path).unwrap()["a.mp3"].label, "Fraction Records");
    }

    #[test]
    fn failed_files_get_a_row_and_formulas_are_neutralised() {
        let out = csv(&[], &[("bad.mp3".into(), "=cmd|' /C calc'!A0".into())]);
        let rows = parse_csv(out.trim_start_matches('\u{feff}'));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1][0], "bad.mp3");
        assert_eq!(rows[1][3], "Failed");
        assert!(rows[1].last().unwrap().starts_with("'="), "{:?}", rows[1]);
    }
}
