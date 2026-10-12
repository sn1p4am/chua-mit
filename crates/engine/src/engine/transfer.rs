//! Segment transfer: the loop that turns a range request into durably recorded
//! bytes — `spec/engine-v1.md` §6.
//!
//! Every response this module is willing to write goes through
//! [`crate::engine::ranges::validate_range_response`] first. There is no second
//! path to the file.

use std::sync::{Arc, Mutex};

use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;

use crate::engine::ranges::{RangeRequest, validate_range_response, verdict_failure};
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
    Ok((response, offset, plan.remaining()))
}

/// Read one segment to completion or to a stop signal.
///
/// Cancellation safety: the body is read, written and accounted for one chunk
/// at a time in that order. A stop between chunks leaves the file consistent
/// with the progress board, and the store is told about it by the caller.
pub async fn fetch_segment(context: &TransferContext, plan: SegmentPlan) -> SegmentOutcome {
    let id = plan.id;
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
            () = pace(global.as_ref(), task.as_ref(), length) => {}
        }
        if let Err(failure) = write_chunk(context, id, offset, &chunk) {
            return SegmentOutcome::Rejected { id, failure };
        }
        offset = offset.saturating_add(length);
        remaining = remaining.saturating_sub(length);
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

/// Run a whole segment layout to completion, with at most `concurrency`
/// segments in flight.
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
pub async fn run_layout(
    context: TransferContext,
    segments: Vec<SegmentState>,
    total_len: Option<u64>,
    concurrency: usize,
) -> LayoutOutcome {
    let plans = plans_for(&segments, total_len);
    // Nothing left to fetch. Returning here also covers the zero-length file,
    // whose layout has no segments at all.
    if plans.is_empty() {
        return LayoutOutcome::Finished;
    }

    let permits = Arc::new(Semaphore::new(concurrency.max(1)));
    let mut lanes: JoinSet<SegmentOutcome> = JoinSet::new();
    for plan in plans {
        let context = context.clone();
        let permits = Arc::clone(&permits);
        lanes.spawn(async move {
            let id = plan.id;
            let Ok(_permit) = permits.acquire_owned().await else {
                // Only `run_layout` closes the semaphore, and it cannot return
                // while a lane is still running, so this is unreachable in
                // practice; reporting it as a stop is the safe reading.
                return SegmentOutcome::Stopped { id };
            };
            fetch_segment(&context, plan).await
        });
    }

    let mut rejection: Option<Failure> = None;
    let mut stopped = false;
    while let Some(joined) = lanes.join_next().await {
        match joined {
            Ok(SegmentOutcome::Done { .. }) => {}
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

    match (rejection, stopped) {
        (Some(failure), _) => LayoutOutcome::Rejected { failure },
        (None, true) => LayoutOutcome::Stopped,
        (None, false) => LayoutOutcome::Finished,
    }
}

/// How a pass over the whole layout ended.
#[derive(Debug)]
pub enum LayoutOutcome {
    /// Every segment reported completion.
    Finished,
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
pub fn plans_for(segments: &[SegmentState], total_len: Option<u64>) -> Vec<SegmentPlan> {
    segments
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
