//! Identify every audio file in a folder without changing anything, and
//! judge the results against a ground truth.
//!
//! ```text
//! cargo run --release -p rbl-identify --example dry_run -- <folder>
//!     [--ground-truth <folder>/ground_truth.csv] [--out target/identify/identify_report.csv]
//!     [--env .env.identify] [--limit N]
//! ```
//!
//! Read-only: files are decoded and their tags read, and nothing is
//! written into the folder; the report goes to `target/identify/` unless
//! `--out` says otherwise. Makes live requests to AcoustID and MusicBrainz under
//! the crate's rate limits (about 2–8 s per track, mostly MusicBrainz's
//! 1 request per second). Progress and the summary go to stderr.
#![allow(clippy::print_stderr, reason = "a command-line tool reporting progress")]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rbl_identify::acoustid::AcoustIdClient;
use rbl_identify::clock::{Clock, SystemClock};
use rbl_identify::config::Config;
use rbl_identify::musicbrainz::MusicBrainzClient;
use rbl_identify::pipeline::{self, Services, Settings};
use rbl_identify::ratelimit::Limiters;
use rbl_identify::report::{self, Summary, Verdict};
use rbl_identify::transport::{HttpTransport, Transport};

const AUDIO: [&str; 7] = ["mp3", "flac", "wav", "aif", "aiff", "m4a", "mp4"];

struct Args {
    folder: PathBuf,
    ground_truth: PathBuf,
    out: PathBuf,
    env: PathBuf,
    limit: Option<usize>,
}

fn args() -> Result<Args, String> {
    let mut raw = std::env::args().skip(1);
    let mut folder = None;
    let (mut ground_truth, mut out, mut env, mut limit) = (None, None, None, None);
    while let Some(arg) = raw.next() {
        let mut value = || raw.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--ground-truth" => ground_truth = Some(PathBuf::from(value()?)),
            "--out" => out = Some(PathBuf::from(value()?)),
            "--env" => env = Some(PathBuf::from(value()?)),
            "--limit" => limit = Some(value()?.parse().map_err(|e| format!("--limit: {e}"))?),
            _ if folder.is_none() && !arg.starts_with("--") => folder = Some(PathBuf::from(arg)),
            _ => return Err(format!("unexpected argument {arg}")),
        }
    }
    let folder = folder.ok_or("usage: dry_run <folder> [--ground-truth csv] [--out csv] [--env file] [--limit N]")?;
    Ok(Args {
        ground_truth: ground_truth.unwrap_or_else(|| folder.join("ground_truth.csv")),
        // Never into the test-set folder, which must stay untouched.
        out: out.unwrap_or_else(|| PathBuf::from("target").join("identify").join("identify_report.csv")),
        env: env.unwrap_or_else(|| PathBuf::from(".env.identify")),
        folder,
        limit,
    })
}

fn audio_files(folder: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(folder)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension().and_then(|e| e.to_str()).is_some_and(|e| AUDIO.contains(&e.to_ascii_lowercase().as_str()))
        })
        .collect();
    files.sort();
    Ok(files)
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dry_run: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args = args()?;
    let config = Config::load(&args.env).map_err(|e| e.to_string())?;
    let truth = if args.ground_truth.is_file() {
        report::load_ground_truth(&args.ground_truth).map_err(|e| e.to_string())?
    } else {
        eprintln!("no ground truth at {}; results are not judged", args.ground_truth.display());
        std::collections::HashMap::new()
    };
    let mut files = audio_files(&args.folder).map_err(|e| format!("{}: {e}", args.folder.display()))?;
    if let Some(limit) = args.limit {
        files.truncate(limit);
    }

    // One clock, one transport and one set of limiters for the whole run.
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let transport: Arc<dyn Transport> = Arc::new(HttpTransport::new().map_err(|e| e.to_string())?);
    let limiters = Limiters::new(Arc::clone(&clock));
    let acoustid = AcoustIdClient::new(Arc::clone(&transport), Arc::clone(&clock), Arc::clone(&limiters.acoustid), config.acoustid_key.clone());
    let musicbrainz =
        MusicBrainzClient::new(Arc::clone(&transport), Arc::clone(&clock), Arc::clone(&limiters.musicbrainz), config.mb_user_agent.clone());
    let services = Services { acoustid: &acoustid, musicbrainz: &musicbrainz };

    let started = std::time::Instant::now();
    let mut results = Vec::new();
    let mut failures = Vec::new();
    let mut summary = Summary::default();
    for (i, path) in files.iter().enumerate() {
        let name = path.strip_prefix(&args.folder).unwrap_or(path).to_string_lossy().replace('\\', "/");
        match pipeline::identify(path, &services, Settings::default()) {
            Ok(result) => {
                let verdict = report::judge(&result, truth.get(&name));
                summary.add(&result, verdict, truth.get(&name));
                eprintln!(
                    "[{}/{}] {:?} {:<12} {:.2}  {}",
                    i + 1,
                    files.len(),
                    result.decision,
                    format!("{verdict:?}"),
                    result.best().map_or(0.0, |b| b.score),
                    name
                );
                results.push((name, result, verdict));
            }
            Err(e) => {
                summary.add_failure(truth.get(&name));
                eprintln!("[{}/{}] FAILED {name}: {e}", i + 1, files.len());
                failures.push((name, e.to_string()));
            }
        }
    }

    if let Some(dir) = args.out.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let rows: Vec<(String, &pipeline::TrackResult, Verdict)> = results.iter().map(|(n, r, v)| (n.clone(), r, *v)).collect();
    std::fs::write(&args.out, report::csv(&rows, &failures)).map_err(|e| format!("{}: {e}", args.out.display()))?;
    let text = format!("{}elapsed {:.0} s\n", summary.render(), started.elapsed().as_secs_f64());
    let summary_path = args.out.with_extension("summary.txt");
    std::fs::write(&summary_path, &text).map_err(|e| format!("{}: {e}", summary_path.display()))?;
    eprintln!("\n{text}report: {}\nsummary: {}", args.out.display(), summary_path.display());
    Ok(())
}
