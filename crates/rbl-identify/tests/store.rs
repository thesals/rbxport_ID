//! The identify store as a file on disk: creation, reopening, and the
//! files it must refuse.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;

use rbl_identify::IdentifyError;
use rbl_identify::model::{Decision, ExistingTags, Field, FrameValues, RawId3, TagSnapshot};
use rbl_identify::store::{Filter, ReviewState, SCHEMA_VERSION, Store, TrackKey};

fn key(path: &str) -> TrackKey {
    TrackKey { path: path.to_owned(), file_size: 4_096, modified_unix: 1_700_000_000 }
}

fn snapshot() -> TagSnapshot {
    let mut values = BTreeMap::new();
    values.insert(Field::Title, vec![FrameValues { frame: "TIT2".into(), values: vec!["Eple".into()] }]);
    TagSnapshot {
        format: "Id3v2".into(),
        values,
        has_artwork: true,
        raw_id3: Some(RawId3 { v2: Some("SUQzBAAAAAAAAA".into()), v1: None }),
    }
}

#[test]
fn a_store_survives_closing_and_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("identify.sqlite");
    let path = "C:\\Music\\Röyksopp\\Eple.mp3";
    let tags = ExistingTags { artist: "Röyksopp".into(), title: "Eple".into(), ..ExistingTags::default() };
    let (track, result, art) = {
        let store = Store::open(&file).unwrap();
        let track = store.upsert_track(&key(path), 220.5, Some("AQAB"), &tags).unwrap();
        assert!(store.save_original_snapshot(track, &snapshot()).unwrap());
        let result = store.save_result(track, Decision::Review, &[], None, None, &["w".into()], false).unwrap();
        store.set_state(result, ReviewState::Accepted).unwrap();
        let art = store.put_artwork(b"\xff\xd8jpeg", "image/jpeg").unwrap();
        (track, result, art)
    };
    let journal: String = rusqlite::Connection::open(&file)
        .unwrap()
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(journal.to_ascii_lowercase(), "wal");

    let store = Store::open(&file).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    assert_eq!(store.track_id(path).unwrap(), Some(track));
    assert_eq!(store.original_snapshot(track).unwrap(), Some(snapshot()));
    assert!(!store.save_original_snapshot(track, &TagSnapshot::default()).unwrap());
    let items = store.list(Filter::All, 0, 10).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!((items[0].result, items[0].state, items[0].path.as_str()), (result, ReviewState::Accepted, path));
    assert_eq!(items[0].existing, tags);
    assert_eq!(store.artwork(&art).unwrap().unwrap().1, "image/jpeg");
}

#[test]
fn a_store_from_a_newer_build_is_refused_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("identify.sqlite");
    drop(Store::open(&file).unwrap());
    let conn = rusqlite::Connection::open(&file).unwrap();
    conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1).unwrap();
    drop(conn);

    let err = Store::open(&file).unwrap_err();
    assert!(matches!(&err, IdentifyError::Store(m) if m.contains("newer")), "{err}");
    let version: i64 = rusqlite::Connection::open(&file)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION + 1, "left as found");
}

#[test]
fn a_sqlite_file_that_is_not_an_identify_store_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("other.db");
    let conn = rusqlite::Connection::open(&file).unwrap();
    conn.execute_batch("CREATE TABLE djmdContent (ID TEXT PRIMARY KEY);").unwrap();
    drop(conn);
    let err = Store::open(&file).unwrap_err();
    assert!(matches!(&err, IdentifyError::Store(m) if m.contains("not an identify store")), "{err}");

    // Nor a file that is not SQLite at all (an encrypted master.db reads
    // like this without its key).
    let junk = dir.path().join("master.db");
    std::fs::write(&junk, vec![0x5a_u8; 8_192]).unwrap();
    assert!(matches!(Store::open(&junk), Err(IdentifyError::Store(_))));
    assert_eq!(std::fs::read(&junk).unwrap(), vec![0x5a_u8; 8_192], "left as found");
}
