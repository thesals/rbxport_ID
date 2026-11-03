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
    /// A tag write or undo refused or failed: the file changed since it
    /// was read, is read-only or in use, or its tag cannot be written
    /// safely. Says why, never a tag value.
    #[error("{0}")]
    Write(String),
    /// A write refused because another program has the file open (a
    /// player, rekordbox): it can be made once the file is free.
    #[error("{0}")]
    InUse(String),
    /// A write refused because the file's tags are no longer the ones the
    /// proposal was reviewed against.
    #[error("{0}")]
    Changed(String),
    /// A write that could not be made now because the file is open in
    /// another program, and that the store keeps as a waiting write, to be
    /// made when the file is free. Says so, in the user's terms.
    #[error("{0}")]
    Deferred(String),
    /// A write stopped before it touched the file because its time ran out
    /// (the exit pass): nothing was changed, and it can be made later.
    #[error("{0}")]
    OutOfTime(String),
    /// A file whose tags (or format) cannot be parsed, as opposed to one
    /// that cannot be read ([`IdentifyError::Io`]).
    #[error("{0}")]
    Tags(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, IdentifyError>;
