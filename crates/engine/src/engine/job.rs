//! One task's worker: probe, plan, transfer, persist, verify.
//!
//! The worker owns a task's transfer and nothing else. It never mutates the
//! engine's records directly — every state change and every durable byte count
//! goes to the actor as a [`JobReport`], so there is exactly one writer for each
//! task's persisted state (`spec/engine-v1.md` §7, ADR-0004).

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Semaphore, mpsc, watch};

use crate::config::EngineConfig;
use crate::engine::throttle::{RateWindow, SharedLimiter};
use crate::engine::transfer::{self, LayoutOutcome, ProgressBoard, TransferContext};
use crate::engine::{Command, same_shape};
use crate::failure::{Failure, FailureKind, FailureScope};
use crate::http::ProbeOutcome;
use crate::model::{SegmentState, TaskId, TaskRecord};
use crate::state::TaskState;

/// What the host wants the worker to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobPhase {
    /// Run: transfer to completion.
    Run,
    /// Wind the transfer down and hold position.
    Pause,
    /// The task was parked in `Queued`; pick the work back up.
    Resume,
    /// Re-probe and start over from what is durably on disk.
    Retry,
    /// Stop; the task reached a terminal state.
    Stop,
}

impl JobPhase {
    fn code(self) -> u8 {
        match self {
            Self::Run => 0,
            Self::Pause => 1,
            Self::Resume => 2,
            Self::Retry => 3,
            Self::Stop => 4,
        }
    }

    fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Pause,
            2 => Self::Resume,
            3 => Self::Retry,
            4 => Self::Stop,
            _ => Self::Run,
        }
    }
}

/// The actor's handle on a worker.
#[derive(Debug)]
pub struct JobControl {
    phase: AtomicU8,
    sender: watch::Sender<u8>,
    task_limit: Arc<LimitSlot>,
}

impl JobControl {
    /// A control pair: the actor keeps the handle, the worker gets the receiver.
    pub fn new() -> (Self, JobControlReceiver) {
        let (sender, receiver) = watch::channel(JobPhase::Run.code());
        let control = Self {
            phase: AtomicU8::new(JobPhase::Run.code()),
            sender,
            task_limit: Arc::new(LimitSlot::default()),
        };
        (control, JobControlReceiver { phase: receiver })
    }

    /// Ask the worker to change phase.
    pub fn set_phase(&self, phase: JobPhase) {
        self.phase.store(phase.code(), Ordering::Release);
        // `send` fails only when there is no receiver, which means the worker
        // has already exited.
        if self.sender.send(phase.code()).is_err() {
            tracing::trace!(phase = ?phase, "worker already gone");
        }
    }

    /// The phase the worker was last asked for.
    pub fn phase(&self) -> JobPhase {
        JobPhase::from_code(self.phase.load(Ordering::Acquire))
    }

    /// Replace the per-task rate limiter, or clear it.
    ///
    /// An existing limiter is reconfigured in place rather than replaced, so a
    /// transfer that already holds a clone sees the new ceiling immediately.
    pub fn set_task_limit(&self, bytes_per_second: Option<u64>) {
        let existing = self.task_limit.get();
        match (existing, bytes_per_second) {
            (Some(limiter), Some(rate)) => limiter.set_rate(Some(rate)),
            (Some(limiter), None) => limiter.set_rate(None),
            (None, Some(rate)) => {
                self.task_limit
                    .set(Some(crate::engine::throttle::RateLimiter::shared(Some(
                        rate,
                    ))))
            }
            (None, None) => {}
        }
    }

    /// The current per-task limiter, if any.
    pub fn task_limit(&self) -> Option<SharedLimiter> {
        self.task_limit.get()
    }

    /// The slot the worker reads from. Shared, not cloned, so reconfiguration
    /// reaches the running transfer.
    pub fn limit_slot(&self) -> Arc<LimitSlot> {
        Arc::clone(&self.task_limit)
    }
}

/// The worker's half of the control pair.
#[derive(Debug)]
pub struct JobControlReceiver {
    phase: watch::Receiver<u8>,
}

impl JobControlReceiver {
    /// Current phase.
    pub fn phase(&self) -> JobPhase {
        JobPhase::from_code(*self.phase.borrow())
    }

    /// Wait for the next phase change.
    pub async fn changed(&mut self) -> JobPhase {
        if self.phase.changed().await.is_err() {
            // The actor dropped the handle: treat it as a stop.
            return JobPhase::Stop;
        }
        self.phase()
    }
}

/// A change the worker wants the actor to record.
#[derive(Debug)]
pub struct JobReport {
    /// Which task.
    pub task: TaskId,
    /// What changed.
    pub kind: JobReportKind,
}

/// The kinds of change a worker reports.
#[derive(Debug)]
pub enum JobReportKind {
    /// A request to move the task's state, with the state the worker believed
    /// it was in. The actor refuses the move if the record disagrees, which is
    /// how a stale worker is kept from overwriting newer state.
    State {
        /// From.
        from: TaskState,
        /// To.
        to: TaskState,
        /// Structured reason, for `Failed`.
        failure: Option<Failure>,
    },
    /// The segment layout changed.
    Segments {
        /// The new layout.
        segments: Vec<SegmentState>,
    },
    /// Periodic progress. `segments` carries durable positions that the caller
    /// has already flushed to the file.
    Progress {
        /// Layout with updated durable positions.
        segments: Vec<SegmentState>,
        /// Bytes received across the task, durable plus pending, used for the
        /// rate window.
        received: u64,
        /// Segments that have finished.
        segments_complete: u32,
    },
    /// The worker is exiting.
    Done,
}

/// Everything a worker needs that the actor owns.
pub struct WorkerContext {
    /// Task identity.
    pub id: TaskId,
    /// The record as it stood when the worker was started.
    pub record: TaskRecord,
    /// Engine configuration.
    pub config: EngineConfig,
    /// Shared HTTP client.
    pub client: reqwest::Client,
    /// Concurrency slots, shared across all tasks.
    pub permits: Arc<Semaphore>,
    /// Where reports go.
    pub(crate) reports: mpsc::Sender<Command>,
    /// Phase control from the actor.
    pub control: JobControlReceiver,
    /// Sliding-window rate estimate.
    pub rate_window: Arc<RateWindow>,
    /// Live progress, shared with the actor so a snapshot can show pending
    /// bytes without waiting for a report.
    pub board: Arc<Mutex<ProgressBoard>>,
    /// Global limiter, when one is configured.
    pub global_limiter: Option<SharedLimiter>,
    /// This task's own limiter, shared with the actor.
    pub limit_slot: Arc<LimitSlot>,
}

impl WorkerContext {
    fn report(&self, kind: JobReportKind) {
        let report = JobReport {
            task: self.id,
            kind,
        };
        if self
            .reports
            .try_send(Command::Report(Box::new(report)))
            .is_err()
        {
            // The actor's inbox is full or closed. Dropping a report is safe for
            // `Progress` (the next one supersedes it) and for `Done` (the actor
            // notices the closed channel), and a state report is retried by the
            // caller's next step. Blocking the transfer path is worse than
            // losing a progress tick, which is what §9.4 asks for.
            tracing::trace!(task = %self.id, "report dropped");
        }
    }

    fn set_state(&self, from: TaskState, to: TaskState) {
        self.report(JobReportKind::State {
            from,
            to,
            failure: None,
        });
    }

    fn fail(&self, from: TaskState, failure: Failure) {
        self.report(JobReportKind::State {
            from,
            to: TaskState::Failed,
            failure: Some(failure),
        });
    }
}

/// How many bytes were written to the target and confirmed durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreparedTarget {
    /// Durable bytes across the whole file.
    pub durable_len: u64,
    /// Whether pre-existing bytes beyond the durable record had to be dropped.
    /// §7.3(5) calls for exactly this conservative reset.
    pub truncated_extra: bool,
}

/// Drive one task from wherever it is to wherever it can get.
///
/// The loop is intentionally flat: each iteration handles exactly one requested
/// phase and then re-reads the control state, so a pause that arrives during a
/// transfer is honoured before the next one starts.
pub async fn run(mut context: WorkerContext) {
    let mut state = context.record.state;
    loop {
        match context.control.phase() {
            JobPhase::Stop => break,
            JobPhase::Pause => {
                if !wait_until_running(&mut context).await {
                    break;
                }
                continue;
            }
            JobPhase::Run | JobPhase::Resume | JobPhase::Retry => {}
        }

        if matches!(context.control.phase(), JobPhase::Retry) {
            context.record.segments.clear();
        }
        // Acquire a concurrency slot. `Queued -> Downloading` is the transition
        // that the slot represents (§3.2).
        let Ok(_permit) = context.permits.clone().acquire_owned().await else {
            break;
        };
        if context.control.phase() == JobPhase::Stop {
            break;
        }

        let outcome = run_once(&mut context, state).await;
        match outcome {
            StepOutcome::Finished => break,
            StepOutcome::Failed => {
                // The task is dead. Whatever the file holds beyond the bytes
                // that were confirmed durable cannot be attributed to an
                // interval, and leaving it in place would leave a file that
                // looks complete at the destination of a download that failed —
                // the one outcome this project is built to avoid.
                discard_unconfirmed_tail(&context);
                break;
            }
            StepOutcome::Restart(next) => {
                state = next;
            }
        }
    }
    context.report(JobReportKind::Done);
}

/// Drop the part of the target file that no interval can vouch for.
///
/// Only reached when a task is giving up for good. §7.3(5) already says that a
/// file disagreeing with the segment record is to be read conservatively, and
/// the alternative here is worse than conservative: a full-size file of
/// arbitrary bytes sitting at the destination of a download that failed is
/// indistinguishable from a completed one to everything except a checksum.
///
/// Everything that was confirmed durable is kept, because that is what a later
/// resume re-uses. Note that the retained prefix may contain holes — bytes
/// belonging to a segment that never ran — which is harmless: resume drives off
/// the per-segment durable positions, not off the file's length, so those
/// intervals are fetched again.
fn discard_unconfirmed_tail(context: &WorkerContext) {
    let confirmed = match context.board.lock() {
        Ok(board) => {
            if board.all_complete() {
                // Every byte was fetched and confirmed, so the failure is not a
                // reason to throw the data away.
                return;
            }
            board
                .snapshot_segments()
                .iter()
                .map(|segment| segment.start.saturating_add(segment.durable_len()))
                .max()
                .unwrap_or(0)
        }
        Err(_) => return,
    };
    let path = &context.record.target_path;
    let file = match std::fs::OpenOptions::new().write(true).open(path) {
        Ok(file) => file,
        // Either the task never got as far as creating a file, or the path is
        // not a file at all (a task that failed during the probe still carries
        // its destination *directory*). Neither is made worse by doing nothing.
        Err(error) => {
            tracing::debug!(path = %path.display(), kind = ?error.kind(), "nothing to trim");
            return;
        }
    };
    if let Err(error) = file.set_len(confirmed) {
        tracing::warn!(
            path = %path.display(),
            kind = ?error.kind(),
            "could not drop the unconfirmed tail"
        );
    }
}

/// Wait for the actor to ask for work again. Returns `false` when the worker
/// should exit.
async fn wait_until_running(context: &mut WorkerContext) -> bool {
    loop {
        match context.control.changed().await {
            JobPhase::Stop => return false,
            JobPhase::Run | JobPhase::Resume | JobPhase::Retry => return true,
            JobPhase::Pause => continue,
        }
    }
}

enum StepOutcome {
    /// The task reached a terminal state.
    Finished,
    /// The task failed; already reported.
    Failed,
    /// The worker should loop again, starting from this state.
    Restart(TaskState),
}

async fn run_once(context: &mut WorkerContext, state: TaskState) -> StepOutcome {
    match state {
        TaskState::Queued | TaskState::Probing | TaskState::Created => {
            run_probe_and_transfer(context, state).await
        }
        TaskState::Paused => {
            if wait_until_running(context).await {
                StepOutcome::Restart(context.control.phase_state())
            } else {
                StepOutcome::Finished
            }
        }
        TaskState::Downloading | TaskState::Verifying => {
            // A record that claims to be mid-transfer is normalised to `Queued`
            // at restore time, so reaching this state means the worker was
            // restarted while its own report was in flight.
            StepOutcome::Restart(TaskState::Queued)
        }
        TaskState::Completed | TaskState::Failed | TaskState::Removed => StepOutcome::Finished,
    }
}

impl JobControlReceiver {
    /// The state the current phase implies.
    fn phase_state(&self) -> TaskState {
        match self.phase() {
            JobPhase::Retry => TaskState::Probing,
            _ => TaskState::Queued,
        }
    }
}

async fn run_probe_and_transfer(context: &mut WorkerContext, state: TaskState) -> StepOutcome {
    // `Created -> Probing` and `Queued -> Downloading` are not the same edge, so
    // walk the worker's own view of the state forward one step at a time.
    let mut current = state;
    if current == TaskState::Created {
        context.set_state(current, TaskState::Probing);
        current = TaskState::Probing;
    }
    if current == TaskState::Probing {
        context.set_state(current, TaskState::Queued);
        current = TaskState::Queued;
    }

    let probe = match probe_with_retry(context).await {
        Ok(probe) => probe,
        Err(failure) => {
            context.fail(current, failure);
            return StepOutcome::Failed;
        }
    };

    // §I2: the length is fixed once known. A source that disagrees is a
    // different file, and §5.2 forbids merging the two.
    if let (Some(known), Some(again)) = (context.record.total_len, probe.total_len)
        && known != again
    {
        let failure = Failure::new(
            FailureKind::DataMismatch,
            FailureScope::Probe,
            format!("length_changed_{again}_was_{known}"),
        )
        .not_retryable();
        context.fail(current, failure);
        return StepOutcome::Failed;
    }
    context.record.total_len = context.record.total_len.or(probe.total_len);
    context.record.range_supported = probe.range_supported;
    context.record.validators = probe.validators.clone();

    let total_len = context.record.total_len;
    let use_ranges = context.record.range_supported && total_len.is_some();
    let layout = layout_for(context, use_ranges);
    // The layout the pass below runs over has to be the worker's own record of
    // it, not a local: the completeness check after the pass, the retry path
    // and the segment reports all read `record.segments`, and a layout that
    // lived only in a local made every one of them see an empty task that
    // finished instantly with nothing written.
    context.record.segments = layout.clone();
    context.report(JobReportKind::Segments {
        segments: layout.clone(),
    });
    if let Ok(mut board) = context.board.lock() {
        *board = ProgressBoard::new(&layout);
    }

    let prepared = match prepare_target(&context.record.target_path, total_len, &layout) {
        Ok(prepared) => prepared,
        Err(failure) => {
            context.fail(current, failure);
            return StepOutcome::Failed;
        }
    };
    let _ = prepared;

    context.set_state(current, TaskState::Downloading);
    current = TaskState::Downloading;

    transfer_loop(context, current, &probe).await
}

/// The segment layout to use, preserving durable progress where the intervals
/// still line up.
fn layout_for(context: &WorkerContext, use_ranges: bool) -> Vec<SegmentState> {
    let total_len = context.record.total_len;
    let desired: Vec<(u64, u64)> = match (total_len, use_ranges) {
        // §4.3: an unknown length degrades to a single sequential segment.
        (None, _) => vec![(0, u64::MAX)],
        (Some(total), false) => {
            if total == 0 {
                Vec::new()
            } else {
                vec![(0, total - 1)]
            }
        }
        (Some(total), true) => context.config.segment_policy.plan(total, true),
    };
    let existing = &context.record.segments;

    let mut layout = Vec::with_capacity(desired.len());
    for (index, (start, end)) in desired.into_iter().enumerate() {
        let id = u32::try_from(index).unwrap_or(u32::MAX);
        let previous = existing
            .iter()
            .find(|segment| segment.start == start && segment.end == end);
        match previous {
            Some(segment) => layout.push(SegmentState {
                id,
                start,
                end,
                // Durable progress carries over only when the interval is
                // byte-for-byte the same.
                durable_end: segment.durable_end,
            }),
            None => layout.push(SegmentState::new(id, start, end)),
        }
    }
    layout
}

/// Prepare the destination file for the transfer.
///
/// Two invariants meet here and they pull in opposite directions:
///
/// * §I3 — the engine must not delete or truncate user-visible data without an
///   explicit instruction;
/// * §7.3(5) — when the segment record and the file disagree, be conservative:
///   truncate to the confirmed byte count and re-fetch the rest.
///
/// The two are reconciled by resolving the conflict *before* this runs: a path
/// that already exists is reported to the host (§7.4) and the task never reaches
/// the transfer. So by the time this is called, any file present is one this
/// task created on a previous attempt, and the conservative truncation of
/// §7.3(5) applies to it. A file longer than the durable record is the residual
/// case of a crash between a write and its commit: the extra bytes cannot be
/// attributed to an interval, so they are dropped rather than trusted.
fn prepare_target(
    path: &std::path::Path,
    total_len: Option<u64>,
    layout: &[SegmentState],
) -> Result<PreparedTarget, Failure> {
    use std::fs::OpenOptions;

    let durable_len: u64 = layout.iter().map(SegmentState::durable_len).sum();
    let existing_len = match std::fs::metadata(path) {
        Ok(meta) => Some(meta.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(crate::http::classify_io(&error, "stat_target")),
    };

    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|error| crate::http::classify_io(&error, "open_target"))?;

    let mut truncated_extra = false;
    let mut confirmed = durable_len;
    if let Some(len) = existing_len {
        if len > durable_len {
            file.set_len(durable_len)
                .map_err(|error| crate::http::classify_io(&error, "truncate_target"))?;
            truncated_extra = true;
        } else if len < durable_len {
            // The store claims more than the file holds. The smaller of the two
            // is the trustworthy figure, and the next progress report corrects
            // the store.
            confirmed = len;
        }
    }

    // Preallocate so the whole range can be written at any offset (§6.4).
    if let Some(total) = total_len {
        let current = file
            .metadata()
            .map_err(|error| crate::http::classify_io(&error, "stat_target"))?
            .len();
        if current < total {
            file.set_len(total)
                .map_err(|error| crate::http::classify_io(&error, "preallocate"))?;
        }
    }

    Ok(PreparedTarget {
        durable_len: confirmed,
        truncated_extra,
    })
}

async fn probe_with_retry(context: &WorkerContext) -> Result<ProbeOutcome, Failure> {
    let url = reqwest::Url::parse(&context.record.source_label).map_err(|_| {
        Failure::new(
            FailureKind::Unknown,
            FailureScope::Probe,
            "source_not_a_url",
        )
        .not_retryable()
    })?;
    let mut attempt = 1u32;
    loop {
        match crate::http::probe(&context.client, &url, context.config.probe_timeout).await {
            Ok(outcome) => return Ok(outcome),
            Err(failure) => {
                if !failure.retryable {
                    return Err(failure);
                }
                let jitter = jitter_sample(&context.record.source_label, attempt);
                match context
                    .config
                    .backoff
                    .next_delay(attempt, failure.retry_after, jitter)
                {
                    Some(delay) => {
                        tokio::time::sleep(delay).await;
                        attempt = attempt.saturating_add(1);
                    }
                    None => return Err(failure),
                }
            }
        }
    }
}

fn jitter_sample(seed: &str, attempt: u32) -> f64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    seed.hash(&mut hasher);
    attempt.hash(&mut hasher);
    crate::http::now_unix().hash(&mut hasher);
    ((hasher.finish() >> 11) as f64) / ((1u64 << 53) as f64)
}

/// The transfer passes, including the §6.3 recovery path.
async fn transfer_loop(
    context: &mut WorkerContext,
    mut current: TaskState,
    probe: &ProbeOutcome,
) -> StepOutcome {
    let mut attempt = 1u32;
    let mut replans = 0u32;
    loop {
        let layout = context.record.segments.clone();
        let (stop_tx, stop_rx) = watch::channel(false);
        let task_limit = context.task_rate_limiter();
        let transfer_context = TransferContext {
            client: context.client.clone(),
            base: probe.final_url.clone(),
            validators: context.record.validators.clone(),
            global_limiter: context.global_limiter.clone(),
            task_limiter: task_limit,
            file: match open_for_transfer(&context.record.target_path) {
                Ok(file) => Arc::new(Mutex::new(file)),
                Err(failure) => {
                    context.fail(current, failure);
                    return StepOutcome::Failed;
                }
            },
            board: Arc::clone(&context.board),
            stop: stop_rx.clone(),
        };

        let concurrency = usize::try_from(context.config.segment_policy.max_segments).unwrap_or(1);
        // A pause or a stop that arrived before this pass began must take effect
        // immediately rather than after the whole file is fetched.
        if matches!(context.control.phase(), JobPhase::Pause | JobPhase::Stop)
            && let Err(error) = stop_tx.send(true)
        {
            tracing::trace!(?error, "stop signal had no receiver");
        }

        let flusher = tokio::spawn(flush_loop(
            Arc::clone(&context.board),
            Arc::clone(&transfer_context.file),
            stop_rx,
            context.config.progress_flush_interval,
        ));

        let total_len = context.record.total_len.or(probe.total_len);
        let outcome = tokio::select! {
            outcome = transfer::run_layout(
                transfer_context.clone(),
                layout.clone(),
                total_len,
                concurrency,
            ) => outcome,
            phase = context.control.changed() => {
                // Wind the transfer down, wait for the lanes to finish their
                // current chunk, then read the durable positions.
                if let Err(error) = stop_tx.send(true) {
                    tracing::trace!(?error, "stop signal had no receiver");
                }
                let _settled = transfer::run_layout(
                    transfer_context.clone(),
                    layout.clone(),
                    total_len,
                    concurrency,
                ).await;
                if let Err(error) = stop_tx.send(true) {
                    tracing::trace!(?error, "stop signal had no receiver");
                }
                if let Err(join_error) = flusher.await {
                    tracing::warn!(?join_error, "flush task ended abnormally");
                }
                settle_progress(context, &transfer_context.file);
                return match phase {
                    JobPhase::Stop => StepOutcome::Finished,
                    _ => StepOutcome::Restart(TaskState::Queued),
                };
            }
        };
        if let Err(error) = stop_tx.send(true) {
            tracing::trace!(?error, "stop signal had no receiver");
        }
        // Flush what the transfer wrote before reading the durable positions.
        if let Err(join_error) = flusher.await {
            tracing::warn!(?join_error, "flush task ended abnormally");
        }
        settle_progress(context, &transfer_context.file);

        match outcome {
            LayoutOutcome::Finished => {
                // §4.3: a close-delimited body has just said how long the
                // representation is. Adopt it before the completeness check,
                // which compares against a finite interval.
                adopt_observed_length(context);
            }
            LayoutOutcome::Stopped => return StepOutcome::Restart(TaskState::Queued),
            LayoutOutcome::Rejected { failure } => {
                if failure.kind != FailureKind::DataMismatch {
                    if failure.retryable
                        && let Some(delay) = context.config.backoff.next_delay(
                            attempt,
                            failure.retry_after,
                            jitter_sample(&context.record.source_label, attempt),
                        )
                    {
                        tokio::time::sleep(delay).await;
                        attempt = attempt.saturating_add(1);
                        continue;
                    }
                    context.fail(current, failure);
                    return StepOutcome::Failed;
                }
                // §6.3 recovery. A response that did not carry the requested
                // interval means the source's shape changed. Re-probe, and if
                // the answer changed, adopt it instead of repeating a request
                // that will fail the same way.
                replans = replans.saturating_add(1);
                if replans > context.config.backoff.max_attempts {
                    context.fail(current, failure);
                    return StepOutcome::Failed;
                }
                match probe_with_retry(context).await {
                    Ok(fresh) => {
                        let downgrade = !fresh.range_supported
                            || fresh.total_len.is_none()
                            || fresh.total_len != context.record.total_len;
                        context.record.range_supported = fresh.range_supported;
                        if let (Some(known), Some(again)) =
                            (context.record.total_len, fresh.total_len)
                            && known != again
                        {
                            let mismatch = Failure::new(
                                FailureKind::DataMismatch,
                                FailureScope::Probe,
                                format!("length_changed_{again}_was_{known}"),
                            )
                            .not_retryable();
                            context.fail(current, mismatch);
                            return StepOutcome::Failed;
                        }
                        context.record.total_len = context.record.total_len.or(fresh.total_len);
                        if downgrade {
                            // Multi-segment progress cannot be reused once the
                            // source stops honouring ranges: reset to one
                            // sequential segment from byte zero (§4.3, §7.3(5)).
                            if let Err(error) = reset_target(&context.record.target_path) {
                                context.fail(current, error);
                                return StepOutcome::Failed;
                            }
                            context.record.segments = layout_for(
                                context,
                                context.record.range_supported
                                    && context.record.total_len.is_some(),
                            );
                            context.report(JobReportKind::Segments {
                                segments: context.record.segments.clone(),
                            });
                            if let Ok(mut board) = context.board.lock() {
                                *board = ProgressBoard::new(&context.record.segments);
                            }
                        } else {
                            // Same shape, so the durable progress stands; retry.
                            let fresh_layout = layout_for(context, true);
                            if !same_shape(&context.record.segments, &fresh_layout) {
                                context.record.segments = fresh_layout;
                                context.report(JobReportKind::Segments {
                                    segments: context.record.segments.clone(),
                                });
                            }
                        }
                        continue;
                    }
                    Err(probe_failure) => {
                        context.fail(current, probe_failure);
                        return StepOutcome::Failed;
                    }
                }
            }
        }

        if !context
            .record
            .segments
            .iter()
            .all(SegmentState::is_complete)
        {
            // The pass claims to be finished but a segment is incomplete: the
            // layout was re-planned underneath it. Re-plan and go round again.
            let refreshed = layout_for(
                context,
                context.record.range_supported && context.record.total_len.is_some(),
            );
            context.record.segments = refreshed;
            context.set_state(current, TaskState::Queued);
            if let Ok(mut board) = context.board.lock() {
                *board = ProgressBoard::new(&context.record.segments.clone());
            }
            return StepOutcome::Restart(TaskState::Queued);
        }

        if context.config.verify {
            context.set_state(current, TaskState::Verifying);
            current = TaskState::Verifying;
            // No checksum source is configured in M1, so verification has
            // nothing to compare against and cannot fail.
        }
        context.set_state(current, TaskState::Completed);
        return StepOutcome::Finished;
    }
}

/// Adopt the length a close-delimited body turned out to have (`§4.3`).
///
/// Unknown-length mode asks for the whole representation as one open-ended
/// segment, so the segment is only known to be finished once the body ends and
/// the length it turned out to have is the length of the file. Closing the
/// interval here is what lets everything downstream — the completeness check,
/// the persisted layout, a later resume — work on a finite range. Nothing is
/// discarded: the bytes already written become the durable prefix of exactly
/// the same interval.
fn adopt_observed_length(context: &mut WorkerContext) {
    if context.record.total_len.is_some() {
        return;
    }
    let open = match context.board.lock() {
        Ok(board) => board.snapshot_segments().into_iter().next(),
        Err(_) => None,
    };
    let Some(open) = open else {
        return;
    };
    // Only the open-ended sentinel of unknown-length mode is adopted. A known
    // length is fixed for the life of the task (§I2) and is never derived from
    // what happened to arrive.
    if open.end != u64::MAX {
        return;
    }
    let total = open.durable_end.map_or(0, |end| end.saturating_add(1));
    context.record.total_len = Some(total);
    context.record.segments = if total == 0 {
        Vec::new()
    } else {
        vec![SegmentState {
            id: open.id,
            start: 0,
            end: total - 1,
            durable_end: Some(total - 1),
        }]
    };
    context.report(JobReportKind::Segments {
        segments: context.record.segments.clone(),
    });
    if let Ok(mut board) = context.board.lock() {
        *board = ProgressBoard::new(&context.record.segments);
    }
}

/// Reopen the target for the transfer.
fn open_for_transfer(path: &std::path::Path) -> Result<std::fs::File, Failure> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| crate::http::classify_io(&error, "open_target"))
}

/// Truncate the target to nothing. Used when the source stops honouring ranges
/// and the multi-segment layout has to be abandoned.
fn reset_target(path: &std::path::Path) -> Result<(), Failure> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|error| crate::http::classify_io(&error, "open_target"))?;
    file.set_len(0)
        .map_err(|error| crate::http::classify_io(&error, "reset_target"))
}

/// Periodic "make the file durable, then tell the actor" loop.
///
/// Order matters and is the whole point of §7.3: the flush reaches the
/// filesystem *before* the durable positions are reported, so a crash cannot
/// leave the store claiming bytes the file does not have.
async fn flush_loop(
    board: Arc<Mutex<ProgressBoard>>,
    file: Arc<Mutex<std::fs::File>>,
    stop: watch::Receiver<bool>,
    interval: Duration,
) {
    loop {
        tokio::select! {
            () = transfer::wait_for_stop(&stop) => return,
            () = tokio::time::sleep(interval) => {
                if *stop.borrow() {
                    return;
                }
                flush_once(&board, &file);
            }
        }
    }
}

/// Flush the file and confirm the pending bytes as durable.
fn flush_once(
    board: &Arc<Mutex<ProgressBoard>>,
    file: &Arc<Mutex<std::fs::File>>,
) -> Vec<(u32, u64)> {
    if let Ok(file) = file.lock()
        && let Err(error) = file.sync_data()
    {
        tracing::warn!(kind = ?error.kind(), "target flush failed");
    }
    transfer::mark_durable(board)
}

/// Make the worker's own view of the task agree with what is on disk, then tell
/// the actor about it.
///
/// Three things have to happen in this order and none of them may be skipped:
///
/// 1. the file is flushed, so the bytes are actually there;
/// 2. the confirmed positions are pulled *back* from the board into the worker's
///    own layout;
/// 3. the actor is told, so the store and the host's view move with them.
///
/// Step 2 is the one that is easy to leave out, and leaving it out is not a
/// small omission: the board is where a transfer records progress (it is
/// updated under the file lock by the lane that wrote each chunk) while the
/// completeness check and every re-plan read `record.segments`. Two copies of
/// one fact, and if only the board advances then every finished pass looks
/// unfinished — the worker re-plans from a layout with no durable progress,
/// throws away everything it fetched and starts again, forever.
fn settle_progress(context: &mut WorkerContext, file: &Arc<Mutex<std::fs::File>>) {
    flush_once(&context.board, file);
    sync_record_from_board(context);
    report_progress(context);
}

/// Copy the board's confirmed durable positions into the worker's layout.
///
/// Matched by segment id rather than by position: `record.segments` is the
/// authority on *which intervals exist* (invariant I1) and the board is the
/// authority on *how much of each is durable*.
fn sync_record_from_board(context: &mut WorkerContext) {
    let Ok(board) = context.board.lock() else {
        return;
    };
    let confirmed = board.snapshot_segments();
    for segment in &mut context.record.segments {
        if let Some(fresh) = confirmed
            .iter()
            .find(|candidate| candidate.id == segment.id)
        {
            segment.durable_end = fresh.durable_end;
        }
    }
}

fn report_progress(context: &WorkerContext) {
    let Ok(board) = context.board.lock() else {
        return;
    };
    let segments = board.snapshot_segments();
    let received = board.received_len();
    let complete = board.completed_segments();
    drop(board);
    context.report(JobReportKind::Progress {
        segments,
        received,
        segments_complete: complete,
    });
}

impl WorkerContext {
    fn task_rate_limiter(&self) -> Option<SharedLimiter> {
        // The limiter object is shared with the actor, which reconfigures it in
        // place. Reading it once per pass is enough for §9.1: a pass is
        // re-entered after every pause, re-plan and retry, and within a pass the
        // same limiter object is consulted for every chunk.
        self.limit_slot.get()
    }
}

/// A worker's view of its own rate limiter, kept in sync by the actor.
///
/// The limiter is replaced or reconfigured in place rather than swapped for a
/// new one, so a transfer already holding a clone keeps pacing itself correctly
/// after the host changes the ceiling.
#[derive(Debug, Default)]
pub struct LimitSlot {
    inner: Mutex<Option<SharedLimiter>>,
}

impl LimitSlot {
    /// Replace the limiter.
    pub fn set(&self, limiter: Option<SharedLimiter>) {
        if let Ok(mut slot) = self.inner.lock() {
            *slot = limiter;
        }
    }

    /// The current limiter.
    pub fn get(&self) -> Option<SharedLimiter> {
        self.inner.lock().ok().and_then(|slot| slot.clone())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn record(total: Option<u64>, range_supported: bool) -> TaskRecord {
        TaskRecord {
            id: TaskId(1),
            state: TaskState::Queued,
            source_label: "https://example.test/x".into(),
            target_path: std::path::PathBuf::from("x"),
            total_len: total,
            range_supported,
            validators: Default::default(),
            segments: Vec::new(),
            failure: None,
            name_hint: None,
            created_at: 0,
            updated_at: 0,
            rate_limit_bps: None,
        }
    }

    fn context_with(record: TaskRecord) -> WorkerContext {
        let (_control, control_rx) = JobControl::new();
        let (reports, _inbox) = mpsc::channel(8);
        WorkerContext {
            id: record.id,
            record,
            config: EngineConfig::default(),
            client: crate::http::build_client().unwrap(),
            permits: Arc::new(Semaphore::new(4)),
            reports,
            control: control_rx,
            rate_window: Arc::new(RateWindow::new(5)),
            board: Arc::new(Mutex::new(ProgressBoard::new(&[]))),
            global_limiter: None,
            limit_slot: Arc::new(LimitSlot::default()),
        }
    }
    #[test]
    fn unknown_length_degrades_to_one_open_ended_segment() {
        let context = context_with(record(None, false));
        let layout = layout_for(&context, false);
        assert_eq!(layout.len(), 1);
        assert_eq!(layout[0].start, 0);
        assert_eq!(layout[0].end, u64::MAX);
    }

    #[test]
    fn no_range_support_means_a_single_segment_whatever_the_size() {
        let context = context_with(record(Some(1024 * 1024 * 1024), false));
        let layout = layout_for(&context, false);
        assert_eq!(layout.len(), 1);
        assert_eq!(layout[0].end, 1024 * 1024 * 1024 - 1);
    }

    #[test]
    fn range_support_splits_a_large_file() {
        let context = context_with(record(Some(1024 * 1024 * 1024), true));
        let layout = layout_for(&context, true);
        assert_eq!(layout.len(), 8);
        assert_eq!(layout[0].start, 0);
        assert_eq!(layout[layout.len() - 1].end, 1024 * 1024 * 1024 - 1);
    }

    #[test]
    fn durable_progress_survives_an_identical_replan() {
        let mut existing = SegmentState::new(0, 0, 4095);
        existing.mark_durable_through(1023);
        let mut task = record(Some(4096), true);
        task.segments = vec![existing];
        let context = context_with(task);
        let layout = layout_for(&context, true);
        assert_eq!(layout.len(), 1);
        assert_eq!(layout[0].durable_end, Some(1023));
    }

    #[test]
    fn a_different_interval_does_not_inherit_progress() {
        let mut existing = SegmentState::new(0, 0, 4095);
        existing.mark_durable_through(1023);
        let mut task = record(Some(8192), true);
        task.segments = vec![existing];
        let context = context_with(task);
        let layout = layout_for(&context, true);
        // 8192 is small enough for one segment but its interval differs from
        // the recorded one only in the end byte, so this asserts the exact
        // interval match is what carries progress across.
        assert!(layout.iter().all(|segment| segment.durable_end.is_none()));
    }

    #[test]
    fn prepare_target_preallocates_and_reports_durable_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("target.bin");
        let layout = vec![SegmentState::new(0, 0, 1023)];
        let prepared = prepare_target(&path, Some(1024), &layout).unwrap();
        assert_eq!(prepared.durable_len, 0);
        assert!(!prepared.truncated_extra);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 1024);
    }

    #[test]
    fn prepare_target_truncates_to_the_confirmed_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("target.bin");
        std::fs::write(&path, vec![0xAAu8; 4096]).unwrap();
        let mut segment = SegmentState::new(0, 0, 1023);
        segment.mark_durable_through(511);
        let prepared = prepare_target(&path, Some(1024), &[segment]).unwrap();
        // The 3.5 KiB past the durable watermark is dropped, because no interval
        // can claim it (§7.3(5)); it will be fetched again.
        assert!(prepared.truncated_extra);
        assert_eq!(prepared.durable_len, 512);
        // ... and then the preallocation extends the file to the full length so
        // that any segment can be written at any offset (§6.4).
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 1024);
    }

    #[test]
    fn prepare_target_extends_an_exactly_matching_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("target.bin");
        std::fs::write(&path, vec![0xAAu8; 512]).unwrap();
        let mut segment = SegmentState::new(0, 0, 1023);
        segment.mark_durable_through(511);
        let prepared = prepare_target(&path, Some(1024), &[segment]).unwrap();
        // File and store agree exactly, so nothing is dropped and the
        // preallocation simply extends to the full length.
        assert!(!prepared.truncated_extra);
        assert_eq!(prepared.durable_len, 512);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 1024);
    }

    #[test]
    fn prepare_target_trusts_the_smaller_of_store_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("target.bin");
        std::fs::write(&path, vec![0u8; 100]).unwrap();
        let mut segment = SegmentState::new(0, 0, 999);
        segment.mark_durable_through(999);
        let prepared = prepare_target(&path, Some(1000), &[segment]).unwrap();
        // The store claimed 1000 durable bytes, the file only has 100. The
        // conservative reading is the file's.
        assert_eq!(prepared.durable_len, 100);
        assert!(!prepared.truncated_extra);
    }

    #[test]
    fn reset_target_empties_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("target.bin");
        std::fs::write(&path, vec![7u8; 512]).unwrap();
        reset_target(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
    }

    #[test]
    fn job_phase_round_trips_through_its_code() {
        for phase in [
            JobPhase::Run,
            JobPhase::Pause,
            JobPhase::Resume,
            JobPhase::Retry,
            JobPhase::Stop,
        ] {
            assert_eq!(JobPhase::from_code(phase.code()), phase);
        }
    }

    #[tokio::test]
    async fn control_handle_publishes_phase_changes() {
        let (control, mut receiver) = JobControl::new();
        assert_eq!(control.phase(), JobPhase::Run);
        control.set_phase(JobPhase::Pause);
        assert_eq!(receiver.changed().await, JobPhase::Pause);
        assert_eq!(control.phase(), JobPhase::Pause);
        control.set_phase(JobPhase::Run);
        assert_eq!(receiver.changed().await, JobPhase::Run);
    }

    #[test]
    fn task_limiter_is_replaced_and_reconfigured() {
        let (control, _receiver) = JobControl::new();
        assert!(control.task_limit().is_none());
        control.set_task_limit(Some(1024));
        let limiter = control.task_limit().unwrap();
        assert_eq!(limiter.rate(), Some(1024));
        control.set_task_limit(Some(2048));
        // The limiter object is reconfigured in place, so a transfer already
        // holding a clone sees the new ceiling (§9.1). The last requested rate
        // is remembered even while pacing is switched off.
        assert_eq!(control.task_limit().unwrap().rate(), Some(2048));
        control.set_task_limit(None);
        assert!(control.task_limit().unwrap().is_unlimited());
        assert_eq!(control.task_limit().unwrap().rate(), Some(2048));
        control.set_task_limit(Some(4096));
        assert_eq!(control.task_limit().unwrap().rate(), Some(4096));
        assert!(!control.task_limit().unwrap().is_unlimited());
    }

    #[test]
    fn limit_slot_is_shared_between_threads() {
        let slot = Arc::new(LimitSlot::default());
        slot.set(Some(crate::engine::throttle::RateLimiter::shared(Some(
            100,
        ))));
        let clone = Arc::clone(&slot);
        assert_eq!(clone.get().unwrap().rate(), Some(100));
    }

    #[test]
    fn flush_marks_pending_bytes_durable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.bin");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        let segment = SegmentState::new(0, 0, 99);
        let board = Arc::new(Mutex::new(ProgressBoard::new(&[segment])));
        {
            let mut guard = board.lock().unwrap();
            guard.extend(0, 49);
        }
        let file = Arc::new(Mutex::new(file));
        let confirmed = flush_once(&board, &file);
        assert_eq!(confirmed, vec![(0, 49)]);
        assert_eq!(board.lock().unwrap().segments()[0].durable_len(), 50);
        // A second flush has nothing new to confirm.
        assert!(flush_once(&board, &file).is_empty());
    }
}
