//! [`StoreError`] — every failure the [`Store`] facade can produce.
//!
//! [`Store`]: crate::Store

use std::io;
use std::path::PathBuf;

/// Error produced by [`Store`](crate::Store) operations.
///
/// `Display` renders `<path>: <detail>` where a path is known, so a
/// permissions problem or a corrupt file names what it hit. The `ait`
/// frontend degrades any `StoreError` to a toast plus an unpersisted
/// run (Spec R08, user story 25) — the store failing never crashes the
/// terminal.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Creating the store's parent directory failed.
    #[error("{path}: cannot create store directory: {source}")]
    CreateDir {
        /// The directory that could not be created.
        path: PathBuf,
        /// The underlying I/O error.
        source: io::Error,
    },
    /// Opening the database file or applying its pragmas failed.
    #[error("{path}: cannot open database: {source}")]
    Open {
        /// The database file.
        path: PathBuf,
        /// The underlying SQLite error.
        source: rusqlite::Error,
    },
    /// A schema migration failed — the database may belong to a newer
    /// build or be corrupt.
    #[error("schema migration to version {version} failed: {source}")]
    Migrate {
        /// The `user_version` the failing migration targets.
        version: u32,
        /// The underlying SQLite error.
        source: rusqlite::Error,
    },
    /// A CRUD statement failed.
    #[error("store query failed: {0}")]
    Query(#[from] rusqlite::Error),
}
