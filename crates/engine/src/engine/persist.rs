//! The actor's owned handle on durable state.
//!
//! `rusqlite::Connection` is `Send` but not `Sync`, because it holds a
//! `RefCell` for its statement cache. The actor is spawned on the
//! multi-threaded runtime, so `&Actor` has to be `Sync`; a plain field would
//! make the whole actor future `!Send` and `tokio::spawn` would refuse it.
//!
//! Wrapping the store in a `std::sync::Mutex` fixes that and buys a second
//! thing: the guard is `!Send`, so the compiler now *enforces* that no
//! persistence call is ever held across an `.await`. That is the discipline the
//! store's own documentation asks for — short, serialised, synchronous
//! statements — made a type-level rule instead of a convention.
//!
//! # Poisoning
//!
//! A poisoned mutex means some other thread panicked while holding it. That
//! says nothing about the data: every durability invariant lives in SQLite and
//! in `durable_end`, not in this in-process handle. So poisoning is recovered
//! from (`PoisonError::into_inner`) rather than propagated — a panic elsewhere
//! must not permanently cost the engine its ability to record progress. The
//! alternative, reporting it as a [`Failure`], would need `Store: Debug` purely
//! to build an error value nobody could act on.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use crate::engine::EngineError;
use crate::failure::Failure;
use crate::model::{TaskId, TaskRecord};
use crate::store::{EngineSettings, Store};

/// Durable state, owned by the actor alone.
pub struct Persist {
    store: Mutex<Store>,
}

impl Persist {
    /// Wrap an open store.
    pub fn new(store: Store) -> Self {
        Self {
            store: Mutex::new(store),
        }
    }

    /// Open (creating if needed) the database at `path`.
    pub fn open(path: &Path) -> Result<Self, Failure> {
        Ok(Self::new(Store::open(path)?))
    }

    /// Open an in-memory database.
    pub fn open_in_memory() -> Result<Self, Failure> {
        Ok(Self::new(Store::open_in_memory()?))
    }

    /// The only place the lock is taken, so the recovery rule has one home.
    fn lock(&self) -> MutexGuard<'_, Store> {
        match self.store.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Allocate the next task id.
    pub fn next_task_id(&self) -> Result<TaskId, Failure> {
        self.lock().next_task_id()
    }

    /// Write a task and its segments.
    pub fn save_task(&self, record: &TaskRecord) -> Result<(), Failure> {
        self.lock().save_task(record)
    }

    /// Every task in the store.
    pub fn load_all(&self) -> Result<Vec<TaskRecord>, Failure> {
        self.lock().load_all()
    }

    /// Engine-level settings.
    pub fn load_settings(&self) -> Result<EngineSettings, Failure> {
        self.lock().load_settings()
    }

    /// Persist engine-level settings.
    pub fn save_settings(&self, settings: &EngineSettings) -> Result<(), Failure> {
        self.lock().save_settings(settings)
    }

    /// Wrap a store failure in the engine's command error.
    pub fn unavailable(failure: Failure) -> EngineError {
        EngineError::Unavailable(Box::new(failure))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn persist_round_trips_a_task() {
        let persist = Persist::open_in_memory().unwrap();
        let id = persist.next_task_id().unwrap();
        assert_eq!(id, TaskId(1));
        let record = TaskRecord {
            id,
            state: crate::state::TaskState::Queued,
            source_label: "https://example.test/f".into(),
            target_path: std::path::PathBuf::from("f"),
            total_len: Some(10),
            range_supported: true,
            validators: Default::default(),
            segments: vec![crate::model::SegmentState::new(0, 0, 9)],
            failure: None,
            name_hint: None,
            created_at: 1,
            updated_at: 2,
            rate_limit_bps: None,
        };
        persist.save_task(&record).unwrap();
        let loaded = persist.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, TaskId(1));
    }

    #[test]
    fn settings_round_trip() {
        let persist = Persist::open_in_memory().unwrap();
        assert_eq!(persist.load_settings().unwrap().global_rate_limit_bps, None);
        persist
            .save_settings(&EngineSettings {
                global_rate_limit_bps: Some(4096),
            })
            .unwrap();
        assert_eq!(
            persist.load_settings().unwrap().global_rate_limit_bps,
            Some(4096)
        );
    }
}
