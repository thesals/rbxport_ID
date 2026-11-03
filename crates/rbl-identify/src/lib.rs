//! Track identification for the rbxport fork: fingerprint a file, ask
//! AcoustID and MusicBrainz who it is, score what comes back against the
//! file's own tags, and decide whether a change may be proposed.
//!
//! The crate reads audio, talks to free metadata services under enforced
//! rate limits and returns decisions; [`write`] writes accepted changes to
//! the audio file's tags only, on the user's request. Nothing here ever
//! writes to a rekordbox library: identification is one-way (the app reads
//! the library into [`store`], never back). See
//! `docs/identify/IDENTIFY_SPEC.md` for the rules this follows.

pub mod acoustid;
pub mod clock;
pub mod comment;
pub mod config;
pub mod coverart;
pub mod decide;
pub mod discogs;
pub mod error;
pub mod fallback;
pub mod fingerprint;
pub mod mixname;
pub mod model;
pub mod musicbrainz;
pub mod normalize;
pub mod paths;
pub mod pipeline;
pub mod ratelimit;
pub mod report;
pub mod score;
pub mod store;
pub mod tags;
pub mod transport;
pub mod write;

pub use error::{IdentifyError, Result};
