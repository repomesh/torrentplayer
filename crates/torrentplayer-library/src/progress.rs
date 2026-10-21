// SPDX-License-Identifier: MIT OR Apache-2.0

//! Playback history and resume positions, stored in SQLite.
//!
//! A playable file is identified by its torrent info hash and file index. Every value from
//! torrent metadata is bound as a query parameter; SQL is always static. The database lives in
//! a private directory and is itself made user-readable only on Unix.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::queue::{MAX_SOURCE_BYTES, valid_text, validate_source};

/// The database file inside the app data directory.
pub const LIBRARY_FILE_NAME: &str = "library.sqlite3";
/// Positions before this are treated as not having started when offering to resume.
const MIN_RESUME_SECS: f64 = 10.0;
/// Do not offer to resume in the final few seconds.
const MIN_REMAINING_SECS: f64 = 10.0;
/// Current schema version (`PRAGMA user_version`).
const SCHEMA_VERSION: u32 = 1;

/// Stable metadata for one playable file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Media {
    /// Lowercase hexadecimal BitTorrent info hash.
    pub info_hash: String,
    /// File index inside the torrent.
    pub file_index: u32,
    /// A magnet or HTTP(S) link that can add the torrent again. Never a stream URL.
    pub source: String,
    /// File name shown to the user.
    pub title: String,
    /// Torrent/course name.
    pub torrent_name: String,
    /// File size, when known.
    pub size_bytes: Option<u64>,
}

impl Media {
    fn validate(&self) -> Result<(), LibraryError> {
        if !(self.info_hash.len() == 40
            && self.info_hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err(LibraryError::Invalid(
                "info hash must be 40 hexadecimal digits",
            ));
        }
        if self.source.len() > MAX_SOURCE_BYTES || validate_source(&self.source).is_err() {
            return Err(LibraryError::Invalid("source is not a safe torrent link"));
        }
        if !valid_text(&self.title) {
            return Err(LibraryError::Invalid("title is not valid display text"));
        }
        if !valid_text(&self.torrent_name) {
            return Err(LibraryError::Invalid(
                "torrent name is not valid display text",
            ));
        }
        Ok(())
    }
}

/// One item in playback history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct LibraryItem {
    /// Lowercase hexadecimal BitTorrent info hash.
    pub info_hash: String,
    /// File index inside the torrent.
    pub file_index: u32,
    /// Link that can add the torrent again. Never a stream URL.
    pub source: String,
    /// File name shown to the user.
    pub title: String,
    /// Torrent/course name.
    pub torrent_name: String,
    /// File size, when known.
    pub size_bytes: Option<u64>,
    /// Last playback position in seconds.
    pub position_secs: f64,
    /// Media duration in seconds, once the player has reported it.
    pub duration_secs: Option<f64>,
    /// Whether playback reached the end.
    pub completed: bool,
    /// Unix time of the last open or progress update.
    pub updated_at_secs: u64,
}

/// A useful saved position, or `None` when playback should start at the beginning.
#[must_use]
pub fn resume_position(item: &LibraryItem) -> Option<f64> {
    if item.completed || !item.position_secs.is_finite() || item.position_secs < MIN_RESUME_SECS {
        return None;
    }
    if item.duration_secs.is_some_and(|duration| {
        !duration.is_finite()
            || duration <= 0.0
            || item.position_secs >= duration - MIN_REMAINING_SECS
    }) {
        return None;
    }
    Some(item.position_secs)
}

/// Why the playback library could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum LibraryError {
    /// Storage was unavailable when the app started; playback may continue without history.
    #[error("the playback library is unavailable")]
    Unavailable,
    /// Media metadata or playback values were invalid.
    #[error("invalid library value: {0}")]
    Invalid(&'static str),
    /// The library belongs to a newer app version.
    #[error("library schema version {0} is newer than this app supports")]
    NewerVersion(u32),
    /// A file-system operation failed.
    #[error("could not prepare the library database: {0}")]
    Io(#[from] io::Error),
    /// SQLite rejected an operation.
    #[error("library database operation failed: {0}")]
    Sql(#[from] rusqlite::Error),
}

/// SQLite playback-history store.
pub struct LibraryStore {
    connection: Connection,
    path: PathBuf,
}

impl std::fmt::Debug for LibraryStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LibraryStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl LibraryStore {
    /// Opens (and, on first use, creates) `dir/library.sqlite3`.
    ///
    /// # Errors
    ///
    /// Returns an error if the private directory, database, or schema cannot be prepared, or if
    /// the database was written by a newer incompatible version.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, LibraryError> {
        let dir = dir.into();
        create_private_dir(&dir)?;
        let path = dir.join(LIBRARY_FILE_NAME);
        let connection = Connection::open(&path)?;
        set_private_file(&path)?;
        connection.busy_timeout(std::time::Duration::from_secs(2))?;
        connection.execute_batch("PRAGMA journal_mode = DELETE; PRAGMA synchronous = NORMAL;")?;
        let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(LibraryError::NewerVersion(version));
        }
        if version == 0 {
            connection.execute_batch(
                "BEGIN IMMEDIATE;
                CREATE TABLE IF NOT EXISTS playback (
                    info_hash TEXT NOT NULL,
                    file_index INTEGER NOT NULL,
                    source TEXT NOT NULL,
                    title TEXT NOT NULL,
                    torrent_name TEXT NOT NULL,
                    size_bytes INTEGER,
                    position_secs REAL NOT NULL,
                    duration_secs REAL,
                    completed INTEGER NOT NULL,
                    updated_at_secs INTEGER NOT NULL,
                    PRIMARY KEY (info_hash, file_index),
                    CHECK(length(info_hash) = 40),
                    CHECK(file_index >= 0 AND file_index <= 4294967295),
                    CHECK(position_secs >= 0),
                    CHECK(duration_secs IS NULL OR duration_secs > 0),
                    CHECK(completed IN (0, 1)),
                    CHECK(updated_at_secs >= 0)
                );
                CREATE INDEX IF NOT EXISTS playback_recent
                    ON playback(updated_at_secs DESC, info_hash, file_index);
                PRAGMA user_version = 1;
                COMMIT;",
            )?;
        }
        Ok(Self { connection, path })
    }

    /// Path of the database file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Adds an item to history, or refreshes its safe display metadata without destroying saved
    /// progress. Returns the current row.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid metadata or a database failure.
    pub fn opened(
        &mut self,
        media: &Media,
        updated_at_secs: u64,
    ) -> Result<LibraryItem, LibraryError> {
        media.validate()?;
        let updated = sqlite_integer(updated_at_secs, "update time is out of range")?;
        let size = media
            .size_bytes
            .map(|value| sqlite_integer(value, "file size is out of range"))
            .transpose()?;
        self.connection.execute(
            "INSERT INTO playback (
                info_hash, file_index, source, title, torrent_name, size_bytes,
                position_secs, duration_secs, completed, updated_at_secs
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, NULL, 0, ?7)
             ON CONFLICT(info_hash, file_index) DO UPDATE SET
                source = excluded.source,
                title = excluded.title,
                torrent_name = excluded.torrent_name,
                size_bytes = excluded.size_bytes,
                updated_at_secs = excluded.updated_at_secs",
            params![
                media.info_hash.to_ascii_lowercase(),
                media.file_index,
                media.source,
                media.title,
                media.torrent_name,
                size,
                updated,
            ],
        )?;
        self.get(&media.info_hash, media.file_index)?
            .ok_or(LibraryError::Invalid(
                "the opened library item could not be read back",
            ))
    }

    /// Saves a playback position and returns the updated row.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid metadata, position, duration, or a database failure.
    pub fn record(
        &mut self,
        media: &Media,
        position_secs: f64,
        duration_secs: Option<f64>,
        updated_at_secs: u64,
    ) -> Result<LibraryItem, LibraryError> {
        media.validate()?;
        if !position_secs.is_finite() || position_secs < 0.0 {
            return Err(LibraryError::Invalid(
                "playback position must be non-negative and finite",
            ));
        }
        if duration_secs.is_some_and(|duration| !duration.is_finite() || duration <= 0.0) {
            return Err(LibraryError::Invalid(
                "playback duration must be positive and finite",
            ));
        }
        let position = duration_secs.map_or(position_secs, |duration| position_secs.min(duration));
        let completed = duration_secs.is_some_and(|duration| position >= duration - 1.0);
        let updated = sqlite_integer(updated_at_secs, "update time is out of range")?;
        let size = media
            .size_bytes
            .map(|value| sqlite_integer(value, "file size is out of range"))
            .transpose()?;
        self.connection.execute(
            "INSERT INTO playback (
                info_hash, file_index, source, title, torrent_name, size_bytes,
                position_secs, duration_secs, completed, updated_at_secs
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(info_hash, file_index) DO UPDATE SET
                source = excluded.source,
                title = excluded.title,
                torrent_name = excluded.torrent_name,
                size_bytes = excluded.size_bytes,
                position_secs = excluded.position_secs,
                duration_secs = COALESCE(excluded.duration_secs, playback.duration_secs),
                completed = excluded.completed,
                updated_at_secs = excluded.updated_at_secs",
            params![
                media.info_hash.to_ascii_lowercase(),
                media.file_index,
                media.source,
                media.title,
                media.torrent_name,
                size,
                position,
                duration_secs,
                completed,
                updated,
            ],
        )?;
        self.get(&media.info_hash, media.file_index)?
            .ok_or(LibraryError::Invalid(
                "the updated library item could not be read back",
            ))
    }

    /// Returns one item's progress.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails or a stored numeric value cannot be represented.
    pub fn get(
        &self,
        info_hash: &str,
        file_index: u32,
    ) -> Result<Option<LibraryItem>, LibraryError> {
        let mut statement = self.connection.prepare(
            "SELECT info_hash, file_index, source, title, torrent_name, size_bytes,
                    position_secs, duration_secs, completed, updated_at_secs
             FROM playback WHERE info_hash = ?1 AND file_index = ?2",
        )?;
        Ok(statement
            .query_row(
                params![info_hash.to_ascii_lowercase(), file_index],
                row_item,
            )
            .optional()?)
    }

    /// All playback history, most recently opened or watched first.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails or a stored numeric value cannot be represented.
    pub fn list(&self) -> Result<Vec<LibraryItem>, LibraryError> {
        let mut statement = self.connection.prepare(
            "SELECT info_hash, file_index, source, title, torrent_name, size_bytes,
                    position_secs, duration_secs, completed, updated_at_secs
             FROM playback ORDER BY updated_at_secs DESC, info_hash, file_index",
        )?;
        let rows = statement.query_map([], row_item)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

fn row_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<LibraryItem> {
    let size_bytes = row
        .get::<_, Option<i64>>(5)?
        .map(|value| unsigned_column(5, value))
        .transpose()?;
    let updated_at_secs = unsigned_column(9, row.get(9)?)?;
    Ok(LibraryItem {
        info_hash: row.get(0)?,
        file_index: row.get(1)?,
        source: row.get(2)?,
        title: row.get(3)?,
        torrent_name: row.get(4)?,
        size_bytes,
        position_secs: row.get(6)?,
        duration_secs: row.get(7)?,
        completed: row.get(8)?,
        updated_at_secs,
    })
}

fn unsigned_column(index: usize, value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn sqlite_integer(value: u64, reason: &'static str) -> Result<i64, LibraryError> {
    i64::try_from(value).map_err(|_| LibraryError::Invalid(reason))
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)
    }
}

fn set_private_file(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        // Opening succeeded just above; checking metadata here still surfaces an unexpected
        // disappearance instead of pretending the permissions step succeeded.
        fs::metadata(path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c";

    fn media() -> Media {
        Media {
            info_hash: HASH.to_owned(),
            file_index: 2,
            source: format!("magnet:?xt=urn:btih:{HASH}&dn=Sintel"),
            title: "Sintel.mp4".to_owned(),
            torrent_name: "Sintel course".to_owned(),
            size_bytes: Some(123_456),
        }
    }

    #[test]
    fn opens_records_and_reopens_progress() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = LibraryStore::open(dir.path()).unwrap();
        let opened = store.opened(&media(), 100).unwrap();
        assert!(opened.position_secs.abs() < f64::EPSILON);
        assert_eq!(opened.updated_at_secs, 100);

        let watched = store.record(&media(), 123.5, Some(600.0), 101).unwrap();
        assert_eq!(resume_position(&watched), Some(123.5));
        drop(store);

        let store = LibraryStore::open(dir.path()).unwrap();
        assert_eq!(store.list().unwrap(), [watched]);
    }

    #[test]
    fn opening_again_refreshes_metadata_without_losing_progress() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = LibraryStore::open(dir.path()).unwrap();
        store.record(&media(), 80.0, Some(100.0), 1).unwrap();
        let mut renamed = media();
        renamed.title = "A better title".to_owned();
        let item = store.opened(&renamed, 2).unwrap();
        assert_eq!(item.title, "A better title");
        assert!((item.position_secs - 80.0).abs() < f64::EPSILON);
        assert_eq!(item.duration_secs, Some(100.0));
    }

    #[test]
    fn completion_and_resume_edges_are_safe() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = LibraryStore::open(dir.path()).unwrap();
        let near_start = store.record(&media(), 9.9, Some(100.0), 1).unwrap();
        assert_eq!(resume_position(&near_start), None);
        let middle = store.record(&media(), 10.0, Some(100.0), 2).unwrap();
        assert_eq!(resume_position(&middle), Some(10.0));
        let near_end = store.record(&media(), 91.0, Some(100.0), 3).unwrap();
        assert_eq!(resume_position(&near_end), None);
        let finished = store.record(&media(), 100.0, Some(100.0), 4).unwrap();
        assert!(finished.completed);
        assert_eq!(resume_position(&finished), None);
    }

    #[test]
    fn metadata_is_bound_as_data_not_sql() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = LibraryStore::open(dir.path()).unwrap();
        let mut hostile = media();
        hostile.title = "Lecture'); DROP TABLE playback; --".to_owned();
        hostile.torrent_name = "Course ?1; DELETE FROM playback".to_owned();
        store.opened(&hostile, 1).unwrap();
        assert_eq!(store.list().unwrap()[0].title, hostile.title);
        assert!(store.record(&hostile, 20.0, None, 2).is_ok());
    }

    #[test]
    fn rejects_invalid_values() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = LibraryStore::open(dir.path()).unwrap();
        let mut invalid = media();
        invalid.info_hash = "not-a-hash".to_owned();
        assert!(matches!(
            store.opened(&invalid, 1),
            Err(LibraryError::Invalid(_))
        ));
        assert!(matches!(
            store.record(&media(), f64::NAN, None, 1),
            Err(LibraryError::Invalid(_))
        ));
    }

    #[test]
    fn rejects_negative_stored_unsigned_values() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = LibraryStore::open(dir.path()).unwrap();
        store.opened(&media(), 1).unwrap();
        store
            .connection
            .execute(
                "UPDATE playback SET size_bytes = -1 WHERE info_hash = ?1",
                [HASH],
            )
            .unwrap();
        assert!(matches!(store.list(), Err(LibraryError::Sql(_))));
    }

    #[cfg(unix)]
    #[test]
    fn database_and_directory_are_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("library");
        let store = LibraryStore::open(&dir).unwrap();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(store.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
