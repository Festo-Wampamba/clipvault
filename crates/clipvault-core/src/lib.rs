//! Clip storage, search and retention for ClipVault. No OS code: capture
//! backends hand clips in, the UI and CLI read them out.

mod migrate;
mod store;

pub use migrate::{LATEST_SCHEMA_VERSION, migrate_down_to};
pub use store::{
    Added, ClipId, ClipSummary, Format, Kind, Limits, NewClip, PNG_MIME, Store, TEXT_MIME,
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("file error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid search pattern: {0}")]
    Regex(#[from] regex::Error),
    #[error("no clip with id {0}")]
    NotFound(ClipId),
    #[error("a clip needs at least one format")]
    EmptyClip,
    #[error(
        "the history was written by a newer ClipVault (schema {found}; this build knows {known})"
    )]
    NewerSchema { found: i64, known: i64 },
    /// The clips are deleted, but another ClipVault process is still reading an older
    /// snapshot, so their content stays in the write-ahead log until the next `prune`.
    #[error(
        "deleted, but another ClipVault process kept the old content in the database log; it will be wiped at the next cleanup"
    )]
    DeletedButNotFlushed,
}

pub type Result<T> = std::result::Result<T, Error>;
