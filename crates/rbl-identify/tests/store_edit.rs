//! Editing a proposal before it is written: which results and fields may be
//! edited, and that the edit is what the store then gives back.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;

use rbl_identify::model::{Decision, ExistingTags, Field, FieldChange, FrameValues, Proposal, TagSnapshot};
use rbl_identify::store::{ReviewState, Store, TrackKey};

fn key(path: &str) -> TrackKey {
    TrackKey { path: path.to_owned(), file_size: 4_096, modified_unix: 1_700_000_000 }
}

fn proposal() -> Proposal {
    Proposal::new(
        vec![
            FieldChange { field: Field::Label, current: String::new(), proposed: "Wall of Sound".into() },
            FieldChange { field: Field::Year, current: String::new(), proposed: "2001".into() },
        ],
        None,
        true,
    )
}

fn saved(store: &Store, path: &str) -> i64 {
    let track = store.upsert_track(&key(path), 200.0, None, &ExistingTags::default()).unwrap();
    store.save_result(track, Decision::Review, &[], Some(&proposal()), None, &[], false).unwrap()
}

#[test]
fn an_edit_replaces_only_the_named_fields() {
    let store = Store::open_in_memory().unwrap();
    let result = saved(&store, "C:\\a.mp3");
    let edited = store.edit_proposal(result, &[(Field::Label, "Wall Of Sound Ltd".into())]).unwrap();
    assert_eq!(edited.changes[0].proposed, "Wall Of Sound Ltd");
    assert_eq!(edited.changes[1].proposed, "2001", "an unnamed field is left alone");
    let (item, ..) = store.result(result).unwrap().unwrap();
    assert_eq!(item.proposal, Some(edited));
    assert_eq!(item.state, ReviewState::Pending, "an edit is not an accept");
}

#[test]
fn a_field_the_proposal_lacks_or_artwork_is_refused_and_nothing_changes() {
    let store = Store::open_in_memory().unwrap();
    let result = saved(&store, "C:\\a.mp3");
    assert!(store.edit_proposal(result, &[(Field::Label, "x".into()), (Field::Isrc, "y".into())]).is_err());
    assert!(store.edit_proposal(result, &[(Field::Artwork, "x".into())]).is_err());
    let (item, ..) = store.result(result).unwrap().unwrap();
    assert_eq!(item.proposal, Some(proposal()), "a refused edit stores nothing");
}

#[test]
fn only_a_pending_or_accepted_result_without_an_open_write_can_be_edited() {
    let store = Store::open_in_memory().unwrap();
    let result = saved(&store, "C:\\a.mp3");
    store.set_state(result, ReviewState::Rejected).unwrap();
    assert!(store.edit_proposal(result, &[(Field::Year, "2002".into())]).is_err());
    store.set_state(result, ReviewState::Pending).unwrap();
    store.set_state(result, ReviewState::Accepted).unwrap();
    store.edit_proposal(result, &[(Field::Year, "2002".into())]).unwrap();

    let mut values = BTreeMap::new();
    values.insert(Field::Title, vec![FrameValues { frame: "TIT2".into(), values: vec!["t".into()] }]);
    let before = TagSnapshot { format: "Id3v2".into(), values, has_artwork: true, raw_id3: None };
    store.begin_application(result, &proposal().changes, None, &before, None).unwrap();
    let error = store.edit_proposal(result, &[(Field::Year, "2003".into())]).unwrap_err().to_string();
    assert!(error.contains("in progress"), "{error}");
}
