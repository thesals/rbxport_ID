//! Errors. Messages never carry a secret: a key appears in no URL, header
//! or body that is formatted into an error.

#[derive(Debug, thiserror::Error)]
pub enum IdentifyError {
    #[error("could not read audio: {0}")]
    Audio(#[from] rbl_audio::AudioError),
    #[error("fingerprinting failed: {0}")]
    Fingerprint(String),
    #[error("configuration: {0}")]
    Config(String),
    /// A transport failure: no response at all.
    #[error("{service}: request failed: {reason}")]
    Http { service: &'static str, reason: String },
    /// A response with a status the client does not accept.
    #[error("{service}: HTTP {status}")]
    Status { service: &'static str, status: u16 },
    /// The service answered with something the client cannot read.
    #[error("{service}: unexpected response: {reason}")]
    Malformed { service: &'static str, reason: String },
    /// The service's own error object (AcoustID `status: error`).
    #[error("{service}: {message}")]
    Service { service: &'static str, message: String },
    /// The identify store (its own SQLite file, never rekordbox's).
    #[error("identify store: {0}")]
    Store(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, IdentifyError>;
