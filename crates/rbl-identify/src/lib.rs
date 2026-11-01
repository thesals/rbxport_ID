//! Track identification for the rbxport fork: fingerprint a file, ask
//! AcoustID and MusicBrainz who it is, score what comes back against the
//! file's own tags, and decide whether a change may be proposed.
//!
//! Nothing here writes tags or touches a library. The crate reads audio,
//! talks to free metadata services under enforced rate limits, and returns
//! decisions; applying them belongs to the app's edit path. See
//! `docs/identify/IDENTIFY_SPEC.md` for the rules this follows.

pub mod acoustid;
pub mod clock;
pub mod config;
pub mod decide;
pub mod error;
pub mod fingerprint;
pub mod mixname;
pub mod model;
pub mod musicbrainz;
pub mod normalize;
pub mod pipeline;
pub mod ratelimit;
pub mod report;
pub mod score;
pub mod transport;

pub use error::{IdentifyError, Result};
