//! Task identity, the caller-supplied task seed, and progress arithmetic.
//!
//! See `spec/engine-v1.md` §2 and §3.3.

use std::path::PathBuf;

use crate::failure::Failure;
use crate::http::Validators;
use crate::state::TaskState;

/// Local task identity. Stable across process restarts.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct TaskId(pub u64);

impl TaskId {
    /// The underlying number, for persistence and display.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What the host supplies when it asks for a download.
///
/// The source identifier is kept verbatim — §3.3 requires `source_label` to be
/// stored as given, credentials and all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSeed {
    /// The source identifier, e.g. an HTTP URL.
    pub source_label: String,
    /// Destination directory. The final filename is derived by the probe.
    pub dest_dir: PathBuf,
    /// Host-supplied filename, used only when neither the response nor the URL
    /// yields a usable name (§4.2).
    pub name_hint: Option<String>,
    /// Host-supplied complete target path. When present it wins over the
    /// derived name, because the host asked for that exact path.
    pub explicit_target: Option<PathBuf>,
    /// Per-task transfer ceiling in bytes per second.
    pub rate_limit_bps: Option<u64>,
}

impl TaskSeed {
    /// A seed that writes the derived filename into `dest_dir`.
    pub fn new(source_label: impl Into<String>, dest_dir: impl Into<PathBuf>) -> Self {
        Self {
            source_label: source_label.into(),
            dest_dir: dest_dir.into(),
            name_hint: None,
            explicit_target: None,
            rate_limit_bps: None,
        }
    }
}

/// One byte interval of a task, plus its durable progress.
///
/// The interval is `[start, end]`, both inclusive. `durable_end` is the highest
/// offset, inclusive, that has been written *and* flushed *and* recorded in the
/// store. Bytes beyond it may well be on disk already; they are simply not
/// trusted, and are fetched again after a crash (§7.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentState {
    /// Stable within the task.
    pub id: u32,
    /// First byte, inclusive.
    pub start: u64,
    /// Last byte, inclusive.
    pub end: u64,
    /// Highest durably recorded byte, inclusive. `None` means nothing yet.
    pub durable_end: Option<u64>,
}

impl SegmentState {
    /// A fresh segment covering `[start, end]`.
    pub const fn new(id: u32, start: u64, end: u64) -> Self {
        Self {
            id,
            start,
            end,
            durable_end: None,
        }
    }

    /// Total bytes in the interval.
    pub const fn len(&self) -> u64 {
        self.end.saturating_sub(self.start) + 1
    }

    /// Whether the interval is empty (defensive).
    pub const fn is_empty(&self) -> bool {
        self.end < self.start
    }

    /// Durable bytes in this segment.
    pub fn durable_len(&self) -> u64 {
        match self.durable_end {
            Some(end) if end >= self.start => end - self.start + 1,
            _ => 0,
        }
    }

    /// The next byte that still has to be fetched.
    pub fn next_offset(&self) -> u64 {
        match self.durable_end {
            Some(end) if end >= self.start => end + 1,
            _ => self.start,
        }
    }

    /// Remaining bytes.
    pub fn remaining(&self) -> u64 {
        self.len().saturating_sub(self.durable_len())
    }

    /// Whether every byte of the interval is durably recorded.
    pub fn is_complete(&self) -> bool {
        self.durable_len() == self.len()
    }

    /// Record that everything up to and including `end` is durable.
    ///
    /// Never moves backwards: a late report from a winding-down worker cannot
    /// revoke progress that the store has already been told about.
    pub fn mark_durable_through(&mut self, end: u64) {
        let capped = end.min(self.end);
        if capped < self.start {
            return;
        }
        self.durable_end = Some(match self.durable_end {
            Some(current) => current.max(capped),
            None => capped,
        });
    }
}

/// Everything the engine knows about one task that is not derived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRecord {
    /// Local identity.
    pub id: TaskId,
    /// Lifecycle state.
    pub state: TaskState,
    /// Source identifier, verbatim.
    pub source_label: String,
    /// Final file path.
    pub target_path: PathBuf,
    /// Total length; `None` while unknown.
    pub total_len: Option<u64>,
    /// Whether the source honoured a range request.
    pub range_supported: bool,
    /// Validators observed at probe time.
    pub validators: Validators,
    /// Active segment layout.
    pub segments: Vec<SegmentState>,
    /// Failure that put the task in `Failed`, if any.
    pub failure: Option<Failure>,
    /// The source name the host supplied, if any. Kept so that a restart can
    /// re-derive the same target path without the original command line.
    pub name_hint: Option<String>,
    /// Creation time, Unix seconds.
    pub created_at: u64,
    /// Last modification time, Unix seconds.
    pub updated_at: u64,
    /// Per-task transfer ceiling.
    pub rate_limit_bps: Option<u64>,
}

impl TaskRecord {
    /// Bytes that are durably recorded across all segments.
    pub fn durable_len(&self) -> u64 {
        self.segments.iter().map(SegmentState::durable_len).sum()
    }

    /// Whether every active segment is complete.
    pub fn all_segments_complete(&self) -> bool {
        !self.segments.is_empty() && self.segments.iter().all(SegmentState::is_complete)
    }

    /// The union of all active segment intervals, as a coverage check.
    ///
    /// Returns the total bytes covered when the segments exactly tile
    /// `[0, total_len)` without overlap, which is invariant I1 of §2.
    pub fn coverage(&self) -> Coverage {
        let mut sorted: Vec<(u64, u64)> = self
            .segments
            .iter()
            .map(|segment| (segment.start, segment.end))
            .collect();
        sorted.sort_unstable();
        let mut covered = 0u64;
        let mut expected_start = 0u64;
        let mut overlaps = false;
        let mut gaps = false;
        for (start, end) in sorted {
            if start < expected_start {
                overlaps = true;
            } else if start > expected_start {
                gaps = true;
            }
            covered = covered.saturating_add(end.saturating_sub(start) + 1);
            expected_start = expected_start.max(end.saturating_add(1));
        }
        Coverage {
            covered,
            overlaps,
            gaps,
        }
    }
}

/// Result of the invariant check on a segment layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coverage {
    /// Total bytes spanned by the union of the intervals.
    pub covered: u64,
    /// Whether any two intervals overlap.
    pub overlaps: bool,
    /// Whether the layout leaves a hole.
    pub gaps: bool,
}

impl Coverage {
    /// Whether the layout is exactly a tiling of `[0, total)`.
    pub fn tiles(&self, total: u64) -> bool {
        !self.overlaps && !self.gaps && self.covered == total
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn record(segments: Vec<SegmentState>) -> TaskRecord {
        TaskRecord {
            id: TaskId(1),
            state: TaskState::Downloading,
            source_label: "https://example.test/f".into(),
            target_path: PathBuf::from("f"),
            total_len: Some(1000),
            range_supported: true,
            validators: Validators::default(),
            segments,
            failure: None,
            name_hint: None,
            created_at: 0,
            updated_at: 0,
            rate_limit_bps: None,
        }
    }

    #[test]
    fn segment_progress_is_the_durable_prefix() {
        let mut segment = SegmentState::new(0, 100, 199);
        assert_eq!(segment.len(), 100);
        assert_eq!(segment.durable_len(), 0);
        assert_eq!(segment.next_offset(), 100);
        assert_eq!(segment.remaining(), 100);
        assert!(!segment.is_complete());

        segment.mark_durable_through(149);
        assert_eq!(segment.durable_len(), 50);
        assert_eq!(segment.next_offset(), 150);
        assert_eq!(segment.remaining(), 50);

        segment.mark_durable_through(199);
        assert!(segment.is_complete());
        assert_eq!(segment.remaining(), 0);
    }

    #[test]
    fn durable_progress_never_goes_backwards() {
        let mut segment = SegmentState::new(0, 0, 99);
        segment.mark_durable_through(79);
        segment.mark_durable_through(10);
        assert_eq!(segment.durable_end, Some(79));
    }

    #[test]
    fn durable_progress_is_clamped_to_the_interval() {
        let mut segment = SegmentState::new(0, 10, 19);
        segment.mark_durable_through(1_000);
        assert_eq!(segment.durable_end, Some(19));
        let mut fresh = SegmentState::new(1, 10, 19);
        fresh.mark_durable_through(9);
        assert_eq!(fresh.durable_end, None);
    }

    #[test]
    fn task_coverage_tiles_the_whole_file() {
        let layout = record(vec![
            SegmentState::new(0, 0, 399),
            SegmentState::new(1, 400, 799),
            SegmentState::new(2, 800, 999),
        ]);
        let coverage = layout.coverage();
        assert!(coverage.tiles(1000), "{coverage:?}");
        assert_eq!(layout.durable_len(), 0);
        assert!(!layout.all_segments_complete());
    }

    #[test]
    fn coverage_detects_overlap_and_gaps() {
        let overlapped = record(vec![
            SegmentState::new(0, 0, 499),
            SegmentState::new(1, 400, 999),
        ]);
        assert!(overlapped.coverage().overlaps);
        assert!(!overlapped.coverage().tiles(1000));

        let gapped = record(vec![
            SegmentState::new(0, 0, 399),
            SegmentState::new(1, 500, 999),
        ]);
        let coverage = gapped.coverage();
        assert!(coverage.gaps);
        assert_eq!(coverage.covered, 900);
        assert!(!coverage.tiles(1000));
    }

    #[test]
    fn empty_layout_is_not_complete() {
        let empty = record(Vec::new());
        assert!(!empty.all_segments_complete());
        assert_eq!(empty.durable_len(), 0);
    }

    #[test]
    fn single_segment_by_value() {
        assert_eq!(TaskId(7).to_string(), "7");
        assert_eq!(TaskId(7).get(), 7);
        let segment = SegmentState::new(3, 5, 5);
        assert_eq!(segment.len(), 1);
        assert!(!segment.is_empty());
    }
}
