//! Segment transfer: the loop that turns a range request into durably recorded
//! bytes — `spec/engine-v1.md` §6.
//!
//! Every response this module is willing to write goes through
//! [`crate::engine::ranges::validate_range_response`] first. There is no second
//! path to the file.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Semaphore, mpsc, watch};
use tokio::task::JoinSet;

use crate::config::SegmentPolicy;
use crate::engine::ranges::{RangeRequest, validate_range_response, verdict_failure};
use crate::engine::split::{
    LagTracker, SplitDensity, SplitPlan, SplitPolicy, SplitReason, plan_split,
};
use crate::engine::throttle::{SharedLimiter, pace};
use crate::failure::{Failure, FailureKind, FailureScope};
use crate::http::{Validators, parse_content_range};
use crate::model::SegmentState;

/// Largest slice read from one body at a time. Bounds per-segment memory
/// independently of the file size (§12).
pub const CHUNK_BYTES: usize = 64 * 1024;

/// Everything a segment needs in order to be fetched.
#[derive(Clone)]
pub struct SegmentPlan {
    /// Segment identity.
    pub id: u32,
    /// First byte to fetch, inclusive.
    pub start: u64,
    /// Last byte to fetch, inclusive.
    pub end: u64,
    /// The engine's idea of the whole file length. `None` in unknown-length
    /// mode, where the request asks for the whole representation.
    pub total_len: Option<u64>,
    /// Number of bytes already durably recorded for this segment; the fetch
    /// resumes from `start + done`.
    pub done: u64,
}

impl SegmentPlan {
    /// The request this plan issues.
    pub fn request(&self) -> RangeRequest {
        let from = self.start.saturating_add(self.done);
        match self.total_len {
            None => RangeRequest::whole_file(None),
            Some(total) => {
                if self.start == 0 && self.end.saturating_add(1) >= total {
                    // A single segment covering the file: send an open-ended
                    // range so a server that wants to send everything may.
                    RangeRequest::whole_file(Some(total))
                } else {
                    RangeRequest::segment(from, self.end, total)
                }
            }
        }
    }

    /// Bytes still to fetch.
    pub fn remaining(&self) -> u64 {
        self.end
            .saturating_sub(self.start)
            .saturating_add(1)
            .saturating_sub(self.done)
    }

    /// The interval this plan's request asks for.
    ///
    /// For an open-ended request (`end == u64::MAX`) the request covers "the
    /// rest of the representation", whose size is not knowable from the request
    /// alone. The response is then judged against what is known locally instead:
    /// the `206` is still required to start where the interval starts, which is
    /// the only claim the engine can check without a declared length.
    fn request_len(&self) -> u64 {
        if self.end == u64::MAX {
            return u64::MAX;
        }
        // `done` is never past the interval's end for a plan built by
        // `plans_for`, but the saturating subtraction costs nothing and a
        // malformed plan must not be able to produce a huge "expected length".
        self.end
            .saturating_sub(self.start.saturating_add(self.done))
            .saturating_add(1)
    }

    /// Adjust the plan to a replacement interval issued by a §6.2 split.
    fn retarget(&mut self, start: u64, end: u64, done: u64) {
        self.start = start;
        self.end = end;
        self.done = done;
    }
}

/// Shared, concurrent progress record for one task's transfer.
///
/// `pending` counts bytes that have been written to the file but not yet
/// confirmed durable. It is deliberately *not* part of
/// [`crate::model::SegmentState`]: the durable value only moves when the store
/// has been told about it, while `pending` is allowed to be lost.
#[derive(Debug)]
pub struct ProgressBoard {
    segments: Vec<SegmentState>,
    pending: Vec<u64>,
    complete: Vec<bool>,
    /// Highest absolute offset each lane has reached. Derived from `pending` but
    /// kept separately because it is the value a lag detector reads before the
    /// bytes are confirmed durable.
    cursors: Vec<u64>,
    total_received: u64,
    completed_segments: u32,
}

impl ProgressBoard {
    /// A board for a fresh layout.
    pub fn new(segments: &[SegmentState]) -> Self {
        Self {
            segments: segments.to_vec(),
            pending: vec![0; segments.len()],
            complete: vec![false; segments.len()],
            cursors: segments.iter().map(|segment| segment.start).collect(),
            total_received: 0,
            completed_segments: 0,
        }
    }

    /// The current durable layout.
    pub fn segments(&self) -> &[SegmentState] {
        &self.segments
    }

    /// Pending bytes for one segment.
    pub fn pending_for(&self, id: u32) -> u64 {
        self.index(id).map_or(0, |index| self.pending[index])
    }

    /// Bytes received across the task, durable plus pending.
    pub fn received_len(&self) -> u64 {
        self.total_received
    }

    /// How many segments have finished.
    pub fn completed_segments(&self) -> u32 {
        self.completed_segments
    }

    /// Whether every segment has finished.
    pub fn all_complete(&self) -> bool {
        usize::try_from(self.completed_segments).unwrap_or(usize::MAX) == self.segments.len()
    }

    /// The full segment list as an owned copy.
    pub fn snapshot_segments(&self) -> Vec<SegmentState> {
        self.segments.clone()
    }

    fn index(&self, id: u32) -> Option<usize> {
        self.segments.iter().position(|segment| segment.id == id)
    }

    /// Advance the in-memory position. Called with the file lock already held,
    /// so the byte is on its way to the file before this returns.
    pub fn extend(&mut self, id: u32, end: u64) {
        let Some(index) = self.index(id) else {
            return;
        };
        let segment = &self.segments[index];
        let capped = end.min(segment.end);
        let current = self.pending[index];
        let durable = segment.durable_len();
        if capped < segment.start {
            return;
        }
        // Position within the interval, then the part of it that is new.
        let absolute = capped - segment.start + 1;
        if absolute > durable + current {
            self.pending[index] = absolute - durable;
            self.total_received = self
                .total_received
                .saturating_add(absolute - durable - current);
        }
        // The cursor tracks the lane's reach, not the task's bytes, so it moves
        // even when the write is a re-write of bytes already counted.
        self.cursors[index] = self.cursors[index].max(capped);
    }

    /// Confirm that everything written so far is durable, and return the new
    /// durable bounds for the store.
    ///
    /// The caller must have flushed the file before calling this. The returned
    /// values are exactly what may be written to the database.
    fn confirm_durable(&mut self) -> Vec<(u32, u64)> {
        let mut confirmed = Vec::with_capacity(self.segments.len());
        for index in 0..self.segments.len() {
            let pending = self.pending[index];
            if pending == 0 {
                continue;
            }
            let segment = &mut self.segments[index];
            let end = segment.start + segment.durable_len() + pending - 1;
            segment.mark_durable_through(end);
            self.pending[index] = 0;
            confirmed.push((segment.id, end));
        }
        confirmed
    }

    fn mark_complete(&mut self, id: u32) {
        if let Some(index) = self.index(id)
            && !self.complete[index]
        {
            self.complete[index] = true;
            self.completed_segments = self.completed_segments.saturating_add(1);
        }
    }

    /// Record the absolute offset a lane has reached in its interval.
    ///
    /// Purely diagnostic: it is what the lag detector reads to tell a lane that
    /// is busy from one that has stopped. It does not affect durability, which
    /// is why a lost update here is harmless.
    pub fn set_cursor(&mut self, id: u32, cursor: u64) {
        if let Some(index) = self.index(id) {
            self.cursors[index] = self.cursors[index].max(cursor);
        }
    }

    /// The offset a lane has reached, if the segment is known.
    pub fn cursor_for(&self, id: u32) -> Option<u64> {
        self.index(id).map(|index| self.cursors[index])
    }

    /// Bytes on disk for this interval: the durable prefix plus what has been
    /// written since the last flush.
    ///
    /// This is the number a lag detector needs. The durable count alone only
    /// moves at a flush, so a lane that is transferring perfectly happily can
    /// look identical to one that has hung for the length of a flush interval.
    pub fn on_disk_for(&self, id: u32) -> Option<u64> {
        let index = self.index(id)?;
        Some(
            self.segments[index]
                .durable_len()
                .saturating_add(self.pending[index]),
        )
    }

    /// Bytes confirmed durable for this interval.
    pub fn durable_bytes_for(&self, id: u32) -> u64 {
        self.index(id)
            .map_or(0, |index| self.segments[index].durable_len())
    }

    /// How many bytes are recorded for the whole task, durable plus pending.
    pub fn received_total(&self) -> u64 {
        self.total_received
    }

    /// Whether any segment *before* `id` holds bytes that are written but not
    /// yet confirmed durable.
    ///
    /// A split hands the durable prefix to the first half, so the split point is
    /// only correct once every byte an earlier lane may have written has been
    /// confirmed. Asking first keeps the common case — nothing pending — from
    /// forcing a file flush on every check tick.
    pub fn pending_before(&self, id: u32) -> bool {
        let Some(threshold) = self.index(id).map(|index| self.segments[index].start) else {
            return false;
        };
        self.segments
            .iter()
            .enumerate()
            .any(|(index, segment)| segment.id != id && segment.start < threshold && self.pending[index] > 0)
    }

    /// Halve one segment at its durable frontier, atomically.
    ///
    /// §6.2 as a mutation. Three things have to be true at once and there is no
    /// moment at which any of them is not:
    ///
    /// 1. **I1 holds.** The two new intervals are the old interval split at
    ///    `front_end + 1`, so no byte is claimed twice and none is dropped. The
    ///    caller holds the board lock across this call, and a chunk write has to
    ///    take the same lock to record what it wrote, so no observer — in the
    ///    scheduler or in the actor — can see a half-applied split.
    /// 2. **No durable progress is lost.** The first half's interval is exactly
    ///    the bytes already durable, so it is complete on creation; it inherits
    ///    the durability by virtue of being the same bytes, not by copying a
    ///    watermark across. The second half starts at the next byte with nothing
    ///    written.
    /// 3. **The second half keeps the lane's identity.** A lane in flight is
    ///    already fetching `[old.start, old.end]`; after the split the bytes it
    ///    has yet to fetch are exactly the second half, so the second half takes
    ///    the old id and the lane keeps its accounting. The first half is the
    ///    part that is finished with, so it takes the fresh id.
    ///
    /// `None` means the plan could not be applied, which happens when the board
    /// and the plan disagree about the layout — in which case doing nothing is
    /// the only safe answer. A completed segment is refused for the same reason
    /// the decision function refuses one: §6.2 forbids it, and this is the last
    /// line of defence in case a lane finished between deciding and applying.
    pub fn replace_split(&mut self, plan: &SplitPlan) -> Option<SplitOutcome> {
        let index = self.index(plan.id)?;
        // §6.2: a completed segment MUST NOT be split. Checked here as well as
        // in the decision function, because a lane can complete between the two.
        if self.complete[index] {
            return None;
        }
        let segment = self.segments[index].clone();
        // The plan has to describe the interval the board actually holds. A
        // mismatch means the layout moved between deciding and applying, and the
        // only safe response is to leave it alone.
        if segment.start != plan.front_start || segment.end != plan.back_end {
            return None;
        }
        if plan.front_end >= plan.back_start || plan.back_start > plan.back_end {
            return None;
        }
        // The split point is the durable frontier. If the board's own watermark
        // disagrees with the plan's the plan is stale, and applying it would
        // hand the first half bytes it has not confirmed.
        let durable = segment.durable_len();
        if durable == 0 || segment.start.saturating_add(durable).saturating_sub(1) != plan.front_end {
            return None;
        }

        let pending = self.pending[index];
        self.segments[index].end = plan.front_end;
        self.segments[index].durable_end = Some(plan.front_end);
        self.pending[index] = 0;
        self.complete[index] = true;
        self.completed_segments = self.completed_segments.saturating_add(1);

        self.segments.push(SegmentState {
            id: plan.id,
            start: plan.back_start,
            end: plan.back_end,
            durable_end: None,
        });
        self.pending.push(0);
        self.complete.push(false);
        self.cursors.push(plan.back_start);

        Some(SplitOutcome {
            front_id: self.segments[index].id,
            front_start: plan.front_start,
            front_end: plan.front_end,
            back_id: plan.id,
            back_start: plan.back_start,
            back_end: plan.back_end,
            pending_moved: pending,
            reason: plan.reason,
        })
    }
}

/// What a successful [`ProgressBoard::replace_split`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitOutcome {
    /// Identity of the finished first half.
    pub front_id: u32,
    /// First byte of the first half.
    pub front_start: u64,
    /// Last byte of the first half.
    pub front_end: u64,
    /// Identity of the second half — the id the split segment had.
    pub back_id: u32,
    /// First byte of the second half.
    pub back_start: u64,
    /// Last byte of the second half.
    pub back_end: u64,
    /// Bytes that were written but not yet confirmed when the split happened.
    ///
    /// The second half fetches them again. That is deliberate: assigning them to
    /// the first half would mean claiming bytes the store never confirmed, and
    /// the first half would then be "complete" over a hole.
    pub pending_moved: u64,
    /// Why the segment was chosen.
    pub reason: SplitReason,
}

/// What a finished fetch attempt means for the task.
#[derive(Debug)]
pub enum SegmentOutcome {
    /// The segment's interval is fully fetched.
    Done {
        /// Segment identity.
        id: u32,
    },
    /// The response did not carry the requested interval. Nothing was written
    /// for this attempt.
    Rejected {
        /// Segment identity.
        id: u32,
        /// Structured reason.
        failure: Failure,
    },
    /// The transfer was asked to stop. The segment keeps whatever it already
    /// persisted and is left incomplete.
    Stopped {
        /// Segment identity.
        id: u32,
    },
    /// A §6.2 split took this interval away. Whatever was written stays written
    /// and is accounted for; the bytes that are left belong to another lane.
    WoundDown {
        /// Segment identity.
        id: u32,
    },
}

/// Immutable context shared by every segment of one task.
#[derive(Clone)]
pub struct TransferContext {
    /// The shared HTTP client.
    pub client: reqwest::Client,
    /// Base URL — the final URL the probe settled on.
    pub base: reqwest::Url,
    /// Validators recorded at probe time.
    pub validators: Validators,
    /// Global limiter, when one is configured.
    pub global_limiter: Option<SharedLimiter>,
    /// Per-task limiter, when one is configured.
    pub task_limiter: Option<SharedLimiter>,
    /// The preallocated destination file.
    pub file: Arc<Mutex<std::fs::File>>,
    /// Live progress.
    pub board: Arc<Mutex<ProgressBoard>>,
    /// Stop signal. Watchers see `true` when the transfer must wind down.
    pub stop: watch::Receiver<bool>,
    /// Where a §6.2 split is announced.
    ///
    /// The report goes to the worker rather than being kept inside the pass
    /// because the worker's own copy of the layout — the one the completeness
    /// check and every re-plan read — has to learn about the new interval, and
    /// because the actor needs to hear about it for the host's benefit. It is a
    /// `try_send` from the transfer path, so a full channel cannot stall a
    /// transfer (§9.4).
    pub split_tx: mpsc::Sender<SplitOutcome>,
}

/// Read the whole digest of a range response, deciding first whether it may be
/// read at all.
///
/// Returns `Ok(bytes)` only for a response that passed the §6.3 guard, or
/// `Err(failure)` when the caller must not touch the file.
pub async fn open_validated_body(
    context: &TransferContext,
    plan: &SegmentPlan,
) -> Result<(reqwest::Response, u64, u64), Failure> {
    let request = plan.request();
    // The header must express the interval on the wire; `whole_file` uses an
    // open-ended range that every server understands.
    let header = match (plan.total_len, request.start, request.end) {
        (None, _, _) => "bytes=0-".to_string(),
        (Some(_), start, end) if end == u64::MAX => format!("bytes={start}-"),
        (Some(_), start, end) => format!("bytes={start}-{end}"),
    };
    let response = context
        .client
        .get(context.base.clone())
        .header(reqwest::header::RANGE, header)
        .send()
        .await
        .map_err(|error| crate::http::classify(&error, FailureScope::Transfer, "segment_send"))?;

    let status = response.status().as_u16();
    let content_range = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_content_range);
    let content_len = response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());

    let verdict = validate_range_response(&request, status, content_range, content_len);
    if let Some(failure) = verdict_failure(&verdict, FailureScope::Transfer) {
        return Err(failure);
    }
    // `is_writable` is checked by construction above: `verdict_failure` returns
    // `None` only for the two writable verdicts. The offsets they report must
    // still agree with the plan, so assert that here rather than trusting the
    // shape of the enum.
    let offset = verdict.write_offset(&request).ok_or_else(|| {
        Failure::new(
            FailureKind::DataMismatch,
            FailureScope::Transfer,
            "verdict_without_offset",
        )
        .not_retryable()
    })?;
    let expected_from = plan.start.saturating_add(plan.done);
    if offset != expected_from && offset != 0 {
        return Err(Failure::new(
            FailureKind::DataMismatch,
            FailureScope::Transfer,
            format!("write_offset_{offset}_expected_{expected_from}"),
        )
        .not_retryable());
    }
    Ok((response, offset, plan.request_len()))
}

/// A replacement interval the scheduler hands to a lane that is already running.
///
/// A §6.2 split does not stop the lane that owns the segment: the bytes that
/// lane has yet to fetch after the split are exactly the interval of the second
/// half, so it is told what those bytes are and carries on. The alternative —
/// aborting the lane and starting a fresh request — throws away a live
/// connection and whatever the response had already buffered, on the very
/// segment that was already behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentDirective {
    /// Carry on, but the interval is now `[start, end]` and `done` bytes of it
    /// are already accounted for.
    Retarget {
        /// First byte of the new interval.
        start: u64,
        /// Last byte of the new interval, inclusive.
        end: u64,
        /// Bytes of the new interval the board already has.
        done: u64,
    },
    /// Stop: the interval is no longer this lane's to fetch, and the file must
    /// not be written to again.
    Release,
}

/// Read one segment to completion or to a stop signal.
///
/// Cancellation safety: the body is read, written and accounted for one chunk
/// at a time in that order. A stop between chunks leaves the file consistent
/// with the progress board, and the store is told about it by the caller.
///
/// `directives` is how a §6.2 split reaches a lane that is already in flight. It
/// is watched on the same footing as the stop signal, so a lane blocked in
/// `response.chunk()` still hears about it immediately — which is the whole
/// point, because the lane being split is usually the one that is blocked.
/// [`SegmentDirective::Release`] tells the lane to wind the rest of its interval
/// down; that happens *after* the byte it is currently writing, so the file can
/// never gain a byte past the interval the board is willing to account for.
pub async fn fetch_segment(
    context: &TransferContext,
    mut plan: SegmentPlan,
    mut directives: watch::Receiver<Option<SegmentDirective>>,
) -> SegmentOutcome {
    let mut id = plan.id;
    let (mut response, mut offset, mut remaining) = match open_validated_body(context, &plan).await
    {
        Ok(triple) => triple,
        Err(failure) => return SegmentOutcome::Rejected { id, failure },
    };

    loop {
        if remaining == 0 {
            record_complete(context, id);
            return SegmentOutcome::Done { id };
        }
        if *context.stop.borrow() {
            return SegmentOutcome::Stopped { id };
        }
        let chunk = tokio::select! {
            biased;
            () = wait_for_stop(&context.stop) => return SegmentOutcome::Stopped { id },
            () = wait_for_directive(&mut directives) => {
                match *directives.borrow_and_update() {
                    // This lane is now the second half of a split: same bytes to
                    // fetch, new interval boundaries. The remaining count is
                    // recomputed from the interval rather than from the bytes
                    // already taken off the socket, so a directive that shrinks
                    // the interval mid-flight ends the lane instead of writing
                    // past the new end.
                    Some(SegmentDirective::Retarget { start, end, done }) => {
                        plan.retarget(start, end, done);
                        remaining = plan.remaining();
                        continue;
                    }
                    // The interval belongs to someone else now. Winding down here
                    // rather than aborting means the byte already in flight is
                    // still written and still accounted for.
                    _ => {
                        return SegmentOutcome::WoundDown { id };
                    }
                }
            }
            chunk = response.chunk() => chunk,
        };
        let chunk = match chunk {
            Ok(Some(chunk)) => chunk,
            Ok(None) => {
                // The body ended. With a known length that is either a clamped
                // range or a truncated response, and both are a re-plan rather
                // than a silent short write.
                //
                // In unknown-length mode there is no expected length to fall
                // short of: the request was the open-ended `bytes=0-` of §4.3
                // and the close of the connection is what delimits the
                // representation, so reaching the end of the body *is*
                // completion. The caller adopts the length it turned out to
                // have.
                if plan.total_len.is_none() {
                    record_complete(context, id);
                    return SegmentOutcome::Done { id };
                }
                return SegmentOutcome::Rejected {
                    id,
                    failure: Failure::new(
                        FailureKind::DataMismatch,
                        FailureScope::Transfer,
                        format!("body_short_by_{remaining}"),
                    )
                    .not_retryable(),
                };
            }
            Err(error) => {
                return SegmentOutcome::Rejected {
                    id,
                    failure: crate::http::classify(&error, FailureScope::Transfer, "body_read"),
                };
            }
        };

        let length = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
        if length > remaining {
            // More bytes than the interval allows: a server sending the whole
            // file as a 206 would look like this. Take only what belongs to the
            // interval so the write cannot run past the segment.
            let allowed = usize::try_from(remaining).unwrap_or(usize::MAX);
            let slice = chunk.slice(0..allowed.min(chunk.len()));
            if let Err(failure) = write_chunk(context, id, offset, &slice) {
                return SegmentOutcome::Rejected { id, failure };
            }
            offset = offset.saturating_add(u64::try_from(slice.len()).unwrap_or(0));
            remaining = 0;
            continue;
        }

        // Pace before writing, so the ceiling applies to what leaves the socket
        // rather than to what has already been buffered. The stop signal races
        // the wait so that a pause during a long pacing sleep takes effect
        // promptly rather than after it completes.
        let global = context.global_limiter.clone();
        let task = context.task_limiter.clone();
        tokio::select! {
            biased;
            () = wait_for_stop(&context.stop) => return SegmentOutcome::Stopped { id },
            () = wait_for_directive(&mut directives) => {
                match *directives.borrow_and_update() {
                    Some(SegmentDirective::Retarget { start, end, done }) => {
                        plan.retarget(start, end, done);
                        id = plan.id;
                        remaining = plan.remaining();
                    }
                    _ => return SegmentOutcome::WoundDown { id },
                }
            }
            () = pace(global.as_ref(), task.as_ref(), length) => {}
        }
        if let Err(failure) = write_chunk(context, id, offset, &chunk) {
            return SegmentOutcome::Rejected { id, failure };
        }
        offset = offset.saturating_add(length);
        remaining = remaining.saturating_sub(length);
    }
}

/// Wait until a directive is published. Returns immediately when one already is.
async fn wait_for_directive(directives: &mut watch::Receiver<Option<SegmentDirective>>) {
    if directives.borrow_and_update().is_some() {
        return;
    }
    loop {
        if directives.changed().await.is_err() {
            // Every sender is gone, so the pass is over and so is this lane.
            return;
        }
        if directives.borrow_and_update().is_some() {
            return;
        }
    }
}

/// Write one chunk and advance the shared progress board under the file lock.
///
/// The board is updated while the file lock is still held, so a task abort
/// between the write and the report can only under-count progress, never
/// attribute bytes to the wrong interval.
fn write_chunk(
    context: &TransferContext,
    id: u32,
    offset: u64,
    bytes: &[u8],
) -> Result<(), Failure> {
    use std::io::{Seek, SeekFrom, Write};

    let mut file = context.file.lock().map_err(|_| {
        Failure::new(
            FailureKind::DiskOther,
            FailureScope::Storage,
            "file_lock_poisoned",
        )
        .not_retryable()
    })?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| crate::http::classify_io(&error, "seek"))?;
    file.write_all(bytes)
        .map_err(|error| crate::http::classify_io(&error, "write"))?;
    let mut board = context.board.lock().map_err(|_| {
        Failure::new(
            FailureKind::Unknown,
            FailureScope::Model,
            "progress_lock_poisoned",
        )
        .not_retryable()
    })?;
    if !bytes.is_empty() {
        board.extend(id, offset + u64::try_from(bytes.len()).unwrap_or(1) - 1);
    }
    Ok(())
}

fn record_complete(context: &TransferContext, id: u32) {
    if let Ok(mut board) = context.board.lock() {
        board.mark_complete(id);
    }
}

/// Wait until the stop flag flips. Used inside `select!` so a pause takes
/// effect at the next chunk boundary rather than at the end of the file.
pub async fn wait_for_stop(stop: &watch::Receiver<bool>) {
    let mut stop = stop.clone();
    loop {
        if *stop.borrow() {
            return;
        }
        if stop.changed().await.is_err() {
            // The sender was dropped: treat it as a stop request rather than
            // waiting forever.
            return;
        }
    }
}

/// Everything one pass over a layout needs.
///
/// A struct rather than a parameter list because the pass now has two jobs —
/// fetch the bytes and watch for a segment that has fallen behind — and both
/// need the same configuration.
pub struct LayoutRun {
    /// Shared transfer context.
    pub context: TransferContext,
    /// The intervals to fetch.
    pub segments: Vec<SegmentState>,
    /// The length the task believes the file has.
    pub total_len: Option<u64>,
    /// How many intervals may be in flight at once.
    pub concurrency: usize,
    /// Layout rules, for the §6.1 floor a split must respect.
    pub policy: SegmentPolicy,
    /// Splitting rules (§6.2).
    pub split: SplitPolicy,
}

/// Run a whole segment layout to completion, with at most `concurrency`
/// segments in flight, splitting a lagging segment when §6.2 says to.
///
/// Returns once every segment has finished, the layout has to be re-planned, or
/// the transfer was stopped. The caller decides what the task's next state is.
///
/// Each outstanding segment gets its own lane and a permit bounds how many run
/// at once. Every lane therefore returns exactly once, which is what lets the
/// join loop below know the pass is over — an earlier version kept lanes alive
/// on a shared work queue and parked the idle ones on a future that never
/// completes, so a fully successful pass never returned at all.
///
/// One rejected segment aborts the pass: the caller's remedy for a bad interval
/// response is to re-probe and re-plan, not to keep hammering the same endpoint.
///
/// # Splitting
///
/// The loop is a join loop with a check tick beside it. While any lane is
/// running, the scheduler wakes every `split.check_interval` and asks
/// [`plan_split`] whether some interval should be halved. A "yes" is applied to
/// the progress board under the board's own lock — the same lock every chunk
/// write takes to record what it wrote — and the bytes the old lane had not
/// reached are handed to a new lane in this same pass. Nothing else in the pass
/// is disturbed: lanes that are making progress are not paused, not
/// re-requested and not re-planned.
///
/// Two rules keep the loop from spinning. Every failed attempt to apply a split
/// waits `check_interval` for the next tick, and §6.2's cooldown is measured
/// from the last split that actually happened rather than from the last one that
/// was considered.
pub async fn run_layout(run: LayoutRun) -> LayoutOutcome {
    let LayoutRun {
        context,
        segments,
        total_len,
        concurrency,
        policy,
        split,
    } = run;
    let plans = plans_for(&segments, total_len);
    // Nothing left to fetch. Returning here also covers the zero-length file,
    // whose layout has no segments at all.
    if plans.is_empty() {
        return LayoutOutcome::Finished {
            segments: segments.clone(),
        };
    }

    let permits = Arc::new(Semaphore::new(concurrency.max(1)));
    let mut lanes: JoinSet<SegmentOutcome> = JoinSet::new();
    // Keyed by task id so an abort can name the one lane it wants to release.
    // `abort_handle` is the only way to target a single lane: `JoinSet` offers
    // `abort_all` and nothing finer, and aborting every lane would throw away
    // the progress of the ones that are doing well.
    //
    // The sender lives here too, which also keeps the channel open: a lane
    // parked on `changed()` treats a closed channel as a release, so a channel
    // that closed early would wind lanes down for no reason.
    type LaneOwner = (u32, watch::Sender<Option<SegmentDirective>>, tokio::task::AbortHandle);
    let mut owners: std::collections::HashMap<tokio::task::Id, LaneOwner> =
        std::collections::HashMap::new();

    for plan in plans {
        let id = plan.id;
        let (tx, rx) = watch::channel(None);
        let context_for_lane = context.clone();
        let permits_for_lane = Arc::clone(&permits);
        let handle = lanes.spawn(async move {
            let Ok(_permit) = permits_for_lane.acquire_owned().await else {
                // Only `run_layout` closes the semaphore, and it cannot return
                // while a lane is still running, so this is unreachable in
                // practice; reporting a release is the safe reading.
                return SegmentOutcome::WoundDown { id };
            };
            fetch_segment(&context_for_lane, plan, rx).await
        });
        owners.insert(handle.id(), (id, tx, handle));
    }

    // The lag tracker starts from what the board already holds, so a lane that
    // is slow from the very first chunk is measured from the start of the pass
    // rather than from the first time it moved.
    let mut tracker = LagTracker::new();
    let started = tokio::time::Instant::now();
    for segment in &segments {
        if segment.is_complete() {
            continue;
        }
        tracker.register(segment.id, received_of(&context, segment.id), started);
    }

    let mut ticker = tokio::time::interval(split.check_interval.max(Duration::from_millis(10)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick of a `tokio` interval fires immediately, and a split at
    // that instant would be a split with no evidence behind it.
    ticker.tick().await;

    let mut rejection: Option<Failure> = None;
    let mut stopped = false;
    let mut last_split: Option<tokio::time::Instant> = None;

    loop {
        let joined = tokio::select! {
            maybe = lanes.join_next(), if !lanes.is_empty() => Some(maybe),
            _ = ticker.tick(), if split.enabled && !lanes.is_empty() => None,
        };
        // The tick arm. Nothing in here may return early on a failed split: "no
        // segment is eligible" and "the eligible one cannot be halved" are both
        // ordinary answers, and the pass simply carries on.
        let Some(joined) = joined else {
            let now = tokio::time::Instant::now();
            let within_cooldown = last_split
                .is_some_and(|at| now.duration_since(at) < split.cooldown);
            tracker.observe(&|id| Some(received_of(&context, id)), now);

            let Some((layout, density)) = board_view(&context) else {
                continue;
            };
            let stalled: Vec<u32> = layout
                .iter()
                .map(|segment| segment.id)
                .filter(|id| tracker.has_stalled(*id, now, split.stall))
                .collect();
            // §6.2's "has fallen behind", made precise: the segment's own
            // completion is below the task's mean completion by `lag_ratio`, and
            // it owns at least `lag_share` of the bytes still outstanding. The
            // second half of that test matters — a nearly finished segment that
            // is behind in ratio terms has nothing left worth splitting.
            let covered = total_len.unwrap_or(0);
            let remaining_total: u64 = layout
                .iter()
                .map(|segment| segment.len().saturating_sub(density.durable_bytes(segment.id)))
                .sum();
            let mean_pace = if covered == 0 {
                0.0
            } else {
                let done: u64 = layout
                    .iter()
                    .map(|segment| density.durable_bytes(segment.id))
                    .sum();
                (done as f64 / covered as f64).clamp(0.0, 1.0)
            };
            let behind: Vec<u32> = layout
                .iter()
                .filter(|segment| {
                    let length = segment.len();
                    if length == 0 {
                        return false;
                    }
                    let pace = density.durable_bytes(segment.id) as f64 / length as f64;
                    let remaining = length.saturating_sub(density.durable_bytes(segment.id));
                    let share = if remaining_total == 0 {
                        0.0
                    } else {
                        remaining as f64 / remaining_total as f64
                    };
                    mean_pace - pace >= split.lag_ratio && share >= split.lag_share
                })
                .map(|segment| segment.id)
                .collect();

            let Some(plan) = plan_split(
                &layout,
                &density,
                &|id| behind.contains(&id),
                &|id| stalled.contains(&id),
                &split,
                policy.min_segment_bytes,
                policy.max_segments,
                within_cooldown,
            ) else {
                continue;
            };
            let Some(applied) = apply_split(&context, &plan) else {
                continue;
            };
            // §6.2's cooldown is measured from the last split that actually
            // happened, so a refused one does not start the clock.
            last_split = Some(now);
            if let Err(error) = context.split_tx.try_send(applied.clone()) {
                tracing::debug!(?error, "split report dropped");
            }

            // Take the interval away from the lane that had it, then give the
            // bytes it had not reached to a new lane. The order is deliberate:
            // the released lane winds down at its next chunk boundary and writes
            // nothing past the interval the board is willing to account for, so
            // there is no window in which two lanes may write the same byte.
            let mut released = None;
            for (task_id, (owner, tx, abort)) in &owners {
                if *owner != plan.id {
                    continue;
                }
                released = Some((*task_id, abort.clone()));
                if tx.send(Some(SegmentDirective::Release)).is_err() {
                    tracing::trace!("split lane had already finished");
                }
                break;
            }
            if let Some((task_id, abort)) = released {
                abort.abort();
                owners.remove(&task_id);
            }

            let fresh = SegmentPlan {
                id: applied.back_id,
                start: applied.back_start,
                end: applied.back_end,
                total_len,
                done: 0,
            };
            let id = fresh.id;
            let (tx, rx) = watch::channel(None);
            let context_for_lane = context.clone();
            let permits_for_lane = Arc::clone(&permits);
            let handle = lanes.spawn(async move {
                let Ok(_permit) = permits_for_lane.acquire_owned().await else {
                    return SegmentOutcome::WoundDown { id };
                };
                fetch_segment(&context_for_lane, fresh, rx).await
            });
            owners.insert(handle.id(), (applied.back_id, tx, handle));
            tracker.register(applied.back_id, applied.back_start, now);
            continue;
        };

        let Some(joined) = joined else { break };
        match joined {
            Ok(SegmentOutcome::Done { .. }) => {}
            // A lane that released its interval, or one whose permit was never
            // handed out. Neither ends the pass.
            Ok(SegmentOutcome::WoundDown { .. }) => {}
            Ok(SegmentOutcome::Stopped { .. }) => {
                stopped = true;
                lanes.abort_all();
                break;
            }
            Ok(SegmentOutcome::Rejected { failure, .. }) => {
                rejection.get_or_insert(failure);
                lanes.abort_all();
                break;
            }
            Err(_join_error) => {
                // An aborted or panicked lane. The durable progress is still
                // valid, so this is a re-plan rather than a hard failure.
                stopped = true;
                lanes.abort_all();
                break;
            }
        }
    }
    // Lanes that were aborted above, and lanes that had not yet taken a permit.
    // Waiting for them here is what makes the pass's return value trustworthy.
    lanes.shutdown().await;

    // The board is the authority on the layout once a split has happened, so the
    // caller is handed the board's own view rather than the one it passed in.
    let segments = match context.board.lock() {
        Ok(board) => board.snapshot_segments(),
        Err(_) => segments,
    };

    match (rejection, stopped) {
        (Some(failure), _) => LayoutOutcome::Rejected { failure },
        (None, true) => LayoutOutcome::Stopped,
        (None, false) => LayoutOutcome::Finished { segments },
    }
}

/// Apply a split to the board, flushing first if a byte an earlier lane wrote
/// has not been confirmed yet.
///
/// The flush is conditional because the ordinary case has nothing pending, and
/// forcing a file flush on every check tick would be a real cost for no gain.
/// When something *is* pending it has to be confirmed first: the split point is
/// the durable frontier, and a frontier that is behind a written byte would
/// leave that byte claimed by neither half.
fn apply_split(context: &TransferContext, plan: &SplitPlan) -> Option<SplitOutcome> {
    let needs_flush = match context.board.lock() {
        Ok(board) => board.pending_before(plan.id) || board.pending_for(plan.id) > 0,
        Err(_) => return None,
    };
    if needs_flush {
        crate::engine::job::flush_once(&context.board, &context.file);
    }
    let mut board = context.board.lock().ok()?;
    board.replace_split(plan)
}

/// The board's layout together with its density, borrowed for one decision.
///
/// The board lock is held for the whole of a check tick. That is deliberate: the
/// decision and the mutation it leads to must see the same board, or a lane
/// could complete a segment in between and the split would be applied to a state
/// the plan was not computed from.
fn board_view(context: &TransferContext) -> Option<(Vec<SegmentState>, BoardDensity)> {
    let board = context.board.lock().ok()?;
    let layout = board.snapshot_segments();
    let density = BoardDensity {
        durable: layout
            .iter()
            .map(|segment| (segment.id, segment.durable_len()))
            .collect(),
    };
    Some((layout, density))
}

/// How much of each segment is confirmed durable, in the form [`plan_split`]
/// wants.
///
/// The split point is the *durable* frontier, so this is the durable count and
/// not the on-disk one: the two differ by whatever has been written since the
/// last flush, and handing the first half a byte the store has not confirmed
/// would make it complete over a hole.
struct BoardDensity {
    durable: Vec<(u32, u64)>,
}

impl crate::engine::split::SplitDensity for BoardDensity {
    fn durable_bytes(&self, id: u32) -> u64 {
        self.durable
            .iter()
            .find(|(candidate, _)| *candidate == id)
            .map_or(0, |(_, bytes)| *bytes)
    }
}

/// Bytes of one interval that are on disk: durable plus pending.
fn received_of(context: &TransferContext, id: u32) -> u64 {
    match context.board.lock() {
        Ok(board) => board.on_disk_for(id).unwrap_or(0),
        Err(_) => 0,
    }
}

/// How a pass over the whole layout ended.
#[derive(Debug)]
pub enum LayoutOutcome {
    /// Every segment reported completion. `segments` is the board's own final
    /// layout, which differs from the one the pass started with whenever §6.2
    /// split something.
    Finished {
        /// The layout as the board ended up holding it.
        segments: Vec<SegmentState>,
    },
    /// A response disagreed with its request. Nothing extra was written.
    Rejected {
        /// Structured reason, for the caller to classify.
        failure: Failure,
    },
    /// The caller asked to stop.
    Stopped,
}

/// Build the plan list for a layout, given the current durable positions and
/// the length the task believes the file has.
pub fn plans_for(segments: &[SegmentState], total_len: Option<u64>) -> Vec<SegmentPlan> {    segments
        .iter()
        .filter(|segment| !segment.is_complete())
        .map(|segment| SegmentPlan {
            id: segment.id,
            start: segment.start,
            end: segment.end,
            total_len,
            done: segment.durable_len(),
        })
        .collect()
}

/// Confirm pending writes as durable. The caller must have flushed the file.
pub fn mark_durable(board: &Arc<Mutex<ProgressBoard>>) -> Vec<(u32, u64)> {
    match board.lock() {
        Ok(mut board) => board.confirm_durable(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn plans_request_the_exact_interval() {
        let plan = SegmentPlan {
            id: 1,
            start: 1024,
            end: 2047,
            total_len: Some(8192),
            done: 0,
        };
        assert_eq!(plan.request(), RangeRequest::segment(1024, 2047, 8192));
        assert_eq!(plan.remaining(), 1024);
    }

    #[test]
    fn a_resumed_plan_starts_where_the_durable_bytes_end() {
        let plan = SegmentPlan {
            id: 1,
            start: 0,
            end: 1023,
            total_len: Some(8192),
            done: 512,
        };
        assert_eq!(plan.request(), RangeRequest::segment(512, 1023, 8192));
        assert_eq!(plan.remaining(), 512);
    }

    #[test]
    fn a_single_segment_covering_the_file_asks_for_everything() {
        let plan = SegmentPlan {
            id: 0,
            start: 0,
            end: 4095,
            total_len: Some(4096),
            done: 0,
        };
        assert!(plan.request().covers_whole_file());
        assert_eq!(plan.request().start, 0);
    }

    #[test]
    fn unknown_length_plans_ask_for_the_whole_representation() {
        let plan = SegmentPlan {
            id: 0,
            start: 0,
            end: u64::MAX,
            total_len: None,
            done: 0,
        };
        assert!(plan.request().covers_whole_file());
    }

    #[test]
    fn progress_board_tracks_pending_separately_from_durable() {
        let board = Arc::new(Mutex::new(ProgressBoard::new(&[
            SegmentState::new(0, 0, 99),
            SegmentState::new(1, 100, 199),
        ])));
        {
            let mut guard = board.lock().unwrap();
            guard.extend(0, 49);
            guard.extend(1, 149);
        }
        {
            let guard = board.lock().unwrap();
            assert_eq!(guard.pending_for(0), 50);
            assert_eq!(guard.pending_for(1), 50);
            assert_eq!(guard.received_len(), 100);
            assert_eq!(guard.segments()[0].durable_len(), 0);
        }
        let confirmed = mark_durable(&board);
        assert_eq!(confirmed.len(), 2);
        {
            let guard = board.lock().unwrap();
            assert_eq!(guard.segments()[0].durable_len(), 50);
            assert_eq!(guard.segments()[1].durable_len(), 50);
            assert_eq!(guard.pending_for(0), 0);
            assert_eq!(guard.received_len(), 100);
        }
    }

    #[test]
    fn progress_board_ignores_repeated_and_rewound_reports() {
        let board = Arc::new(Mutex::new(ProgressBoard::new(&[SegmentState::new(
            0, 0, 99,
        )])));
        {
            let mut guard = board.lock().unwrap();
            guard.extend(0, 19);
            guard.extend(0, 19);
            guard.extend(0, 9);
        }
        let guard = board.lock().unwrap();
        assert_eq!(guard.pending_for(0), 20);
        assert_eq!(guard.received_len(), 20);
    }

    #[test]
    fn progress_board_never_runs_past_the_interval() {
        let board = Arc::new(Mutex::new(ProgressBoard::new(&[SegmentState::new(
            0, 10, 19,
        )])));
        {
            let mut guard = board.lock().unwrap();
            guard.extend(0, 1_000_000);
        }
        let guard = board.lock().unwrap();
        assert_eq!(guard.pending_for(0), 10);
        assert_eq!(guard.received_len(), 10);
    }

    #[test]
    fn completing_segments_is_counted_once() {
        let board = Arc::new(Mutex::new(ProgressBoard::new(&[
            SegmentState::new(0, 0, 99),
            SegmentState::new(1, 100, 199),
        ])));
        {
            let mut guard = board.lock().unwrap();
            assert!(!guard.all_complete());
            guard.mark_complete(0);
            guard.mark_complete(0);
            assert_eq!(guard.completed_segments(), 1);
            guard.mark_complete(1);
            assert!(guard.all_complete());
            assert_eq!(guard.completed_segments(), 2);
        }
    }

    #[test]
    fn plans_skip_completed_segments() {
        let mut done = SegmentState::new(0, 0, 99);
        done.mark_durable_through(99);
        let plans = plans_for(&[done, SegmentState::new(1, 100, 199)], Some(200));
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].id, 1);
        assert_eq!(plans[0].total_len, Some(200));
    }

    #[test]
    fn writes_are_attributed_to_the_segment_that_owns_the_offset() {
        // Regression guard for byte misattribution: segment 1 starts at byte
        // 100, so a write reported at byte 100 must move segment 1 and not
        // segment 0.
        let board = Arc::new(Mutex::new(ProgressBoard::new(&[
            SegmentState::new(0, 0, 99),
            SegmentState::new(1, 100, 199),
        ])));
        {
            let mut guard = board.lock().unwrap();
            guard.extend(1, 199);
        }
        let guard = board.lock().unwrap();
        assert_eq!(guard.pending_for(0), 0);
        assert_eq!(guard.pending_for(1), 100);
        assert_eq!(guard.received_len(), 100);
    }

    #[test]
    fn a_report_for_an_unknown_segment_is_dropped_not_attributed() {
        let board = Arc::new(Mutex::new(ProgressBoard::new(&[SegmentState::new(
            0, 0, 99,
        )])));
        {
            let mut guard = board.lock().unwrap();
            guard.extend(42, 50);
        }
        let guard = board.lock().unwrap();
        assert_eq!(guard.pending_for(0), 0);
        assert_eq!(guard.received_len(), 0);
    }
}
