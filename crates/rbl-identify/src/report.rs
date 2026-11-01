//! The dry-run evaluation: judge each result against a ground truth built
//! from the files' existing tags, and write the per-file CSV and a summary.
//!
//! A top candidate is **correct** when its artist and base title match the
//! ground truth and the two mix names agree (same, or compatible such as
//! Original vs Extended). The acceptance rule "zero wrong auto-applies" is
//! measured on every candidate at or above the auto-apply threshold,
//! whether or not auto-apply was enabled or would have filled only empty
//! fields: a wrong match that strong must never exist.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use serde::Serialize;

use crate::decide;
use crate::error::{IdentifyError, Result};
use crate::mixname::{self, MixAgreement};
use crate::model::{CandidateSource, Decision};
use crate::normalize;
use crate::pipeline::TrackResult;

/// Artist overlap needed for a match: credits are written differently
/// ("A, B" vs "A & B feat. C") more often than titles are.
const ARTIST_MATCH: f64 = 0.7;
const TITLE_MATCH: f64 = 0.85;

/// One ground-truth row: what the track is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroundTruth {
    pub artist: String,
    /// The full title, mix included.
    pub title: String,
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

/// Reads `ground_truth.csv`: the `file`, `artist` and `title` columns, keyed
/// by `file`.
pub fn load_ground_truth(path: &Path) -> Result<HashMap<String, GroundTruth>> {
    let text = std::fs::read_to_string(path)?;
    let rows = parse_csv(text.trim_start_matches('\u{feff}'));
    let mut rows = rows.into_iter();
    let header = rows.next().ok_or_else(|| malformed("empty file"))?;
    let column = |name: &str| header.iter().position(|h| h == name).ok_or_else(|| malformed(&format!("no `{name}` column")));
    let (file, artist, title) = (column("file")?, column("artist")?, column("title")?);
    let cell = |row: &[String], i: usize| row.get(i).cloned().unwrap_or_default();
    Ok(rows
        .filter(|row| row.iter().any(|c| !c.is_empty()))
        .map(|row| (cell(&row, file), GroundTruth { artist: cell(&row, artist), title: cell(&row, title) }))
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
    if is_match(truth, &best.candidate.artist_credit, &best.candidate.title, &best.candidate.disambiguation) {
        Verdict::Correct
    } else {
        Verdict::Wrong
    }
}

/// Artist, base title and mix all agree.
#[must_use]
pub fn is_match(truth: &GroundTruth, artist: &str, title: &str, disambiguation: &str) -> bool {
    let ours = mixname::parse_title(&truth.title);
    let theirs = mixname::parse_candidate(title, disambiguation);
    let (our_base, _) = normalize::strip_featuring(&ours.base);
    let (their_base, _) = normalize::strip_featuring(&theirs.base);
    normalize::artist_similarity(&truth.artist, artist) >= ARTIST_MATCH
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
}

impl Summary {
    pub fn add(&mut self, result: &TrackResult, verdict: Verdict) {
        self.tracks += 1;
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
        let _ = writeln!(
            out,
            "AcoustID: matched {} tracks, {} with a MusicBrainz recording; winner from the fingerprint on {}",
            self.acoustid_matched, self.acoustid_with_recordings, self.by_fingerprint
        );
        out
    }
}

/// The per-file CSV: the spec's columns first (file, top candidate, score,
/// decision, ground-truth match), then the detail behind them.
#[must_use]
pub fn csv(rows: &[(String, &TrackResult, Verdict)]) -> String {
    let header = [
        "file", "top_candidate", "score", "decision", "ground_truth_match", "existing_artist", "existing_title", "top_source",
        "top_mbid", "acoustid", "duration", "text", "mix", "rejected", "acoustid_matches", "acoustid_best", "acoustid_recordings",
        "runner_up", "runner_up_score", "warnings",
    ];
    let mut out = String::from("\u{feff}");
    push_row(&mut out, header.iter().map(|h| (*h).to_owned()));
    for (file, result, verdict) in rows {
        let best = result.best();
        let runner_up = result.candidates.get(1);
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
                result.warnings.join(" | "),
            ],
        );
    }
    out
}

fn push_row(out: &mut String, cells: impl IntoIterator<Item = String>) {
    let line: Vec<String> = cells.into_iter().map(|c| quote(&c)).collect();
    out.push_str(&line.join(","));
    out.push_str("\r\n");
}

/// RFC 4180: quote when the cell holds a comma, quote or line break.
fn quote(cell: &str) -> String {
    if cell.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", cell.replace('"', "\"\""))
    } else {
        cell.to_owned()
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
        assert_eq!(truth["one.mp3"], GroundTruth { artist: "Deepack".into(), title: "The Music (Extended Mix)".into() });
        std::fs::write(&path, "file,artist\r\n").unwrap();
        assert!(load_ground_truth(&path).unwrap_err().to_string().contains("title"));
    }

    #[test]
    fn a_remix_is_not_the_original_and_a_credit_spelling_is_forgiven() {
        let truth = GroundTruth { artist: "Deepack".into(), title: "The Music (Extended Mix)".into() };
        assert!(is_match(&truth, "Deepack", "The Music (Extended Mix)", ""));
        assert!(is_match(&truth, "Deepack", "The Music", "extended mix"));
        assert!(!is_match(&truth, "Deepack", "The Music (Radio Edit)", ""));
        assert!(!is_match(&truth, "Deepack", "The Music (Someone Remix)", ""));
        assert!(!is_match(&truth, "Someone Else", "The Music (Extended Mix)", ""));
        let festa = GroundTruth { artist: "Mastiksoul, The Groove, Sing".into(), title: "Festa Feat The Groove (Original Mix)".into() };
        assert!(is_match(&festa, "Mastiksoul feat. The Groove & Sing", "Festa", ""));
    }
}
