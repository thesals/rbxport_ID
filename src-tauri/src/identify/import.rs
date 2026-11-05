//! The headless import: identifies every track of a COPY of a rekordbox
//! library into the app's identify store, without the app and without
//! writing a single audio file. What the review queue then shows is exactly
//! what an "Identify All Tracks" job would have stored.
//!
//! ```text
//! cargo run --release -p rbxport --example identify_import -- \
//!     --master-db <a COPY of master.db> [--limit N] [--store <dir>] [--report <csv>]
//! ```
//!
//! - `--master-db`: a copy of `master.db`, taken with rekordbox closed. The
//!   installed library itself is refused (the path is compared after
//!   canonicalising both), and so is a copy with a `-journal` or `-wal`
//!   file beside it. The copy is opened read-only and nothing is written
//!   to it or beside it.
//! - `--limit N`: identify at most N tracks this run (after skipping).
//! - `--store <dir>`: the identify data folder; the app's own by default
//!   (`%APPDATA%\rbxport\identify` on Windows), so the app shows the
//!   results.
//! - `--report <csv>`: the per-track CSV, in the dry run's format (its
//!   `ground_truth_match` judged against the file's own artist and title
//!   tags, else the imported rekordbox record's); `target/identify/import_report.csv` by default. The summary is
//!   written beside it (`.summary.txt`) and to stderr.
//!
//! The passphrase comes from rekordbox's agent settings
//! ([`rbl_db::detect`]); only the path is replaced. The keys are resolved
//! as the app resolves them (environment, the data folder's
//! `.env.identify`, a development checkout's), and never printed.
//!
//! **Never writes a file.** It refuses to start while the store's "Write
//! tags to audio files" setting is on (or its settings cannot be read);
//! auto-apply is forced off for the run whatever the settings say; and its
//! state is made never to write ([`IdentifyState::file_writes`] refuses
//! every write), so an auto-apply decision could only ever become a review.
//! It never applies, undoes or retries a waiting write. Fingerprinting
//! only reads the audio.
//!
//! **Resumable.** A track whose file already has a current result in the
//! store (same canonical path, size and modification time) is skipped.
//! Each track is saved the moment it finishes, so stopping with Ctrl-C
//! keeps every finished track; run it again to go on.
//!
//! Progress goes to stderr, one line per track: `n/N`, the decision, the
//! elapsed time and an estimate of the time left.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rbl_identify::pipeline::{Settings, TrackResult};
use rbl_identify::report::{self, GroundTruth, Verdict};
use rbl_identify::store::{RekordboxRecord, TrackKey};

use super::{
    job_tracks, off_runtime, read_isrcs, run_job, store_error, Engine, IdentifyState, Identifier, JobEvent, JobTrack,
    ResultDecision,
};
use crate::error::{AppError, AppResult, ErrorKind};

/// What to import, and where to.
#[derive(Debug, Clone)]
pub struct Options {
    /// A copy of `master.db`, never the installed one.
    pub master_db: PathBuf,
    /// At most this many tracks are identified (after skipping).
    pub limit: Option<usize>,
    /// The identify data folder: the store, keys and settings.
    pub store: PathBuf,
    /// The per-track CSV; the summary goes beside it.
    pub report: PathBuf,
}

impl Options {
    /// The defaults for a copy: the app's own store, the report under
    /// `target/identify/`.
    #[must_use]
    pub fn for_copy(master_db: PathBuf) -> Self {
        Self {
            master_db,
            limit: None,
            store: super::default_root(),
            report: PathBuf::from("target").join("identify").join("import_report.csv"),
        }
    }
}

/// How a run went.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    /// Tracks in the library copy.
    pub library_tracks: usize,
    /// Tracks skipped: their file already had a result in the store.
    pub skipped: usize,
    /// Tracks this run identified (or tried to).
    pub run: usize,
    pub review: usize,
    pub unidentified: usize,
    pub failed: usize,
    /// Always 0: nothing is auto-applied.
    pub auto_applied: usize,
}

/// Identifies the copy's tracks into the store with the live services.
/// `original` is the installed library as [`rbl_db::detect`] found it: its
/// passphrase unlocks the copy, and its path is the one refused.
///
/// # Errors
///
/// A refusal (the original, a copy beside a journal, writes turned on, no
/// keys) or a failure to read the copy or write the report; one message,
/// never a key or the passphrase.
pub fn run(options: &Options, original: &rbl_db::LibraryLocation, progress: &mut dyn Write) -> Result<Summary, String> {
    let state = IdentifyState::never_writing(options.store.clone());
    preflight(&state)?;
    let engine = state.engine_without_auto_apply().map_err(|e| super::message_of(&e))?;
    run_with(options, original, &state, &engine, &AtomicBool::new(false), progress)
}

impl IdentifyState {
    /// State for the import: `root`'s store, keys and settings, read as the
    /// app reads them, but it never writes an audio file.
    pub(crate) fn never_writing(root: PathBuf) -> Self {
        let mut state = Self::with_root(root);
        state.never_write_files = true;
        state
    }

    /// The live services with auto-apply forced off, whatever the settings
    /// say; not cached.
    fn engine_without_auto_apply(&self) -> AppResult<Engine> {
        let config = self.sources.config()?;
        let settings = Settings { auto_apply: false, ..self.sources.stored().pipeline() };
        let (clock, limiters) = (&self.clock, &self.limiters);
        off_runtime(|| Engine::build(&config, settings, clock, limiters))?.map_err(|e| {
            AppError::new(ErrorKind::Internal, "The identification services could not be started.").with_detail(e.to_string())
        })
    }
}

/// Refuses to start unless the store's file-writes setting is readable and
/// off, and the state can never write.
fn preflight(state: &IdentifyState) -> Result<(), String> {
    if !state.never_write_files {
        return Err("the import must run with a state that never writes files".to_owned());
    }
    let stored = state.sources.read_stored().map_err(|e| {
        format!("the identify settings ({}) cannot be read, so the import does not start: {e}", state.root.display())
    })?;
    if stored.file_writes {
        return Err(
            "\"Write tags to audio files\" is turned on for this identify store. Turn it off in Preferences → Identify \
             before importing: the import never writes files."
                .to_owned(),
        );
    }
    Ok(())
}

/// Opens the copy read-only, unlocked with the original's passphrase.
/// Refuses the original itself, and a copy with a journal beside it (whose
/// read-only open could need a writable one to recover).
pub(crate) fn open_copy(copy: &Path, original: &rbl_db::LibraryLocation) -> Result<rbl_db::Library, String> {
    if !copy.is_file() {
        return Err(format!("{} is not a file", copy.display()));
    }
    let canonical = std::fs::canonicalize(copy).map_err(|e| format!("{}: {e}", copy.display()))?;
    let original_path = std::fs::canonicalize(&original.master_db).unwrap_or_else(|_| original.master_db.clone());
    if same_path(&canonical, &original_path) {
        return Err(format!(
            "{} is the installed rekordbox library. Copy master.db somewhere else (with rekordbox closed) and import \
             the copy.",
            copy.display()
        ));
    }
    for suffix in ["-journal", "-wal"] {
        let mut beside = canonical.clone().into_os_string();
        beside.push(suffix);
        if Path::new(&beside).exists() {
            return Err(format!(
                "{} has a {suffix} file beside it, so the copy may be incomplete. Copy master.db again with rekordbox \
                 closed.",
                copy.display()
            ));
        }
    }
    let location = rbl_db::LibraryLocation {
        share_root: canonical.parent().map_or_else(|| PathBuf::from("share"), |p| p.join("share")),
        master_db: canonical,
        passphrase: original.passphrase.clone(),
        is_user_library: false,
        is_rekordbox_install: false,
    };
    let db = rbl_db::Library::open(location, rbl_db::OpenMode::ReadOnly)
        .map_err(|e| format!("the library copy could not be opened read-only: {e}"))?;
    if db.mode() != rbl_db::OpenMode::ReadOnly {
        return Err("the library copy was not opened read-only".to_owned());
    }
    Ok(db)
}

/// Two canonical paths name the same file. Case-insensitive on Windows,
/// where the file system is.
fn same_path(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        a.as_os_str().to_string_lossy().to_lowercase() == b.as_os_str().to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

/// Wraps the identifier to keep each result for the report (without its
/// artwork bytes, which the store keeps).
struct Recording<'a> {
    inner: &'a dyn Identifier,
    results: Mutex<Vec<(PathBuf, TrackResult)>>,
}

impl Identifier for Recording<'_> {
    fn identify(&self, path: &Path) -> rbl_identify::Result<TrackResult> {
        let result = self.inner.identify(path)?;
        let mut kept = result.clone();
        kept.artwork = None;
        self.results.lock().push((path.to_owned(), kept));
        Ok(result)
    }
}

/// [`run`] with any identifier and cancel flag: what the tests drive.
pub(crate) fn run_with(
    options: &Options,
    original: &rbl_db::LibraryLocation,
    state: &IdentifyState,
    identifier: &dyn Identifier,
    cancel: &AtomicBool,
    progress: &mut dyn Write,
) -> Result<Summary, String> {
    preflight(state)?;
    let db = open_copy(&options.master_db, original)?;
    let (library, _) = rbl_index::load(&db).map_err(|e| format!("the library copy could not be read: {e}"))?;
    let ids: Vec<String> = library.ids.iter().map(u64::to_string).collect();
    let tracks = job_tracks(&library, ids, |batch| read_isrcs(&db, batch));
    drop(db);
    let mut summary = Summary { library_tracks: tracks.len(), ..Summary::default() };

    let mut todo = Vec::with_capacity(tracks.len());
    state
        .with_store(|store| {
            for track in tracks {
                let key = track.path.as_deref().and_then(|p| TrackKey::for_file(Path::new(p)).ok());
                match key {
                    Some(key) if store.has_current_result(&key).map_err(store_error)? => summary.skipped += 1,
                    _ => todo.push(track),
                }
            }
            Ok(())
        })
        .map_err(|e| super::message_of(&e))?;
    if let Some(limit) = options.limit {
        todo.truncate(limit);
    }
    let _ = writeln!(
        progress,
        "{} tracks in the library copy; {} already identified, skipped; identifying {}",
        summary.library_tracks,
        summary.skipped,
        todo.len()
    );

    let recording = Recording { inner: identifier, results: Mutex::new(Vec::new()) };
    let started = Instant::now();
    let mut finished = Vec::new();
    run_job(state, 1, &todo, &recording, cancel, &mut |event| {
        let JobEvent::Result(result) = event else { return };
        let done = finished.len() + 1;
        let elapsed = started.elapsed();
        let left = todo.len().saturating_sub(done);
        let eta = elapsed.checked_div(u32::try_from(done).unwrap_or(u32::MAX)).map_or(Duration::ZERO, |each| {
            each.saturating_mul(u32::try_from(left).unwrap_or(u32::MAX))
        });
        let _ = writeln!(
            progress,
            "{done}/{} {:<12} {}  elapsed {}  eta {}  {}",
            todo.len(),
            decision_name(result.decision),
            result.score.map_or_else(|| "    ".to_owned(), |s| format!("{s:.2}")),
            clock(elapsed),
            clock(eta),
            if result.path.is_empty() { result.track_id.as_str() } else { result.path.as_str() }
        );
        finished.push(result);
    });
    summary.run = finished.len();
    for result in &finished {
        match result.decision {
            ResultDecision::AutoApply => summary.auto_applied += 1,
            ResultDecision::Review => summary.review += 1,
            ResultDecision::Unidentified => summary.unidentified += 1,
            ResultDecision::Failed => summary.failed += 1,
        }
    }

    write_report(options, &todo, &finished, &recording.results.into_inner(), &summary, started.elapsed(), progress)?;
    Ok(summary)
}

fn decision_name(decision: ResultDecision) -> &'static str {
    match decision {
        ResultDecision::AutoApply => "auto-apply",
        ResultDecision::Review => "review",
        ResultDecision::Unidentified => "unidentified",
        ResultDecision::Failed => "failed",
    }
}

/// `h:mm:ss`.
fn clock(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// The report's ground truth: the file's own artist and title tags, which
/// identification starts from, or else the rekordbox record's. rekordbox
/// often holds a title taken from the file name (`7413777_Signs_Original_Mix`)
/// that no service returns, so its record is only the fallback.
fn truth_of(result: Option<&TrackResult>, record: Option<&RekordboxRecord>) -> Option<GroundTruth> {
    let usable = |artist: &str, title: &str| !artist.trim().is_empty() && !title.trim().is_empty();
    let truth = |artist: &str, title: &str| GroundTruth {
        artist: artist.to_owned(),
        title: title.to_owned(),
        ..GroundTruth::default()
    };
    result
        .map(|r| &r.existing)
        .filter(|tags| usable(&tags.artist, &tags.title))
        .map(|tags| truth(&tags.artist, &tags.title))
        .or_else(|| record.filter(|r| usable(&r.artist, &r.title)).map(|r| truth(&r.artist, &r.title)))
}

/// The CSV (the dry run's format) and the summary, beside each other; the
/// summary also to `progress`.
fn write_report(
    options: &Options,
    tracks: &[JobTrack],
    finished: &[super::IdentifyResultDto],
    results: &[(PathBuf, TrackResult)],
    summary: &Summary,
    elapsed: Duration,
    progress: &mut dyn Write,
) -> Result<(), String> {
    let mut judged = report::Summary::default();
    let mut rows = Vec::new();
    let mut failures = Vec::new();
    for done in finished {
        let record = tracks.iter().find(|t| t.track_id == done.track_id).and_then(|t| t.rekordbox.as_ref());
        let result = results.iter().find(|(path, _)| path.as_os_str() == done.path.as_str()).map(|(_, r)| r);
        let truth = truth_of(result, record);
        match (done.error.as_ref(), result) {
            (None, Some(result)) => {
                let verdict = report::judge(result, truth.as_ref());
                judged.add(result, verdict, truth.as_ref());
                rows.push((done.path.clone(), result, verdict));
            }
            (error, _) => {
                judged.add_failure(truth.as_ref());
                let file = if done.path.is_empty() { format!("track {}", done.track_id) } else { done.path.clone() };
                failures.push((file, error.cloned().unwrap_or_else(|| "not identified".to_owned())));
            }
        }
    }
    let rows: Vec<(String, &TrackResult, Verdict)> = rows.into_iter().collect();
    if let Some(dir) = options.report.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(&options.report, report::csv(&rows, &failures))
        .map_err(|e| format!("{}: {e}", options.report.display()))?;
    let text = format!(
        "library tracks {}, skipped (already identified) {}, identified this run {}\n\
         this run: review {}, unidentified {}, failed {}, auto-applied {} (files written: none)\n\
         {}elapsed {}\n",
        summary.library_tracks,
        summary.skipped,
        summary.run,
        summary.review,
        summary.unidentified,
        summary.failed,
        summary.auto_applied,
        judged.render(),
        clock(elapsed)
    );
    let summary_path = options.report.with_extension("summary.txt");
    std::fs::write(&summary_path, &text).map_err(|e| format!("{}: {e}", summary_path.display()))?;
    let _ = writeln!(progress, "\n{text}report: {}\nsummary: {}", options.report.display(), summary_path.display());
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::identify::QueueFilter;
    use rbl_identify::model::{
        Candidate, CandidateSource, Decision, ExistingTags, Field, FieldChange, Proposal, ScoreBreakdown, Scored,
        TagSnapshot,
    };
    use rbl_identify::pipeline::FingerprintOutcome;

    const TEST_SET: &str = r"C:\dev\id-testset";

    /// Answers by file name, without the network: `auto*` → an auto-apply
    /// that fills the empty title, `unknown*` → unidentified, anything else
    /// → review.
    struct Fake {
        calls: AtomicU32,
    }

    impl Identifier for Fake {
        fn identify(&self, path: &Path) -> rbl_identify::Result<TrackResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let name = path.file_name().unwrap().to_string_lossy().to_lowercase();
            let decision = if name.starts_with("auto") {
                Decision::AutoApply
            } else if name.starts_with("unknown") {
                Decision::Unidentified
            } else {
                Decision::Review
            };
            let identified = decision != Decision::Unidentified;
            let candidate = Scored {
                candidate: Candidate {
                    recording_mbid: "41f4b491-cc31-40f1-8d81-cc58b7d8f761".into(),
                    artists: vec!["Deepack".into()],
                    artist_credit: "Deepack".into(),
                    title: "The Music (Extended Mix)".into(),
                    disambiguation: String::new(),
                    remixers: Vec::new(),
                    length_ms: Some(300_000),
                    isrcs: Vec::new(),
                    genres: Vec::new(),
                    releases: Vec::new(),
                    acoustid_score: Some(0.95),
                    source: CandidateSource::Fingerprint,
                },
                score: 0.95,
                breakdown: ScoreBreakdown { acoustid: Some(0.95), duration: 1.0, text: 0.9, mix: 1.0, rejected: None },
            };
            Ok(TrackResult {
                path: path.to_owned(),
                existing: ExistingTags::default(),
                duration_secs: 300.0,
                fingerprint: FingerprintOutcome::default(),
                encoded_fingerprint: None,
                candidates: if identified { vec![candidate] } else { Vec::new() },
                decision,
                snapshot: TagSnapshot::default(),
                proposal: identified.then(|| {
                    Proposal::new(
                        vec![FieldChange { field: Field::Title, current: String::new(), proposed: "The Music".into() }],
                        None,
                        true,
                    )
                }),
                enrichment: None,
                artwork: None,
                warnings: Vec::new(),
            })
        }
    }

    /// One second of silent 16-bit mono WAV.
    fn wav(path: &Path) {
        let rate = 44_100_u32;
        let data_len = rate * 2;
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16_u32.to_le_bytes());
        out.extend_from_slice(&1_u16.to_le_bytes());
        out.extend_from_slice(&1_u16.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2).to_le_bytes());
        out.extend_from_slice(&2_u16.to_le_bytes());
        out.extend_from_slice(&16_u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        out.resize(out.len() + usize::try_from(data_len).unwrap(), 0);
        std::fs::write(path, out).unwrap();
    }

    /// Up to three audio files of the test set, copied into `dir` under
    /// names the fake answers by (`review…`, `auto…`, `unknown…`); silent
    /// WAVs where the test set is absent (as in CI). The originals are only
    /// read.
    fn music(dir: &Path) -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = std::fs::read_dir(TEST_SET)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp3")))
                    .collect()
            })
            .unwrap_or_default();
        found.sort();
        ["review", "auto", "unknown"]
            .iter()
            .enumerate()
            .map(|(i, prefix)| {
                if let Some(source) = found.get(i) {
                    let target = dir.join(format!("{prefix}-{i}.mp3"));
                    std::fs::copy(source, &target).unwrap();
                    target
                } else {
                    let target = dir.join(format!("{prefix}-{i}.wav"));
                    wav(&target);
                    target
                }
            })
            .collect()
    }

    struct Setup {
        _dirs: Vec<tempfile::TempDir>,
        /// The fixture, standing in for the installed library.
        original: rbl_db::LibraryLocation,
        /// A copy of its `master.db`.
        copy: PathBuf,
        store: PathBuf,
        report: PathBuf,
        audio: Vec<PathBuf>,
    }

    fn setup() -> Setup {
        let (library, music_dir, copies, store) =
            (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let shape = rbl_db::fixture::Shape { tracks: 4, playlists: 0, tracks_per_playlist: 0, history_sessions: 0, start_usn: 1000 };
        let original = rbl_db::fixture::build(library.path(), shape).unwrap();
        let audio = music(music_dir.path());
        for (i, path) in audio.iter().enumerate() {
            rbl_db::fixture::point_at_audio(&original, i, &path.to_string_lossy(), 300).unwrap();
        }
        // The fourth track's file is not there.
        rbl_db::fixture::point_at_audio(&original, 3, &music_dir.path().join("gone.mp3").to_string_lossy(), 300).unwrap();
        {
            // The disposable fixture, given an ISRC through a writable
            // connection of its own; the import reads only the copy.
            let mut db = rbl_db::Library::open(original.clone(), rbl_db::OpenMode::ReadWrite).unwrap();
            db.connection_mut()
                .execute("UPDATE djmdContent SET ISRC = 'GBAAA1400001' WHERE ID = ?1", [rbl_db::fixture::track_id(0)])
                .unwrap();
        }
        let copy = copies.path().join("master.db");
        std::fs::copy(&original.master_db, &copy).unwrap();
        let report = store.path().join("reports").join("import.csv");
        Setup {
            original,
            copy,
            store: store.path().join("identify"),
            report,
            audio,
            _dirs: vec![library, music_dir, copies, store],
        }
    }

    fn options(s: &Setup, master_db: &Path) -> Options {
        Options { master_db: master_db.to_owned(), limit: None, store: s.store.clone(), report: s.report.clone() }
    }

    fn import(s: &Setup, options: &Options, fake: &Fake) -> (Result<Summary, String>, String) {
        let state = IdentifyState::never_writing(s.store.clone());
        let mut progress = Vec::new();
        let summary = run_with(options, &s.original, &state, fake, &AtomicBool::new(false), &mut progress);
        (summary, String::from_utf8(progress).unwrap())
    }

    fn bytes(paths: &[PathBuf]) -> Vec<Vec<u8>> {
        paths.iter().map(|p| std::fs::read(p).unwrap()).collect()
    }

    #[test]
    fn the_installed_library_and_a_copy_beside_a_journal_are_refused() {
        let s = setup();
        let fake = Fake { calls: AtomicU32::new(0) };
        let (refused, _) = import(&s, &options(&s, &s.original.master_db), &fake);
        assert!(refused.unwrap_err().contains("installed rekordbox library"));
        // Spelled differently, it is still the original.
        let dotted = s.original.master_db.parent().unwrap().join(".").join("master.db");
        let (refused, _) = import(&s, &options(&s, &dotted), &fake);
        assert!(refused.unwrap_err().contains("installed rekordbox library"));
        let mut journal = s.copy.clone().into_os_string();
        journal.push("-journal");
        std::fs::write(&journal, b"hot").unwrap();
        let (refused, _) = import(&s, &options(&s, &s.copy), &fake);
        assert!(refused.unwrap_err().contains("-journal"));
        std::fs::remove_file(&journal).unwrap();
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0, "nothing identified");
        assert!(!s.store.join(super::super::STORE_FILE).exists(), "no store made");
    }

    #[test]
    fn it_refuses_to_start_while_file_writes_are_on() {
        let s = setup();
        let state = IdentifyState::with_root(s.store.clone());
        state
            .save_settings(&crate::identify::IdentifySettingsUpdateDto { file_writes: Some(true), ..Default::default() })
            .unwrap();
        let fake = Fake { calls: AtomicU32::new(0) };
        let (refused, _) = import(&s, &options(&s, &s.copy), &fake);
        assert!(refused.unwrap_err().contains("Write tags to audio files"));
        // Unreadable settings: refused too, never taken as off.
        std::fs::write(s.store.join(super::super::settings::SETTINGS_FILE), "{ broken").unwrap();
        let (refused, _) = import(&s, &options(&s, &s.copy), &fake);
        assert!(refused.unwrap_err().contains("cannot be read"));
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
        // And a state that may write is never used.
        let mut progress = Vec::new();
        let writable = IdentifyState::with_root(tempfile::tempdir().unwrap().path().join("identify"));
        let refused = run_with(&options(&s, &s.copy), &s.original, &writable, &fake, &AtomicBool::new(false), &mut progress);
        assert!(refused.is_err());
    }

    #[test]
    fn results_and_imports_land_in_the_store_files_stay_untouched_and_a_second_run_skips_all() {
        let s = setup();
        let audio_before = bytes(&s.audio);
        let copy_before = std::fs::read(&s.copy).unwrap();
        let original_before = std::fs::read(&s.original.master_db).unwrap();
        let fake = Fake { calls: AtomicU32::new(0) };

        let (summary, progress) = import(&s, &options(&s, &s.copy), &fake);
        let summary = summary.unwrap();
        assert_eq!(
            summary,
            Summary { library_tracks: 4, skipped: 0, run: 4, review: 2, unidentified: 1, failed: 1, auto_applied: 0 },
            "{progress}"
        );
        assert_eq!(fake.calls.load(Ordering::SeqCst), 3, "the missing file is not identified");
        assert!(progress.contains("1/4 ") && progress.contains("4/4 ") && progress.contains("eta"), "{progress}");

        // In the app's store, each with its imported rekordbox record.
        let state = IdentifyState::with_root(s.store.clone());
        let all = state.queue(QueueFilter::All, 0, 50).unwrap();
        assert_eq!(all.total, 3);
        for item in &all.items {
            let detail = state.detail(item.result_id).unwrap();
            let record = state.imported_record(detail.item.result_id).expect("imported with the result");
            assert_eq!(record.track_id, item.track_id);
            assert!(["review", "auto", "unknown"].iter().any(|p| record.title.starts_with(p)), "{}", record.title);
        }
        let first = all.items.iter().find(|i| i.track_id == rbl_db::fixture::track_id(0)).unwrap();
        assert_eq!(state.imported_record(first.result_id).unwrap().isrc, "GBAAA1400001");
        // The auto-apply decision was never written: it waits in review.
        let auto = all.items.iter().find(|i| i.track_id == rbl_db::fixture::track_id(1)).unwrap();
        assert_eq!((auto.state.as_str(), auto.decision.as_str()), ("pending", "review"));
        assert_eq!(state.queue(QueueFilter::Applied, 0, 10).unwrap().total, 0);
        assert_eq!(state.queue(QueueFilter::Waiting, 0, 10).unwrap().total, 0);

        // The report, in the dry run's format.
        let csv = report::parse_csv(std::fs::read_to_string(&s.report).unwrap().trim_start_matches('\u{feff}'));
        assert_eq!(csv[0][..5], ["file", "top_candidate", "score", "decision", "ground_truth_match"]);
        assert_eq!(csv.len(), 5, "a header and one row per track");
        assert!(csv.iter().any(|row| row[3] == "Failed"), "the missing file is listed");
        assert!(s.report.with_extension("summary.txt").is_file());

        // Resumed: everything is skipped.
        let (again, _) = import(&s, &options(&s, &s.copy), &fake);
        let again = again.unwrap();
        assert_eq!((again.skipped, again.run - again.failed), (3, 0), "{again:?}");
        assert_eq!(fake.calls.load(Ordering::SeqCst), 3, "nothing identified again");

        // Not a byte changed: the audio, the copy, the library.
        assert_eq!(bytes(&s.audio), audio_before);
        assert_eq!(std::fs::read(&s.copy).unwrap(), copy_before);
        assert_eq!(std::fs::read(&s.original.master_db).unwrap(), original_before);
    }

    #[test]
    fn the_limit_counts_after_skipping() {
        let s = setup();
        let fake = Fake { calls: AtomicU32::new(0) };
        let one = Options { limit: Some(1), ..options(&s, &s.copy) };
        let (first, _) = import(&s, &one, &fake);
        assert_eq!(first.unwrap().run, 1);
        let (second, _) = import(&s, &one, &fake);
        let second = second.unwrap();
        assert_eq!((second.skipped, second.run), (1, 1));
        assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
    }
}
