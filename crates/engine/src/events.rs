//! The host-facing event channel and the full-state query that complements it.
//!
//! Design follows ADR-0004: a `tokio::sync::broadcast` sender for the stream and
//! a snapshot query for recovery. A receiver that falls behind gets
//! `RecvError::Lagged`, and that is the contract's re-synchronisation signal —
//! the host answers it by calling the snapshot method again
//! (`spec/engine-v1.md` §10.1).

use crate::failure::Failure;
use crate::model::{SegmentState, TaskId, TaskRecord};
use crate::state::TaskState;

/// Default broadcast capacity. Small enough to be cheap, large enough that a
/// host polling at the event rate does not lag under normal operation.
pub const DEFAULT_EVENT_CAPACITY: usize = 1024;

/// Per-segment information as the host sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentView {
    /// Stable within the task.
    pub id: u32,
    /// First byte, inclusive.
    pub start: u64,
    /// Last byte, inclusive.
    pub end: u64,
    /// Durable bytes in this interval.
    pub done_len: u64,
    /// Bytes that have been written but not yet recorded as durable.
    pub pending_len: u64,
    /// Source identifier this segment is currently bound to.
    pub source_label: String,
}

impl SegmentView {
    /// Build a view from a durable record plus the in-memory received count.
    pub fn from_state(segment: &SegmentState, pending_len: u64, source_label: &str) -> Self {
        Self {
            id: segment.id,
            start: segment.start,
            end: segment.end,
            done_len: segment.durable_len(),
            pending_len,
            source_label: source_label.to_string(),
        }
    }
}

/// The full state of one task at a moment in time.
///
/// Every field the host needs is present, so a host that has just seen
/// `Lagged` can rebuild its view from a bare `Vec<TaskView>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskView {
    /// Identity.
    pub id: TaskId,
    /// Lifecycle state.
    pub state: TaskState,
    /// Source identifier, verbatim.
    pub source_label: String,
    /// Final file path.
    pub target_path: String,
    /// Total length; `None` when unknown. Deliberately not `0`.
    pub total_len: Option<u64>,
    /// Bytes confirmed durable. This is the number that survives a crash.
    pub done_len: u64,
    /// Bytes received and written but not yet confirmed durable. Shown so a
    /// progress bar keeps moving between flush ticks; never used for resume.
    pub received_len: u64,
    /// Whether the source honours range requests.
    pub range_supported: bool,
    /// Active segments.
    pub segments: Vec<SegmentView>,
    /// Recent transfer rate in bytes per second, from the sliding window.
    pub rate_bps: Option<u64>,
    /// Estimated seconds remaining; `None` means "unknown", which is a real
    /// answer and not a zero (§9.3).
    pub eta_secs: Option<u64>,
    /// Structured failure, when the task is failed.
    pub failure: Option<Failure>,
    /// Creation time, Unix seconds.
    pub created_at: u64,
    /// Last change, Unix seconds.
    pub updated_at: u64,
}

impl TaskView {
    /// Assemble a view from the durable record, live per-segment pending counts,
    /// and the current rate estimate.
    ///
    /// `on_disk` answers "how many of this segment's bytes are on the file",
    /// which is the durable prefix plus anything written since the last flush.
    /// Folding the two together is what lets `received_len` move between flush
    /// ticks while `done_len` stays conservative.
    pub fn assemble(
        record: &TaskRecord,
        on_disk: &dyn Fn(u32) -> u64,
        rate_bps: Option<u64>,
    ) -> Self {
        let segments: Vec<SegmentView> = record
            .segments
            .iter()
            .map(|segment| {
                let on_disk = on_disk(segment.id);
                let durable = segment.durable_len();
                SegmentView {
                    id: segment.id,
                    start: segment.start,
                    end: segment.end,
                    done_len: durable,
                    pending_len: on_disk.saturating_sub(durable),
                    source_label: record.source_label.clone(),
                }
            })
            .collect();
        let done_len = record.durable_len();
        // `received_len` is what a progress indicator should show, so it is
        // capped at the total: bytes past `total_len` are never part of the file,
        // and letting them through would make the progress bar run backwards as
        // the ETA computation saw a negative remainder.
        let on_disk_total: u64 = segments
            .iter()
            .map(|segment| segment.done_len.saturating_add(segment.pending_len))
            .sum();
        let received_len = match record.total_len {
            Some(total) => on_disk_total.min(total),
            None => on_disk_total,
        };
        Self {
            id: record.id,
            state: record.state,
            source_label: record.source_label.clone(),
            target_path: record.target_path.to_string_lossy().into_owned(),
            total_len: record.total_len,
            done_len,
            received_len,
            range_supported: record.range_supported,
            segments,
            rate_bps,
            eta_secs: eta(total_len_of(record), received_len, rate_bps),
            failure: record.failure.clone(),
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
    }

    /// Fraction complete in `[0.0, 1.0]`, or `None` when the length is unknown.
    ///
    /// §4.3: an unknown-length transfer is shown as a byte count, not as a
    /// percentage, so this returns `None` rather than pretending.
    pub fn progress_fraction(&self) -> Option<f64> {
        let total = self.total_len?;
        if total == 0 {
            return None;
        }
        Some((self.received_len as f64 / total as f64).clamp(0.0, 1.0))
    }
}

fn total_len_of(record: &TaskRecord) -> Option<u64> {
    record.total_len
}

/// Estimated remaining time, or `None` when the estimate would be noise.
///
/// Guard rails: no total length, no rate, a rate below 1 KiB/s, or a remaining
/// time beyond a week all report "unknown" instead of a number that jumps
/// around (§9.3).
fn eta(total_len: Option<u64>, received: u64, rate_bps: Option<u64>) -> Option<u64> {
    const MIN_USEFUL_RATE: u64 = 1024;
    const MAX_ETA_SECS: u64 = 7 * 24 * 3600;
    let total = total_len?;
    let rate = rate_bps?;
    if rate < MIN_USEFUL_RATE {
        return None;
    }
    let remaining = total.saturating_sub(received);
    if remaining == 0 {
        return Some(0);
    }
    let seconds = remaining / rate;
    (seconds <= MAX_ETA_SECS).then_some(seconds)
}

/// A full-state answer. The host renders this after a lag, at startup, or on
/// demand.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Every task known to the engine, in id order.
    pub tasks: Vec<TaskView>,
}

impl Snapshot {
    /// Find one task.
    pub fn get(&self, id: TaskId) -> Option<&TaskView> {
        self.tasks.iter().find(|task| task.id == id)
    }

    /// How many tasks are in the given state.
    pub fn count_in(&self, state: TaskState) -> usize {
        self.tasks.iter().filter(|task| task.state == state).count()
    }
}

/// Event kinds, kept as a separate tag so a host can filter on the discriminant
/// without destructuring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    /// A task entered the engine.
    TaskAdded,
    /// A task left the engine.
    TaskRemoved,
    /// A §3.2 transition happened.
    StateChanged,
    /// Periodic progress report.
    Progress,
    /// Segments appeared, completed, or were re-planned.
    SegmentsChanged,
    /// A task reached `Failed`.
    Failed,
    /// Concurrency-slot allocation changed.
    QueueChanged,
    /// The source set changed; reserved for M3, emitted for the single-source
    /// case when health or identity changes.
    SourcesChanged,
}

impl EventKind {
    /// Stable identifier for logs and host-side filters.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TaskAdded => "task_added",
            Self::TaskRemoved => "task_removed",
            Self::StateChanged => "state_changed",
            Self::Progress => "progress",
            Self::SegmentsChanged => "segments_changed",
            Self::Failed => "failed",
            Self::QueueChanged => "queue_changed",
            Self::SourcesChanged => "sources_changed",
        }
    }
}

/// One event on the wire.
///
/// Events carry the whole task view rather than a delta, so a host that misses
/// one can still make sense of the next. `task` is `None` for engine-level
/// notifications, such as a queue-depth change with no single owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineEvent {
    /// What kind of change this is.
    pub kind: EventKind,
    /// The task it concerns, when there is exactly one. Segments-changed and
    /// state-changed carry `Some`.
    pub task: Option<TaskView>,
    /// The task id, also present when `task` is `None`, so filters work.
    pub task_id: Option<TaskId>,
    /// Free-form machine-readable context, e.g. the old state on a transition.
    pub detail: Option<String>,
}

impl EngineEvent {
    /// A task-scoped event.
    pub fn task(kind: EventKind, view: TaskView) -> Self {
        Self {
            kind,
            task_id: Some(view.id),
            task: Some(view),
            detail: None,
        }
    }

    /// A task-scoped event with machine-readable context.
    pub fn task_with_detail(kind: EventKind, view: TaskView, detail: impl Into<String>) -> Self {
        Self {
            kind,
            task_id: Some(view.id),
            task: Some(view),
            detail: Some(detail.into()),
        }
    }

    /// An engine-level event with no single owning task.
    pub fn engine(kind: EventKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            task: None,
            task_id: None,
            detail: Some(detail.into()),
        }
    }
}

/// Rate limit for progress emission: at most `hz` events per task per second.
///
/// §9.4 requires a ceiling and forbids emitting per data block. This is a
/// decision function only; the caller keeps the timestamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressThrottle {
    min_interval: std::time::Duration,
}

impl ProgressThrottle {
    /// Build a throttle for a frequency in hertz. Zero is treated as 1 Hz.
    pub fn new(hz: u32) -> Self {
        let effective = hz.max(1);
        Self {
            min_interval: std::time::Duration::from_millis(1000 / u64::from(effective)),
        }
    }

    /// The shortest gap between two emissions.
    pub const fn min_interval(&self) -> std::time::Duration {
        self.min_interval
    }

    /// Whether an event may be emitted, given when the last one went out.
    ///
    /// `last` is `None` for the first emission, which is always allowed.
    pub fn allows(&self, last: Option<std::time::Instant>, now: std::time::Instant) -> bool {
        match last {
            None => true,
            Some(last) => now.duration_since(last) >= self.min_interval,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::http::Validators;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn record(segments: Vec<SegmentState>, total: Option<u64>) -> TaskRecord {
        TaskRecord {
            id: TaskId(4),
            state: TaskState::Downloading,
            source_label: "https://example.test/big.bin".into(),
            target_path: PathBuf::from("big.bin"),
            total_len: total,
            range_supported: true,
            validators: Validators::default(),
            segments,
            failure: None,
            name_hint: None,
            created_at: 10,
            updated_at: 20,
            rate_limit_bps: None,
        }
    }

    #[test]
    fn view_separates_durable_from_pending_bytes() {
        let mut first = SegmentState::new(0, 0, 999);
        first.mark_durable_through(499);
        let second = SegmentState::new(1, 1000, 1999);
        let record = record(vec![first, second], Some(2000));

        let view = TaskView::assemble(&record, &|id| if id == 1 { 300 } else { 0 }, Some(5000));
        assert_eq!(view.done_len, 500);
        assert_eq!(view.received_len, 800);
        assert_eq!(view.segments.len(), 2);
        assert_eq!(view.segments[0].done_len, 500);
        assert_eq!(view.segments[0].pending_len, 0);
        assert_eq!(view.segments[1].done_len, 0);
        assert_eq!(view.segments[1].pending_len, 300);
        assert_eq!(view.segments[0].source_label, record.source_label);
        assert_eq!(view.total_len, Some(2000));
        assert_eq!(view.rate_bps, Some(5000));
        // 1200 bytes left at 5000 B/s -> 0 whole seconds.
        assert_eq!(view.eta_secs, Some(0));
        assert_eq!(view.created_at, 10);
    }

    #[test]
    fn unknown_length_reports_no_percentage() {
        let record = record(vec![SegmentState::new(0, 0, 0)], None);
        let view = TaskView::assemble(&record, &|_| 0, Some(1000));
        assert_eq!(view.total_len, None);
        assert_eq!(view.progress_fraction(), None);
        assert_eq!(view.eta_secs, None);
    }

    #[test]
    fn eta_refuses_to_guess_from_a_weak_signal() {
        let task = record(vec![SegmentState::new(0, 0, 999_999)], Some(1_000_000));
        // No rate at all.
        assert_eq!(TaskView::assemble(&task, &|_| 0, None).eta_secs, None);
        // Rate too low to extrapolate.
        assert_eq!(TaskView::assemble(&task, &|_| 0, Some(10)).eta_secs, None);
        // Absurd remaining time: an exabyte at 1 kB/s is far past the ceiling,
        // so the answer is "unknown" rather than a number nobody can use.
        let huge = record(
            vec![SegmentState::new(0, 0, 999_999)],
            Some(1_000_000_000_000),
        );
        assert_eq!(TaskView::assemble(&huge, &|_| 0, Some(1000)).eta_secs, None);
        // A sane rate produces a number: 1 MB remaining at 100 kB/s.
        assert_eq!(
            TaskView::assemble(&task, &|_| 0, Some(100_000)).eta_secs,
            Some(10)
        );
        // Nothing left to fetch reports zero rather than a guess.
        let mut finished = SegmentState::new(0, 0, 999_999);
        finished.mark_durable_through(999_999);
        let done = record(vec![finished], Some(1_000_000));
        assert_eq!(
            TaskView::assemble(&done, &|_| 1_000_000, Some(4096)).eta_secs,
            Some(0)
        );
    }

    #[test]
    fn progress_fraction_is_bounded() {
        let mut segment = SegmentState::new(0, 0, 999);
        segment.mark_durable_through(999);
        let record = record(vec![segment], Some(1000));
        let view = TaskView::assemble(&record, &|_| 500, Some(1));
        assert_eq!(view.progress_fraction(), Some(1.0));
        let zero_len = record.clone();
        let zero_len = TaskRecord {
            total_len: Some(0),
            ..zero_len
        };
        assert_eq!(
            TaskView::assemble(&zero_len, &|_| 0, None).progress_fraction(),
            None
        );
    }

    #[test]
    fn snapshot_lookup_and_counting() {
        let snapshot = Snapshot {
            tasks: vec![
                TaskView::assemble(
                    &record(vec![SegmentState::new(0, 0, 9)], Some(10)),
                    &|_| 0,
                    None,
                ),
                TaskView::assemble(
                    &TaskRecord {
                        id: TaskId(5),
                        state: TaskState::Completed,
                        ..record(vec![SegmentState::new(0, 0, 9)], Some(10))
                    },
                    &|_| 0,
                    None,
                ),
            ],
        };
        assert_eq!(snapshot.count_in(TaskState::Downloading), 1);
        assert_eq!(snapshot.count_in(TaskState::Completed), 1);
        assert!(snapshot.get(TaskId(5)).is_some());
        assert!(snapshot.get(TaskId(99)).is_none());
    }

    #[test]
    fn throttle_limits_emission_rate() {
        let throttle = ProgressThrottle::new(10);
        assert_eq!(throttle.min_interval(), Duration::from_millis(100));
        let start = Instant::now();
        assert!(throttle.allows(None, start));
        assert!(!throttle.allows(Some(start), start + Duration::from_millis(50)));
        assert!(throttle.allows(Some(start), start + Duration::from_millis(100)));
        assert!(throttle.allows(Some(start), start + Duration::from_secs(1)));
        // Zero hertz degrades to 1 Hz rather than dividing by zero.
        assert_eq!(
            ProgressThrottle::new(0).min_interval(),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn event_constructors_set_the_right_fields() {
        let view = TaskView::assemble(
            &record(vec![SegmentState::new(0, 0, 9)], Some(10)),
            &|_| 0,
            None,
        );
        let event = EngineEvent::task(EventKind::Progress, view.clone());
        assert_eq!(event.kind, EventKind::Progress);
        assert_eq!(event.task_id, Some(TaskId(4)));
        assert!(event.detail.is_none());

        let event = EngineEvent::task_with_detail(EventKind::StateChanged, view, "paused");
        assert_eq!(event.detail.as_deref(), Some("paused"));

        let event = EngineEvent::engine(EventKind::QueueChanged, "active=2");
        assert_eq!(event.task_id, None);
        assert!(event.task.is_none());

        assert_eq!(EventKind::TaskAdded.as_str(), "task_added");
        assert_eq!(EventKind::SourcesChanged.as_str(), "sources_changed");
    }
}
