//! chua-engine — download engine core.
//!
//! The engine turns a source identifier into a complete local file while
//! maintaining a recoverable state record. It is deliberately free of any UI,
//! window or FFI dependency (`spec/engine-v1.md` §1), so that the whole
//! transfer path is coverable by headless integration tests.
//!
//! # Host contract
//!
//! * Commands come in through method calls on [`EngineHandle`].
//! * State goes out through a `tokio::sync::broadcast` channel of [`Event`]s
//!   ([`EngineHandle::subscribe`]).
//! * Because a broadcast receiver can lag, [`EngineHandle::snapshot`] returns
//!   the full state at any moment; a `Lagged` receive error is the host's
//!   signal to re-read it (ADR-0004).
//!
//! # No user-facing strings
//!
//! Nothing in this crate produces prose. Failures are structured values
//! ([`Failure`]) and the host owns wording (`spec/engine-v1.md` §8.4).

#![doc(html_no_source)]

pub mod config;
pub mod engine;
pub mod events;
pub mod failure;
pub mod filename;
pub mod http;
pub mod model;
pub mod state;
pub mod store;
pub mod util;

pub use config::{EngineConfig, SegmentPolicy};
pub use engine::{Engine, EngineError, EngineHandle};
pub use events::{EngineEvent, EventKind, SegmentView, Snapshot, TaskView};
pub use failure::{BackoffPolicy, Failure, FailureKind, FailureScope};
pub use model::{SegmentState, TaskId, TaskRecord, TaskSeed};
pub use state::{TaskState, TransitionError};
pub use store::Store;
