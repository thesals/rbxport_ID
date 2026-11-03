//! Identifies every track of a COPY of a rekordbox library into the app's
//! identify store, headless, writing no audio file and never touching the
//! installed library.
//!
//! ```text
//! cargo run --release -p rbxport --example identify_import -- \
//!     --master-db <a COPY of master.db> [--limit N] [--store <dir>] [--report <csv>]
//! ```
//!
//! - `--master-db` (required): a copy of `master.db`, made with rekordbox
//!   closed. The installed library is refused, and so is a copy with a
//!   `-journal` or `-wal` file beside it. Opened read-only.
//! - `--limit N`: identify at most N tracks this run (after skipping).
//! - `--store <dir>`: the identify data folder; by default the app's own
//!   (`%APPDATA%\rbxport\identify`), so the app's review queue shows the
//!   results.
//! - `--report <csv>`: the per-track CSV in the dry run's format
//!   (default `target/identify/import_report.csv`); the summary goes beside
//!   it as `.summary.txt`, and to stderr.
//!
//! The copy is unlocked with the passphrase from rekordbox's agent settings
//! (`rbl_db::detect`); keys come from the environment or `.env.identify`
//! as in the app, and are never printed. It refuses to start while the
//! store's "Write tags to audio files" setting is on, forces auto-apply
//! off, and never applies, undoes or retries a write. A track already
//! identified for the same file is skipped, so a run stopped with Ctrl-C
//! keeps what it finished and the next run goes on. Progress goes to
//! stderr. See `rbxport_lib::identify::import` for the details.
#![allow(clippy::print_stderr, reason = "a command-line tool reporting progress")]

use std::path::PathBuf;
use std::process::ExitCode;

use rbxport_lib::identify::import::{self, Options};

const USAGE: &str = "usage: identify_import --master-db <copy of master.db> [--limit N] [--store <dir>] [--report <csv>]";

fn options() -> Result<Options, String> {
    let mut raw = std::env::args().skip(1);
    let (mut master_db, mut limit, mut store, mut report) = (None, None, None, None);
    while let Some(arg) = raw.next() {
        let mut value = || raw.next().ok_or_else(|| format!("{arg} needs a value\n{USAGE}"));
        match arg.as_str() {
            "--master-db" => master_db = Some(PathBuf::from(value()?)),
            "--limit" => limit = Some(value()?.parse().map_err(|e| format!("--limit: {e}"))?),
            "--store" => store = Some(PathBuf::from(value()?)),
            "--report" => report = Some(PathBuf::from(value()?)),
            _ => return Err(format!("unexpected argument {arg}\n{USAGE}")),
        }
    }
    let mut options = Options::for_copy(master_db.ok_or(USAGE)?);
    options.limit = limit;
    if let Some(store) = store {
        options.store = store;
    }
    if let Some(report) = report {
        options.report = report;
    }
    Ok(options)
}

fn run() -> Result<(), String> {
    let options = options()?;
    // Only for the passphrase; the path it names is the one refused.
    let original = rbl_db::detect().map_err(|e| format!("rekordbox's settings could not be read: {e}"))?;
    import::run(&options, &original, &mut std::io::stderr())?;
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("identify_import: {e}");
            ExitCode::FAILURE
        }
    }
}
