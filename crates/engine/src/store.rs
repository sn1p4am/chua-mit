//! Durable state: tasks, per-segment progress, and engine-level settings.
//!
//! Storage is SQLite. Every write that a reader could observe is a transaction,
//! so a crash can lose the tail of the progress history but can never leave a
//! half-written record (`spec/engine-v1.md` §7.2).
//!
//! # What durability means here
//!
//! [`crate::model::SegmentState::durable_end`] is only advanced after the file
//! has been flushed *and* the new value has been committed. So the store is
//! never optimistic: a crash can cause bytes to be fetched twice, which is
//! explicitly the acceptable direction (§7.3). Bytes the store has not been
//! told about are not counted anywhere.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::failure::{Failure, FailureKind, FailureScope};
use crate::http::Validators;
use crate::model::{SegmentState, TaskId, TaskRecord};
use crate::state::TaskState;

/// Version of the schema in this file. Bumped when a migration is needed.
pub const SCHEMA_VERSION: i64 = 1;

/// Engine-level value that has no per-task home.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineSettings {
    /// Global transfer ceiling in bytes per second; `None` is unlimited.
    pub global_rate_limit_bps: Option<u64>,
}

impl Default for EngineSettings {
    fn default() -> Self {
        Self::new_unlimited()
    }
}

impl EngineSettings {
    /// Settings with no global ceiling, which is §11's default.
    pub const fn new_unlimited() -> Self {
        Self {
            global_rate_limit_bps: None,
        }
    }
}

/// Handle to the durable state.
///
/// Deliberately synchronous. Callers keep it inside a single owner so that the
/// statements below are short and serialised; the intervals involved are well
/// under a millisecond with SQLite's default page cache.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating if needed) the database at `path`.
    pub fn open(path: &Path) -> Result<Self, Failure> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|error| crate::http::classify_io(&error, "create_state_dir"))?;
        }
        let conn = Connection::open(path).map_err(map_sqlite("open"))?;
        Self::prepare(conn)
    }

    /// Open an in-memory database. Used by tests and by hosts that explicitly
    /// want no durable record.
    pub fn open_in_memory() -> Result<Self, Failure> {
        let conn = Connection::open_in_memory().map_err(map_sqlite("open_memory"))?;
        Self::prepare(conn)
    }

    fn prepare(conn: Connection) -> Result<Self, Failure> {
        // WAL keeps readers from blocking the writer and survives a crash by
        // replay, which is what the resume contract depends on.
        let _journal: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
            .map_err(map_sqlite("pragma_journal_mode"))?;
        conn.execute_batch(
            "PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS meta (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS tasks (
                 id             INTEGER PRIMARY KEY,
                 state          TEXT    NOT NULL,
                 source_label   TEXT    NOT NULL,
                 target_path    TEXT    NOT NULL,
                 total_len      INTEGER,
                 range_supported INTEGER NOT NULL,
                 strong_etag    TEXT,
                 weak_etag      TEXT,
                 failure_json   TEXT,
                 name_hint      TEXT,
                 rate_limit_bps INTEGER,
                 created_at     INTEGER NOT NULL,
                 updated_at     INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS segments (
                 task_id     INTEGER NOT NULL,
                 segment_id  INTEGER NOT NULL,
                 start_byte  INTEGER NOT NULL,
                 end_byte    INTEGER NOT NULL,
                 durable_end INTEGER,
                 PRIMARY KEY (task_id, segment_id),
                 FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE
             );",
        )
        .map_err(map_sqlite("schema"))?;

        let store = Self { conn };
        store.write_meta("schema_version", &SCHEMA_VERSION.to_string())?;
        // The counter is initialised once and then owned by the store. Writing
        // it unconditionally on every open looks harmless and is not: each new
        // host process would hand out id 1 again, and the second task would
        // upsert straight over the first one's record — the same id meaning two
        // different downloads, with the earlier one's state silently lost.
        if store.read_meta_u64("next_task_id")?.is_none() {
            store.write_meta("next_task_id", "1")?;
        }
        Ok(store)
    }

    /// Allocate the next task id. Persisted so ids are not reused after a
    /// restart, which keeps event and store ordering consistent.
    pub fn next_task_id(&mut self) -> Result<TaskId, Failure> {
        let current = self.read_meta_u64("next_task_id")?.unwrap_or(1).max(1);
        self.write_meta("next_task_id", &(current + 1).to_string())?;
        Ok(TaskId(current))
    }

    /// Insert or fully replace a task and its segments, in one transaction.
    pub fn save_task(&mut self, record: &TaskRecord) -> Result<(), Failure> {
        let failure_json = match &record.failure {
            Some(failure) => Some(serde_json::to_string(failure).map_err(map_serde("failure"))?),
            None => None,
        };
        let tx = self.conn.transaction().map_err(map_sqlite("begin"))?;
        tx.execute(
            "INSERT INTO tasks (
                 id, state, source_label, target_path, total_len, range_supported,
                 strong_etag, weak_etag, failure_json, name_hint, rate_limit_bps,
                 created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(id) DO UPDATE SET
                 state = excluded.state,
                 source_label = excluded.source_label,
                 target_path = excluded.target_path,
                 total_len = excluded.total_len,
                 range_supported = excluded.range_supported,
                 strong_etag = excluded.strong_etag,
                 weak_etag = excluded.weak_etag,
                 failure_json = excluded.failure_json,
                 name_hint = excluded.name_hint,
                 rate_limit_bps = excluded.rate_limit_bps,
                 created_at = excluded.created_at,
                 updated_at = excluded.updated_at",
            params![
                i64::try_from(record.id.get()).unwrap_or(i64::MAX),
                record.state.as_str(),
                record.source_label,
                record.target_path.to_string_lossy(),
                record
                    .total_len
                    .map(|len| i64::try_from(len).unwrap_or(i64::MAX)),
                i64::from(record.range_supported),
                record.validators.strong,
                record.validators.weak,
                failure_json,
                record.name_hint,
                record
                    .rate_limit_bps
                    .map(|rate| i64::try_from(rate).unwrap_or(i64::MAX)),
                i64::try_from(record.created_at).unwrap_or(i64::MAX),
                i64::try_from(record.updated_at).unwrap_or(i64::MAX),
            ],
        )
        .map_err(map_sqlite("upsert_task"))?;

        // The segment layout is derived state, so replacing it wholesale avoids
        // having to diff it against what is on disk.
        tx.execute(
            "DELETE FROM segments WHERE task_id = ?1",
            params![i64::try_from(record.id.get()).unwrap_or(i64::MAX)],
        )
        .map_err(map_sqlite("clear_segments"))?;
        {
            let mut statement = tx
                .prepare(
                    "INSERT INTO segments (task_id, segment_id, start_byte, end_byte, durable_end)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )
                .map_err(map_sqlite("prepare_segment_insert"))?;
            for segment in &record.segments {
                statement
                    .execute(params![
                        i64::try_from(record.id.get()).unwrap_or(i64::MAX),
                        i64::from(segment.id),
                        i64::try_from(segment.start).unwrap_or(i64::MAX),
                        i64::try_from(segment.end).unwrap_or(i64::MAX),
                        segment
                            .durable_end
                            .map(|end| i64::try_from(end).unwrap_or(i64::MAX)),
                    ])
                    .map_err(map_sqlite("insert_segment"))?;
            }
        }
        tx.commit().map_err(map_sqlite("commit"))?;
        Ok(())
    }

    /// Every task in the store, in id order, with segments attached.
    pub fn load_all(&self) -> Result<Vec<TaskRecord>, Failure> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT id, state, source_label, target_path, total_len, range_supported,
                        strong_etag, weak_etag, failure_json, name_hint, rate_limit_bps,
                        created_at, updated_at
                 FROM tasks ORDER BY id ASC",
            )
            .map_err(map_sqlite("prepare_load"))?;
        let rows = statement
            .query_map([], |row| {
                let id: i64 = row.get(0)?;
                let state_raw: String = row.get(1)?;
                let failure_json: Option<String> = row.get(8)?;
                Ok((
                    id,
                    state_raw,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    failure_json,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<i64>>(10)?,
                    row.get::<_, i64>(11)?,
                    row.get::<_, i64>(12)?,
                ))
            })
            .map_err(map_sqlite("load"))?;

        let mut records = Vec::new();
        for row in rows {
            let (
                id,
                state_raw,
                source_label,
                target_path,
                total_len,
                range_supported,
                strong,
                weak,
                failure_json,
                name_hint,
                rate_limit_bps,
                created_at,
                updated_at,
            ) = row.map_err(map_sqlite("load_row"))?;

            let Some(state) = TaskState::from_str_id(&state_raw) else {
                return Err(Failure::new(
                    FailureKind::DiskOther,
                    FailureScope::Storage,
                    format!("unknown_state_{state_raw}"),
                )
                .not_retryable());
            };
            let failure = match failure_json {
                Some(json) => Some(serde_json::from_str(&json).map_err(map_serde("failure"))?),
                None => None,
            };
            let id = TaskId(u64::try_from(id).unwrap_or_default());
            let mut record = TaskRecord {
                id,
                state,
                source_label,
                target_path: std::path::PathBuf::from(target_path),
                total_len: total_len.and_then(|len| u64::try_from(len).ok()),
                range_supported: range_supported != 0,
                validators: Validators { strong, weak },
                segments: Vec::new(),
                failure,
                name_hint,
                created_at: u64::try_from(created_at).unwrap_or_default(),
                updated_at: u64::try_from(updated_at).unwrap_or_default(),
                rate_limit_bps: rate_limit_bps.and_then(|rate| u64::try_from(rate).ok()),
            };
            record.segments = self.load_segments(id)?;
            records.push(record);
        }
        Ok(records)
    }

    /// Load one task, or `None` when it is not in the store.
    pub fn load_task(&self, id: TaskId) -> Result<Option<TaskRecord>, Failure> {
        Ok(self.load_all()?.into_iter().find(|record| record.id == id))
    }

    fn load_segments(&self, id: TaskId) -> Result<Vec<SegmentState>, Failure> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT segment_id, start_byte, end_byte, durable_end
                 FROM segments WHERE task_id = ?1 ORDER BY segment_id ASC",
            )
            .map_err(map_sqlite("prepare_load_segments"))?;
        let rows = statement
            .query_map(
                params![i64::try_from(id.get()).unwrap_or(i64::MAX)],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .map_err(map_sqlite("load_segments"))?;
        let mut segments = Vec::new();
        for row in rows {
            let (segment_id, start, end, durable_end) = row.map_err(map_sqlite("segment_row"))?;
            segments.push(SegmentState {
                id: u32::try_from(segment_id).unwrap_or_default(),
                start: u64::try_from(start).unwrap_or_default(),
                end: u64::try_from(end).unwrap_or_default(),
                durable_end: durable_end.and_then(|end| u64::try_from(end).ok()),
            });
        }
        Ok(segments)
    }

    /// Engine-level settings, if any have been written.
    pub fn load_settings(&self) -> Result<EngineSettings, Failure> {
        Ok(EngineSettings {
            global_rate_limit_bps: self.read_meta_u64("global_rate_limit_bps")?,
        })
    }

    /// Persist engine-level settings.
    pub fn save_settings(&mut self, settings: &EngineSettings) -> Result<(), Failure> {
        match settings.global_rate_limit_bps {
            Some(rate) => self.write_meta("global_rate_limit_bps", &rate.to_string())?,
            None => self.clear_meta("global_rate_limit_bps")?,
        }
        Ok(())
    }

    fn read_meta_u64(&self, key: &str) -> Result<Option<u64>, Failure> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sqlite("read_meta"))?;
        Ok(raw.and_then(|value| value.parse::<u64>().ok()))
    }

    fn write_meta(&self, key: &str, value: &str) -> Result<(), Failure> {
        self.conn
            .execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map_err(map_sqlite("write_meta"))?;
        Ok(())
    }

    fn clear_meta(&self, key: &str) -> Result<(), Failure> {
        self.conn
            .execute("DELETE FROM meta WHERE key = ?1", params![key])
            .map_err(map_sqlite("clear_meta"))?;
        Ok(())
    }
}

fn map_sqlite(context: &'static str) -> impl Fn(rusqlite::Error) -> Failure {
    move |error| {
        let kind = match &error {
            rusqlite::Error::SqliteFailure(inner, _) => match inner.code {
                rusqlite::ErrorCode::DiskFull => FailureKind::DiskFull,
                rusqlite::ErrorCode::CannotOpen | rusqlite::ErrorCode::PermissionDenied => {
                    FailureKind::DiskOther
                }
                _ => FailureKind::DiskOther,
            },
            _ => FailureKind::DiskOther,
        };
        Failure::new(kind, FailureScope::Storage, format!("sqlite_{context}")).not_retryable()
    }
}

fn map_serde(context: &'static str) -> impl Fn(serde_json::Error) -> Failure {
    move |_| {
        Failure::new(
            FailureKind::DiskOther,
            FailureScope::Storage,
            format!("json_{context}"),
        )
        .not_retryable()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn record(id: u64, state: TaskState) -> TaskRecord {
        TaskRecord {
            id: TaskId(id),
            state,
            source_label: "https://example.test/file.bin".into(),
            target_path: PathBuf::from("file.bin"),
            total_len: Some(4096),
            range_supported: true,
            validators: Validators {
                strong: Some("etag-1".into()),
                weak: Some("Mon, 01 Jan 2024 00:00:00 GMT".into()),
            },
            segments: vec![
                SegmentState::new(0, 0, 2047),
                SegmentState::new(1, 2048, 4095),
            ],
            failure: None,
            name_hint: Some("hint.bin".into()),
            created_at: 100,
            updated_at: 200,
            rate_limit_bps: Some(1234),
        }
    }

    #[test]
    fn round_trips_a_task_with_segments() {
        let mut store = Store::open_in_memory().unwrap();
        let mut original = record(1, TaskState::Downloading);
        original.segments[0].mark_durable_through(1023);
        store.save_task(&original).unwrap();

        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0], original);
    }

    #[test]
    fn saving_again_replaces_segment_layout() {
        let mut store = Store::open_in_memory().unwrap();
        let mut current = record(1, TaskState::Downloading);
        store.save_task(&current).unwrap();

        current.segments = vec![SegmentState::new(0, 0, 4095)];
        current.state = TaskState::Queued;
        store.save_task(&current).unwrap();

        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].segments.len(), 1);
        assert_eq!(loaded[0].segments[0].end, 4095);
        assert_eq!(loaded[0].state, TaskState::Queued);
    }

    #[test]
    fn failed_state_keeps_its_structured_reason() {
        let mut store = Store::open_in_memory().unwrap();
        let mut current = record(7, TaskState::Failed);
        current.failure = Some(
            Failure::new(FailureKind::SourceMissing, FailureScope::Probe, "http_404")
                .with_status(404),
        );
        store.save_task(&current).unwrap();

        let loaded = store.load_all().unwrap();
        let failure = loaded[0].failure.clone().unwrap();
        assert_eq!(failure.kind, FailureKind::SourceMissing);
        assert_eq!(failure.status, Some(404));
        assert!(!failure.retryable);
    }

    #[test]
    fn task_ids_are_allocated_once_each() {
        let mut store = Store::open_in_memory().unwrap();
        assert_eq!(store.next_task_id().unwrap(), TaskId(1));
        assert_eq!(store.next_task_id().unwrap(), TaskId(2));
        assert_eq!(store.next_task_id().unwrap(), TaskId(3));
    }

    #[test]
    fn task_ids_keep_going_up_across_reopens() {
        // A second host process must not re-issue an id that is already in the
        // store: `save_task` upserts, so the collision would overwrite a task
        // that is still perfectly alive.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engine.db");
        let (first, second) = {
            let mut store = Store::open(&path).unwrap();
            let first = store.next_task_id().unwrap();
            store.save_task(&record(1, TaskState::Completed)).unwrap();
            drop(store);
            let mut reopened = Store::open(&path).unwrap();
            let second = reopened.next_task_id().unwrap();
            (first, second)
        };
        assert_eq!(first, TaskId(1));
        assert_eq!(second, TaskId(2), "the counter was reset by reopening");
        let store = Store::open(&path).unwrap();
        assert_eq!(store.load_all().unwrap().len(), 1);
    }

    #[test]
    fn settings_round_trip_including_clearing() {
        let mut store = Store::open_in_memory().unwrap();
        assert_eq!(store.load_settings().unwrap().global_rate_limit_bps, None);
        store
            .save_settings(&EngineSettings {
                global_rate_limit_bps: Some(65_536),
            })
            .unwrap();
        assert_eq!(
            store.load_settings().unwrap().global_rate_limit_bps,
            Some(65_536)
        );
        store
            .save_settings(&EngineSettings {
                global_rate_limit_bps: None,
            })
            .unwrap();
        assert_eq!(store.load_settings().unwrap().global_rate_limit_bps, None);
    }

    #[test]
    fn state_is_recoverable_from_a_file_backed_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("engine.db");
        {
            let mut store = Store::open(&path).unwrap();
            let mut current = record(1, TaskState::Downloading);
            current.segments[1].mark_durable_through(3000);
            store.save_task(&current).unwrap();
        }
        let store = Store::open(&path).unwrap();
        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].segments[1].durable_len(), 953);
        assert_eq!(loaded[0].durable_len(), 953);
    }

    #[test]
    fn corrupt_state_name_is_reported_not_guessed() {
        let mut store = Store::open_in_memory().unwrap();
        store.save_task(&record(1, TaskState::Queued)).unwrap();
        store
            .conn
            .execute("UPDATE tasks SET state = 'teleporting' WHERE id = 1", [])
            .unwrap();
        let failure = store.load_all().unwrap_err();
        assert_eq!(failure.detail, "unknown_state_teleporting");
    }

    #[test]
    fn load_task_filters_by_id() {
        let mut store = Store::open_in_memory().unwrap();
        store.save_task(&record(1, TaskState::Queued)).unwrap();
        store.save_task(&record(2, TaskState::Paused)).unwrap();
        assert!(store.load_task(TaskId(2)).unwrap().is_some());
        assert!(store.load_task(TaskId(9)).unwrap().is_none());
    }
}
