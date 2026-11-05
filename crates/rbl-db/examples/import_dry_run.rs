//! A dry run of the first import, from a COPY of rekordbox's `master.db`
//! into a folder that is not rbxport's real library.
//!
//! ```text
//! cargo run --release -p rbl-db --example import_dry_run -- \
//!     --master-db <a copy of master.db> --share <a share folder to read> --target <an empty folder> [--again]
//! ```
//!
//! `--again` then imports from the same copy a second time, as File ›
//! Import from rekordbox… would, and prints what that took: from an
//! unchanged source, nothing.
//!
//! The passphrase comes from rekordbox's agent settings, read only. The
//! installed `master.db` itself is refused as `--master-db`, and a target
//! inside rekordbox's folders or rbxport's real library folder is refused.
//! `--share` is only read.
#![allow(clippy::pedantic, clippy::print_stdout, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut master_db, mut share, mut target) = (None, None, None);
    let mut again = false;
    while let Some(flag) = args.next() {
        if flag == "--again" {
            again = true;
            continue;
        }
        let value = args.next().map(PathBuf::from);
        match flag.as_str() {
            "--master-db" => master_db = value,
            "--share" => share = value,
            "--target" => target = value,
            other => panic!("unknown argument {other}"),
        }
    }
    let (master_db, share, target) =
        (master_db.expect("--master-db"), share.expect("--share"), target.expect("--target"));

    let installed = rbl_db::rekordbox_import::find_rekordbox().expect("rekordbox's library on this machine");
    assert!(
        !rbl_db::is_within_any(&master_db, std::slice::from_ref(&installed.master_db)),
        "--master-db is rekordbox's installed library; pass a copy"
    );
    let own = rbl_db::own_library_dir().expect("rbxport's library folder");
    assert!(!rbl_db::is_within_any(&target, &[own]), "--target is rbxport's real library folder");

    let source = rbl_db::LibraryLocation {
        master_db,
        share_root: share,
        passphrase: installed.passphrase,
        is_user_library: false,
        is_rekordbox_install: false,
    };
    let started = std::time::Instant::now();
    let mut last = None;
    let summary = rbl_db::rekordbox_import::first_import(&source, &target, &mut |p| {
        if last != Some(p.stage) || p.done == p.total || p.done % 2048 == 0 {
            println!("{:?} {}/{}", p.stage, p.done, p.total);
            last = Some(p.stage);
        }
    })
    .expect("import");
    println!(
        "tracks {} playlists {} cues {} histories {} files {} ({}) problems {} source unchanged {} in {:.1}s",
        summary.tracks,
        summary.playlists,
        summary.cues,
        summary.history_sessions,
        summary.files_copied,
        rbl_db::rekordbox_import::human_bytes(summary.bytes_copied),
        summary.problems.len(),
        summary.source_unchanged,
        started.elapsed().as_secs_f64()
    );
    for problem in summary.problems.iter().take(20) {
        println!("  problem: {problem}");
    }
    println!("log {}", summary.log.display());

    // The imported library against the copy it came from, table by table.
    let rows = |location: rbl_db::LibraryLocation| {
        let library = rbl_db::Library::open(location, rbl_db::OpenMode::ReadOnly).expect("open");
        let conn = library.connection();
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(std::result::Result::unwrap)
            .collect();
        tables
            .into_iter()
            .map(|table| {
                let mut statement = conn.prepare(&format!("SELECT * FROM `{table}` ORDER BY rowid")).unwrap();
                let width = statement.column_count();
                let rows: Vec<Vec<rusqlite::types::Value>> = statement
                    .query_map([], |r| (0..width).map(|i| r.get(i)).collect())
                    .unwrap()
                    .map(std::result::Result::unwrap)
                    .collect();
                (table, rows)
            })
            .collect::<Vec<_>>()
    };
    let before = rows(source.clone());
    let after = rows(summary.location.clone());
    let differing: Vec<_> = before.iter().zip(&after).filter(|(a, b)| a != b).map(|(a, _)| a.0.clone()).collect();
    println!(
        "tables {} vs {}, rows {} vs {}, differing tables {:?}",
        before.len(),
        after.len(),
        before.iter().map(|t| t.1.len()).sum::<usize>(),
        after.iter().map(|t| t.1.len()).sum::<usize>(),
        differing
    );

    if !again {
        return;
    }
    let started = std::time::Instant::now();
    let mut location = summary.location.clone();
    location.is_user_library = false; // the dry run's target, checked above not to be the real one
    let backups = target.parent().expect("target has a folder").join("dry-run-backups");
    let mut writer = rbl_db::write::Writer::open(location, backups).expect("open the imported library");
    let delta = rbl_db::rekordbox_import::import_again(&source, &mut writer, &rbl_db::rekordbox_import::Take::new(), &mut |_| {}).expect("import again");
    let r = &delta.report;
    println!(
        "again: new {} updated {} skipped {} filled {} path matches {} unchanged {} kept deleted {} duplicates {}          playlists made {} appended {} skipped {} histories {} cues {} files {} problems {} source unchanged {} in {:.1}s",
        r.new_tracks.len(),
        r.updated.len(),
        r.skipped.len(),
        r.filled.len(),
        r.path_matches.len(),
        r.unchanged,
        r.kept_deleted,
        r.duplicates.len(),
        r.playlists_created.len(),
        r.playlists_appended.len(),
        r.playlists_skipped.len(),
        r.history_sessions,
        r.cues,
        r.files_copied,
        r.problems.len(),
        delta.source_unchanged,
        started.elapsed().as_secs_f64()
    );
    for skipped in r.skipped.iter().take(5) {
        println!("  skipped {}: {:?}", skipped.track.name, skipped.fields);
    }
    println!("log {}", delta.log.display());
}
