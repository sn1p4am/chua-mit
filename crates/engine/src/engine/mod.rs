//! The download engine: command surface, scheduler, and per-task worker.
//!
//! # Shape
//!
//! One [`EngineHandle`] owns a command channel. A single actor task drains it,
//! so every state change is serialised through one writer — which is what makes
//! invariants I1 (segments tile the file) and I3 (user data is untouched) cheap
//! to hold. Each task additionally has a worker that owns its transfer and
//! reports back through the same channel. `spec/engine-v1.md` §7, §10.
//!
//! # What the host sees
//!
//! * [`EngineHandle`] methods for commands.
//! * [`EngineHandle::subscribe`] for the event stream.
//! * [`EngineHandle::snapshot`] for full state, which is the documented answer
//!   to `broadcast` lag (ADR-0004).

pub mod job;
pub mod persist;
pub mod ranges;
pub mod split;
pub mod throttle;
pub mod transfer;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{Semaphore, broadcast, mpsc, oneshot};

use crate::config::EngineConfig;
use crate::events::{
    DEFAULT_EVENT_CAPACITY, EngineEvent, EventKind, ProgressThrottle, Snapshot, TaskView,
};
use crate::failure::{Failure, FailureKind, FailureScope};
use crate::model::{SegmentState, TaskId, TaskRecord, TaskSeed};
use crate::state::{TaskState, TransitionError, check_transition};
use crate::store::EngineSettings;

use self::job::{JobControl, JobPhase, JobReport, JobReportKind, WorkerContext};
use self::persist::Persist;
use self::throttle::{RateLimiter, RateWindow, SharedLimiter, effective_limit};

/// Errors a host command can produce. Structured only; the host owns wording.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    /// The task id is not known.
    #[error("unknown task id")]
    UnknownTask,
    /// The requested transition is not in the §3.2 table.
    #[error(transparent)]
    Transition(#[from] TransitionError),
    /// The target path already exists. §7.4 forbids the engine from deciding
    /// what to do about that, so it comes back to the host.
    #[error("target path already exists")]
    TargetExists {
        /// Path that is already occupied.
        path: PathBuf,
    },
    /// The source could not be probed.
    #[error("source rejected")]
    Source(Box<Failure>),
    /// Storage or configuration problem.
    #[error("engine unavailable")]
    Unavailable(Box<Failure>),
    /// The engine actor is gone.
    #[error("engine actor has stopped")]
    ActorGone,
}

impl EngineError {
    /// The structured failure behind this error, when there is one.
    pub fn failure(&self) -> Option<&Failure> {
        match self {
            Self::Source(failure) | Self::Unavailable(failure) => Some(failure),
            _ => None,
        }
    }

    /// Stable identifier for logs and host lookup tables.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::UnknownTask => "unknown_task",
            Self::Transition(_) => "illegal_transition",
            Self::TargetExists { .. } => "target_exists",
            Self::Source(_) => "source_failed",
            Self::Unavailable(_) => "engine_unavailable",
            Self::ActorGone => "actor_gone",
        }
    }
}

/// Command surface handed to the host.
#[derive(Clone)]
pub struct EngineHandle {
    commands: mpsc::Sender<Command>,
    events: broadcast::Sender<EngineEvent>,
    throttle: ProgressThrottle,
}

impl EngineHandle {
    /// Subscribe to the event stream.
    ///
    /// A receiver that falls behind gets `RecvError::Lagged`; the contract is
    /// that the host then calls [`EngineHandle::snapshot`] to resynchronise.
    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.events.subscribe()
    }

    /// Register a download and take it through the probe.
    ///
    /// Returns once the probe has settled, so the host learns the id, the final
    /// path and the total length — or the reason it could not be probed.
    pub async fn add(&self, seed: TaskSeed) -> Result<TaskId, EngineError> {
        let (reply, wait) = oneshot::channel();
        self.send(Command::Add {
            seed: Box::new(seed),
            reply,
        })
        .await?;
        wait.await.unwrap_or(Err(EngineError::ActorGone))
    }

    /// Full state, for rebuild-after-lag and for first paint.
    pub async fn snapshot(&self) -> Result<Snapshot, EngineError> {
        let (reply, wait) = oneshot::channel();
        self.send(Command::Snapshot { reply }).await?;
        wait.await.unwrap_or(Err(EngineError::ActorGone))
    }

    /// One task's state.
    pub async fn task(&self, id: TaskId) -> Result<TaskView, EngineError> {
        let snapshot = self.snapshot().await?;
        snapshot.get(id).cloned().ok_or(EngineError::UnknownTask)
    }

    /// Pause a `Queued` or `Downloading` task (`spec/engine-v1.md` §3.2).
    pub async fn pause(&self, id: TaskId) -> Result<TaskState, EngineError> {
        self.transition(id, TaskState::Paused).await
    }

    /// Resume a `Paused` task.
    pub async fn resume(&self, id: TaskId) -> Result<TaskState, EngineError> {
        self.transition(id, TaskState::Queued).await
    }

    /// Ask a `Failed` task to try again. This is the explicit retry that §3.1
    /// allows, and the only way out of a terminal state.
    pub async fn retry(&self, id: TaskId) -> Result<TaskState, EngineError> {
        self.transition(id, TaskState::Probing).await
    }

    /// Remove a task. Never deletes the data file (invariant I3).
    pub async fn remove(&self, id: TaskId) -> Result<TaskState, EngineError> {
        self.transition(id, TaskState::Removed).await
    }

    /// Change a task's own rate ceiling while it runs (§9.1).
    pub async fn set_task_rate_limit(
        &self,
        id: TaskId,
        bytes_per_second: Option<u64>,
    ) -> Result<(), EngineError> {
        let (reply, wait) = oneshot::channel();
        self.send(Command::SetTaskRate {
            id,
            bytes_per_second,
            reply,
        })
        .await?;
        wait.await.unwrap_or(Err(EngineError::ActorGone))
    }

    /// Change the global rate ceiling while tasks run (§9.1).
    pub async fn set_global_rate_limit(
        &self,
        bytes_per_second: Option<u64>,
    ) -> Result<(), EngineError> {
        let (reply, wait) = oneshot::channel();
        self.send(Command::SetGlobalRate {
            bytes_per_second,
            reply,
        })
        .await?;
        wait.await.unwrap_or(Err(EngineError::ActorGone))
    }

    /// The event rate ceiling this engine emits at (§9.4).
    pub fn event_rate_hz(&self) -> u32 {
        let millis = self.throttle.min_interval().as_millis().max(1);
        u32::try_from(1000 / millis).unwrap_or(1)
    }

    /// How many §6.2 splits have been performed since this engine was opened,
    /// summed over every task.
    ///
    /// A diagnostic. The layout is the thing that matters and the snapshot
    /// carries it; this exists so that a host — or a test — can say "a split
    /// happened" without having to diff two segment lists and infer it.
    pub async fn split_count(&self) -> Result<u64, EngineError> {
        let (reply, wait) = oneshot::channel();
        self.send(Command::SplitCount { reply }).await?;
        wait.await.unwrap_or(Err(EngineError::ActorGone))
    }

    /// Stop the actor, flush once, and wait for it to finish.
    ///
    /// §7.2 requires a final flush on the exit path, and equally requires not
    /// depending on it. This does not wait for running transfers to finish.
    pub async fn shutdown(self) {
        let (reply, wait) = oneshot::channel();
        if self.send(Command::Shutdown { reply }).await.is_ok() {
            // A closed reply channel means the actor finished first, which is
            // equally fine. The outcome is inspected rather than discarded.
            if wait.await.is_err() {
                tracing::debug!("actor stopped before acknowledging shutdown");
            }
        }
    }

    async fn transition(&self, id: TaskId, to: TaskState) -> Result<TaskState, EngineError> {
        let (reply, wait) = oneshot::channel();
        self.send(Command::Transition { id, to, reply }).await?;
        wait.await.unwrap_or(Err(EngineError::ActorGone))
    }

    async fn send(&self, command: Command) -> Result<(), EngineError> {
        self.commands
            .send(command)
            .await
            .map_err(|_| EngineError::ActorGone)
    }
}

pub(crate) enum Command {
    Add {
        seed: Box<TaskSeed>,
        reply: oneshot::Sender<Result<TaskId, EngineError>>,
    },
    Transition {
        id: TaskId,
        to: TaskState,
        reply: oneshot::Sender<Result<TaskState, EngineError>>,
    },
    Snapshot {
        reply: oneshot::Sender<Result<Snapshot, EngineError>>,
    },
    SetTaskRate {
        id: TaskId,
        bytes_per_second: Option<u64>,
        reply: oneshot::Sender<Result<(), EngineError>>,
    },
    SetGlobalRate {
        bytes_per_second: Option<u64>,
        reply: oneshot::Sender<Result<(), EngineError>>,
    },
    SplitCount {
        reply: oneshot::Sender<Result<u64, EngineError>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
    /// Internal: a worker changed something.
    Report(Box<JobReport>),
}

/// The engine. Owns the actor task.
pub struct Engine {
    handle: EngineHandle,
    actor: tokio::task::JoinHandle<()>,
}

impl Engine {
    /// Open the engine against a state directory.
    ///
    /// Non-terminal tasks found in the store are restored, and `Downloading`
    /// ones return to `Queued` rather than resuming in place (§7.3).
    pub async fn open(state_dir: &Path, config: EngineConfig) -> Result<Self, Failure> {
        config.validate()?;
        std::fs::create_dir_all(state_dir)
            .map_err(|error| crate::http::classify_io(&error, "create_state_dir"))?;
        let store = Persist::open(&state_dir.join("engine.db"))?;
        Self::with_store(store, config).await
    }

    /// Open with an in-memory store. Useful for tests and for a host that
    /// explicitly wants no durable record.
    pub async fn open_in_memory(config: EngineConfig) -> Result<Self, Failure> {
        config.validate()?;
        let store = Persist::open_in_memory()?;
        Self::with_store(store, config).await
    }

    async fn with_store(store: Persist, config: EngineConfig) -> Result<Self, Failure> {
        let (commands, inbox) = mpsc::channel(256);
        let (events, _) = broadcast::channel(DEFAULT_EVENT_CAPACITY);
        let handle = EngineHandle {
            commands: commands.clone(),
            events: events.clone(),
            throttle: ProgressThrottle::new(config.event_hz),
        };
        let mut actor = Actor::new(store, config, events, handle.throttle, commands)?;
        actor.restore()?;
        let actor = tokio::spawn(actor.run(inbox));
        Ok(Self { handle, actor })
    }

    /// The host-facing handle.
    pub fn handle(&self) -> EngineHandle {
        self.handle.clone()
    }

    /// Wait for the actor to stop. Normally driven by
    /// [`EngineHandle::shutdown`].
    pub async fn join(self) {
        if let Err(error) = self.actor.await {
            // A panicked actor is not something the host can act on here, but it
            // must not pass silently either.
            tracing::error!(?error, "engine actor ended abnormally");
        }
    }
}

/// Live per-task control, shared between the actor and the task's worker.
struct JobEntry {
    control: JobControl,
    rate_window: Arc<RateWindow>,
}

struct Actor {
    store: Persist,
    config: EngineConfig,
    events: broadcast::Sender<EngineEvent>,
    throttle: ProgressThrottle,
    /// Sender half of the actor's own inbox, handed to each worker so reports
    /// arrive through the same channel as host commands.
    reports: mpsc::Sender<Command>,
    records: HashMap<TaskId, TaskRecord>,
    progress: HashMap<TaskId, Arc<Mutex<transfer::ProgressBoard>>>,
    jobs: HashMap<TaskId, JobEntry>,
    workers: HashMap<TaskId, tokio::task::JoinHandle<()>>,
    permits: Arc<Semaphore>,
    client: reqwest::Client,
    global_limiter: Option<SharedLimiter>,
    settings: EngineSettings,
    last_event: HashMap<TaskId, Instant>,
    last_flush: HashMap<TaskId, Instant>,
    /// How many §6.2 splits each task has performed. Live rather than persisted:
    /// it is a diagnostic, and the layout it produced is what actually matters.
    split_count: HashMap<TaskId, u32>,
    /// Set once a shutdown has been requested, so late worker reports are
    /// ignored rather than persisted.
    closing: bool,
}

impl Actor {
    fn new(
        store: Persist,
        config: EngineConfig,
        events: broadcast::Sender<EngineEvent>,
        throttle: ProgressThrottle,
        reports: mpsc::Sender<Command>,
    ) -> Result<Self, Failure> {
        let client = crate::http::build_client()?;
        let settings = store.load_settings()?;
        let global_limiter = settings
            .global_rate_limit_bps
            .map(|rate| RateLimiter::shared(Some(rate)));
        let concurrency = config.global_concurrency;
        Ok(Self {
            store,
            config,
            events,
            throttle,
            reports,
            records: HashMap::new(),
            progress: HashMap::new(),
            jobs: HashMap::new(),
            workers: HashMap::new(),
            permits: Arc::new(Semaphore::new(concurrency)),
            client,
            global_limiter,
            settings,
            last_event: HashMap::new(),
            last_flush: HashMap::new(),
            split_count: HashMap::new(),
            closing: false,
        })
    }

    /// Load persisted tasks and normalise the states §7.3 names.
    fn restore(&mut self) -> Result<(), Failure> {
        let loaded = self.store.load_all()?;
        for mut record in loaded {
            let original = record.state;
            let normalised = match record.state {
                // §7.3(2): a task that was mid-transfer comes back as Queued,
                // because its concurrency slot has to be reallocated.
                TaskState::Downloading | TaskState::Probing | TaskState::Verifying => {
                    TaskState::Queued
                }
                // A task that was killed between registration and the end of its
                // probe never left `Created`, and the table has no direct edge
                // from there to `Queued`. It comes back as `Probing`, which is
                // both a legal edge and the honest description of what it was
                // doing; the worker walks it on to `Queued` itself. Leaving it
                // in `Created` would be worse than untidy — it is not a state a
                // worker can be started from.
                TaskState::Created => TaskState::Probing,
                other => other,
            };
            if normalised != original {
                if let Err(error) = check_transition(original, normalised) {
                    tracing::warn!(
                        task = %record.id,
                        detail = error.tag(),
                        "restore skipped an illegal normalisation"
                    );
                    continue;
                }
                record.state = normalised;
                record.updated_at = crate::http::now_unix();
                self.store.save_task(&record)?;
            }
            self.progress.insert(
                record.id,
                Arc::new(Mutex::new(transfer::ProgressBoard::new(&record.segments))),
            );
            self.records.insert(record.id, record);
        }

        // §7.3(1): non-terminal tasks are restored. Restoring the *record* is
        // not the same as restarting the *work*, and a host that only wants to
        // read state must not find itself transferring bytes, so the two are
        // separated by `auto_resume` and the host decides which one it is
        // asking for. `Paused` is deliberately excluded: §3.1 makes it
        // persistent across restarts, and only an explicit resume may lift it.
        if self.config.auto_resume {
            let resumable: Vec<TaskId> = self
                .records
                .values()
                .filter(|record| matches!(record.state, TaskState::Queued | TaskState::Probing))
                .map(|record| record.id)
                .collect();
            for id in resumable {
                self.start_worker(id);
            }
        }
        Ok(())
    }

    async fn run(mut self, mut inbox: mpsc::Receiver<Command>) {
        while let Some(command) = inbox.recv().await {
            let closing = matches!(command, Command::Shutdown { .. });
            self.handle(command).await;
            if closing {
                break;
            }
        }
        // The host may simply drop the handle. Stop the workers either way so
        // that no transfer outlives the actor that owns its state.
        self.stop_workers();
    }

    async fn handle(&mut self, command: Command) {
        match command {
            Command::Add { seed, reply } => {
                let outcome = self.add(*seed).await;
                // A dropped reply receiver means the host went away mid-call;
                // the value is returned by `send` and logged, not ignored.
                if reply.send(outcome).is_err() {
                    tracing::debug!("add reply dropped before delivery");
                }
            }
            Command::Transition { id, to, reply } => {
                let outcome = self.transition(id, to);
                if reply.send(outcome).is_err() {
                    tracing::debug!("transition reply dropped before delivery");
                }
            }
            Command::Snapshot { reply } => {
                if reply.send(Ok(self.snapshot())).is_err() {
                    tracing::debug!("snapshot reply dropped before delivery");
                }
            }
            Command::SetTaskRate {
                id,
                bytes_per_second,
                reply,
            } => {
                let outcome = self.set_task_rate(id, bytes_per_second);
                if reply.send(outcome).is_err() {
                    tracing::debug!("rate reply dropped before delivery");
                }
            }
            Command::SetGlobalRate {
                bytes_per_second,
                reply,
            } => {
                let outcome = self.set_global_rate(bytes_per_second);
                if reply.send(outcome).is_err() {
                    tracing::debug!("global rate reply dropped before delivery");
                }
            }
            Command::Report(report) => self.apply_report(*report),
            Command::SplitCount { reply } => {
                let total: u64 = self
                    .split_count
                    .values()
                    .map(|count| u64::from(*count))
                    .sum();
                if reply.send(Ok(total)).is_err() {
                    tracing::debug!("split count reply dropped before delivery");
                }
            }
            Command::Shutdown { reply } => {
                self.closing = true;
                self.shutdown().await;
                if reply.send(()).is_err() {
                    tracing::debug!("shutdown reply dropped before delivery");
                }
            }
        }
    }

    /// Register a task and drive its probe, returning as soon as the shape of
    /// the download is known.
    async fn add(&mut self, seed: TaskSeed) -> Result<TaskId, EngineError> {
        let url = reqwest::Url::parse(&seed.source_label).map_err(|_| {
            EngineError::Source(Box::new(
                Failure::new(
                    FailureKind::Unknown,
                    FailureScope::Probe,
                    "source_not_a_url",
                )
                .not_retryable(),
            ))
        })?;

        let id = self
            .store
            .next_task_id()
            .map_err(|failure| EngineError::Unavailable(Box::new(failure)))?;
        let now = crate::http::now_unix();
        let record = TaskRecord {
            id,
            state: TaskState::Created,
            source_label: seed.source_label.clone(),
            // Provisional: the probe decides the real path.
            target_path: seed.dest_dir.clone(),
            total_len: None,
            range_supported: false,
            validators: Default::default(),
            segments: Vec::new(),
            failure: None,
            name_hint: seed.name_hint.clone(),
            created_at: now,
            updated_at: now,
            rate_limit_bps: seed.rate_limit_bps,
        };
        self.store
            .save_task(&record)
            .map_err(|failure| EngineError::Unavailable(Box::new(failure)))?;
        self.records.insert(id, record.clone());
        self.emit(EngineEvent::task(EventKind::TaskAdded, self.view(&record)));

        // Registration is immediately followed by the probe, and §3.2 has no
        // `Created -> Queued` edge: the task has to be walked through `Probing`
        // rather than teleported past it. Doing it here also means the state a
        // crash during the probe leaves behind (`Probing`) is the truth.
        self.apply_state(id, TaskState::Probing, None)?;

        let probe = match Self::probe_with_retry(&self.client, &self.config, &url).await {
            Ok(probe) => probe,
            Err(failure) => {
                self.fail_record(id, failure.clone());
                return Err(EngineError::Source(Box::new(failure)));
            }
        };

        let target_path = match seed.explicit_target.clone() {
            Some(path) => path,
            None => {
                let derived = crate::filename::derive(&crate::filename::NameSources {
                    content_disposition: probe.content_disposition.as_deref(),
                    final_url: &probe.final_url,
                    host_hint: seed.name_hint.as_deref(),
                });
                seed.dest_dir.join(derived.into_name().as_str())
            }
        };

        if target_path.exists() {
            // §7.4: the engine may not decide this. Report and stop.
            let failure =
                Failure::new(FailureKind::Unknown, FailureScope::Storage, "target_exists")
                    .not_retryable();
            self.fail_record(id, failure);
            return Err(EngineError::TargetExists { path: target_path });
        }

        let Some(record) = self.records.get_mut(&id) else {
            return Err(EngineError::UnknownTask);
        };
        record.target_path = target_path;
        record.total_len = probe.total_len;
        record.range_supported = probe.range_supported;
        record.validators = probe.validators;
        record.updated_at = crate::http::now_unix();

        self.apply_state(id, TaskState::Queued, None)?;
        self.persist(id);
        self.start_worker(id);
        Ok(id)
    }

    /// Probe with the §8.3 backoff, and §4.4's total time limit per attempt.
    async fn probe_with_retry(
        client: &reqwest::Client,
        config: &EngineConfig,
        url: &reqwest::Url,
    ) -> Result<crate::http::ProbeOutcome, Failure> {
        let mut attempt = 1u32;
        loop {
            match crate::http::probe(client, url, config.probe_timeout).await {
                Ok(outcome) => return Ok(outcome),
                Err(failure) => {
                    if !failure.retryable {
                        return Err(failure);
                    }
                    let jitter = pseudo_random_unit(url.as_str(), attempt);
                    match config
                        .backoff
                        .next_delay(attempt, failure.retry_after, jitter)
                    {
                        Some(delay) => {
                            tracing::debug!(
                                attempt,
                                detail = %failure.detail,
                                delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                                "retrying probe"
                            );
                            tokio::time::sleep(delay).await;
                            attempt = attempt.saturating_add(1);
                        }
                        None => return Err(failure),
                    }
                }
            }
        }
    }

    fn snapshot(&self) -> Snapshot {
        let mut tasks: Vec<TaskView> = self
            .records
            .values()
            .map(|record| self.view(record))
            .collect();
        tasks.sort_by_key(|task| task.id);
        Snapshot { tasks }
    }

    fn view(&self, record: &TaskRecord) -> TaskView {
        // The view folds durable and pending together to report `received_len`,
        // which is what a progress indicator needs. `done_len` stays separate
        // and only ever counts what the store has confirmed.
        let in_flight = |segment: u32| self.in_flight_for(record.id, segment);
        let rate = self
            .jobs
            .get(&record.id)
            .map(|entry| entry.rate_window.last_rate_bps())
            .filter(|rate| *rate > 0);
        TaskView::assemble(record, &in_flight, rate)
    }

    /// Bytes belonging to one segment that are on disk: the durable prefix plus
    /// whatever has been written since the last flush.
    fn in_flight_for(&self, task: TaskId, segment: u32) -> u64 {
        let Some(record) = self.records.get(&task) else {
            return 0;
        };
        let durable = record
            .segments
            .iter()
            .find(|candidate| candidate.id == segment)
            .map_or(0, SegmentState::durable_len);
        let pending = self
            .progress
            .get(&task)
            .and_then(|board| board.lock().ok().map(|guard| guard.pending_for(segment)))
            .unwrap_or(0);
        durable.saturating_add(pending)
    }

    fn transition(&mut self, id: TaskId, to: TaskState) -> Result<TaskState, EngineError> {
        let Some(record) = self.records.get(&id) else {
            return Err(EngineError::UnknownTask);
        };
        let from = record.state;
        check_transition(from, to)?;
        self.apply_state(id, to, None)?;
        self.persist(id);

        match to {
            TaskState::Paused => {
                if let Some(entry) = self.jobs.get(&id) {
                    entry.control.set_phase(JobPhase::Pause);
                    entry.rate_window.reset();
                }
            }
            TaskState::Queued => {
                if from == TaskState::Paused {
                    if let Some(entry) = self.jobs.get(&id) {
                        entry.control.set_phase(JobPhase::Run);
                    }
                    self.start_worker(id);
                } else if let Some(entry) = self.jobs.get(&id) {
                    // `Downloading -> Queued`: park and reacquire a slot.
                    entry.control.set_phase(JobPhase::Resume);
                }
            }
            TaskState::Probing => {
                if let Some(entry) = self.jobs.get(&id) {
                    entry.control.set_phase(JobPhase::Retry);
                }
                self.start_worker(id);
            }
            TaskState::Removed => {
                if let Some(entry) = self.jobs.get(&id) {
                    entry.control.set_phase(JobPhase::Stop);
                }
                let view = self.view(&self.records[&id]);
                self.emit(EngineEvent::task_with_detail(
                    EventKind::TaskRemoved,
                    view,
                    "removed",
                ));
            }
            _ => {}
        }
        Ok(to)
    }

    fn set_task_rate(
        &mut self,
        id: TaskId,
        bytes_per_second: Option<u64>,
    ) -> Result<(), EngineError> {
        let Some(record) = self.records.get_mut(&id) else {
            return Err(EngineError::UnknownTask);
        };
        record.rate_limit_bps = bytes_per_second;
        record.updated_at = crate::http::now_unix();
        let record = record.clone();
        self.store
            .save_task(&record)
            .map_err(|failure| EngineError::Unavailable(Box::new(failure)))?;
        if let Some(entry) = self.jobs.get(&id) {
            entry.control.set_task_limit(effective_limit(
                self.settings.global_rate_limit_bps,
                bytes_per_second,
            ));
        }
        Ok(())
    }

    fn set_global_rate(&mut self, bytes_per_second: Option<u64>) -> Result<(), EngineError> {
        self.settings.global_rate_limit_bps = bytes_per_second;
        self.store
            .save_settings(&self.settings)
            .map_err(|failure| EngineError::Unavailable(Box::new(failure)))?;
        match (&self.global_limiter, bytes_per_second) {
            (Some(limiter), Some(rate)) => limiter.set_rate(Some(rate)),
            (Some(limiter), None) => limiter.set_rate(None),
            (None, Some(rate)) => {
                self.global_limiter = Some(RateLimiter::shared(Some(rate)));
            }
            (None, None) => {}
        }
        let per_task: Vec<(TaskId, Option<u64>)> = self
            .records
            .iter()
            .map(|(id, record)| (*id, record.rate_limit_bps))
            .collect();
        for (id, task_limit) in per_task {
            if let Some(entry) = self.jobs.get(&id) {
                entry
                    .control
                    .set_task_limit(effective_limit(bytes_per_second, task_limit));
            }
        }
        Ok(())
    }

    fn apply_state(
        &mut self,
        id: TaskId,
        to: TaskState,
        failure: Option<Failure>,
    ) -> Result<(), EngineError> {
        let Some(record) = self.records.get_mut(&id) else {
            return Err(EngineError::UnknownTask);
        };
        let from = record.state;
        check_transition(from, to)?;
        record.state = to;
        record.failure = failure;
        record.updated_at = crate::http::now_unix();
        let record = record.clone();
        let view = self.view(&record);
        let kind = if to == TaskState::Failed {
            EventKind::Failed
        } else {
            EventKind::StateChanged
        };
        self.emit(EngineEvent::task_with_detail(
            kind,
            view,
            format!("{}>{}", from.as_str(), to.as_str()),
        ));
        Ok(())
    }

    /// Record a failure that happened before the worker existed.
    fn fail_record(&mut self, id: TaskId, failure: Failure) {
        // The record is still `Created` at this point, and the transition table
        // has no `Created -> Failed` edge, so step through `Probing` rather than
        // skipping a state.
        if self.records.get(&id).map(|record| record.state) == Some(TaskState::Created)
            && let Err(error) = self.apply_state(id, TaskState::Probing, None)
        {
            tracing::warn!(detail = error.tag(), "could not enter Probing to fail");
            return;
        }
        if let Err(error) = self.apply_state(id, TaskState::Failed, Some(failure)) {
            tracing::warn!(detail = error.tag(), "could not record task failure");
            return;
        }
        self.persist(id);
    }

    fn start_worker(&mut self, id: TaskId) {
        if self.workers.contains_key(&id) {
            return;
        }
        let Some(record) = self.records.get(&id) else {
            return;
        };
        let (control, control_rx) = JobControl::new();
        control.set_task_limit(effective_limit(
            self.settings.global_rate_limit_bps,
            record.rate_limit_bps,
        ));
        let limit_slot = control.limit_slot();
        let rate_window = Arc::new(RateWindow::new(self.config.rate_window_secs));
        let board = self
            .progress
            .entry(id)
            .or_insert_with(|| Arc::new(Mutex::new(transfer::ProgressBoard::new(&[]))))
            .clone();

        let context = WorkerContext {
            id,
            record: record.clone(),
            config: self.config.clone(),
            client: self.client.clone(),
            permits: Arc::clone(&self.permits),
            reports: self.reports.clone(),
            control: control_rx,
            rate_window: Arc::clone(&rate_window),
            board,
            global_limiter: self.global_limiter.clone(),
            limit_slot,
        };
        self.jobs.insert(
            id,
            JobEntry {
                control,
                rate_window,
            },
        );
        let handle = tokio::spawn(job::run(context));
        self.workers.insert(id, handle);
    }

    fn apply_report(&mut self, report: JobReport) {
        if self.closing {
            return;
        }
        let id = report.task;
        match report.kind {
            JobReportKind::State { from, to, failure } => {
                if self.records.get(&id).map(|record| record.state) == Some(to) {
                    return;
                }
                if let Some(record) = self.records.get(&id)
                    && record.state != from
                {
                    tracing::debug!(
                        task = %id,
                        "ignoring stale state report {}>{}",
                        from.as_str(),
                        to.as_str()
                    );
                    return;
                }
                if let Err(error) = self.apply_state(id, to, failure) {
                    tracing::warn!(task = %id, detail = error.tag(), "worker state report refused");
                    return;
                }
                self.persist(id);
                if to.is_terminal()
                    && let Some(entry) = self.jobs.get(&id)
                {
                    entry.control.set_phase(JobPhase::Stop);
                }
            }
            JobReportKind::Segments { segments } => {
                {
                    let Some(record) = self.records.get_mut(&id) else {
                        return;
                    };
                    record.segments = segments;
                    record.updated_at = crate::http::now_unix();
                    let fresh = record.segments.clone();
                    if let Some(board) = self.progress.get(&id)
                        && let Ok(mut board) = board.lock()
                    {
                        *board = transfer::ProgressBoard::new(&fresh);
                    }
                }
                let record = self.records[&id].clone();
                let view = self.view(&record);
                self.emit(EngineEvent::task(EventKind::SegmentsChanged, view));
                self.persist(id);
            }
            JobReportKind::Split {
                segments,
                front_id,
                back_start,
                back_end,
                reason,
            } => {
                {
                    let Some(record) = self.records.get_mut(&id) else {
                        return;
                    };
                    // The board is not re-created here: a split has already been
                    // applied to it, under its own lock, and rebuilding the
                    // board from the record would throw away the pending counts
                    // the lanes are still using.
                    record.segments = segments;
                    record.updated_at = crate::http::now_unix();
                    self.split_count.insert(id, self.split_count.get(&id).copied().unwrap_or(0) + 1);
                }
                let record = self.records[&id].clone();
                let view = self.view(&record);
                // The detail is machine-readable and carries what the host would
                // otherwise have to diff two segment lists to find out.
                let detail = format!("split:{reason}:front={front_id}:back={back_start}-{back_end}");
                self.emit(EngineEvent::task_with_detail(
                    EventKind::SegmentsChanged,
                    view,
                    detail,
                ));
                self.persist(id);
            }
            JobReportKind::Progress {
                segments, received, ..
            } => {
                {
                    let Some(record) = self.records.get_mut(&id) else {
                        return;
                    };
                    record.updated_at = crate::http::now_unix();
                    record.segments = segments;
                }
                if let Some(entry) = self.jobs.get(&id) {
                    entry.rate_window.record(received, Instant::now());
                }
                let flush = self.should_flush(id);
                let emit = self.should_emit(id);
                if flush {
                    self.persist(id);
                }
                if emit {
                    let record = self.records[&id].clone();
                    let view = self.view(&record);
                    self.emit(EngineEvent::task(EventKind::Progress, view));
                }
            }
            JobReportKind::Done => {
                self.workers.remove(&id);
                self.jobs.remove(&id);
            }
        }
    }

    fn should_flush(&mut self, id: TaskId) -> bool {
        let interval = self.config.progress_flush_interval;
        let now = Instant::now();
        match self.last_flush.get(&id) {
            Some(last) if now.duration_since(*last) < interval => false,
            _ => {
                self.last_flush.insert(id, now);
                true
            }
        }
    }

    fn should_emit(&mut self, id: TaskId) -> bool {
        let now = Instant::now();
        let last = self.last_event.get(&id).copied();
        if self.throttle.allows(last, now) {
            self.last_event.insert(id, now);
            true
        } else {
            false
        }
    }

    fn persist(&mut self, id: TaskId) {
        let Some(record) = self.records.get(&id) else {
            return;
        };
        let record = record.clone();
        if let Err(failure) = self.store.save_task(&record) {
            tracing::error!(task = %id, detail = %failure.detail, "state flush failed");
        }
    }

    /// Non-blocking by construction: `broadcast::Sender::send` never awaits. A
    /// full ring drops the oldest event, which is exactly the signal the host
    /// uses to re-read the snapshot.
    fn emit(&self, event: EngineEvent) {
        if let Err(error) = self.events.send(event) {
            // `SendError` means there are no receivers, which is normal when the
            // host has not subscribed.
            tracing::trace!(kind = error.0.kind.as_str(), "event had no receivers");
        }
    }

    async fn shutdown(&mut self) {
        for entry in self.jobs.values() {
            entry.control.set_phase(JobPhase::Stop);
        }
        // Give running transfers a moment to notice and flush, then persist
        // whatever the actor already knows. §7.2 requires this final flush and
        // equally requires not depending on it.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let ids: Vec<TaskId> = self.records.keys().copied().collect();
        for id in ids {
            self.persist(id);
        }
        self.stop_workers();
    }

    fn stop_workers(&mut self) {
        for (_, handle) in self.workers.drain() {
            handle.abort();
        }
        self.jobs.clear();
    }
}

/// A cheap deterministic sample in `[0, 1)` derived from a string and an
/// attempt number. Backoff jitter only needs to decorrelate concurrent tasks,
/// so a hash is enough and it keeps the dependency surface unchanged.
fn pseudo_random_unit(seed: &str, attempt: u32) -> f64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    seed.hash(&mut hasher);
    attempt.hash(&mut hasher);
    crate::http::now_unix().hash(&mut hasher);
    let mixed = hasher.finish();
    // Take the top 53 bits, which is exactly what an f64 can represent.
    ((mixed >> 11) as f64) / ((1u64 << 53) as f64)
}

/// Whether two segment layouts describe the same set of intervals.
///
/// Durable positions are deliberately ignored: this answers "same shape", which
/// is what a re-plan check needs.
pub fn same_shape(left: &[SegmentState], right: &[SegmentState]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right.iter())
            .all(|(a, b)| a.start == b.start && a.end == b.end)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn same_shape_compares_intervals_only() {
        let mut left = vec![SegmentState::new(0, 0, 99), SegmentState::new(1, 100, 199)];
        let right = vec![SegmentState::new(0, 0, 99), SegmentState::new(1, 100, 199)];
        assert!(same_shape(&left, &right));
        left[0].mark_durable_through(50);
        assert!(same_shape(&left, &right));
        assert!(!same_shape(&left, &right[..1]));
        assert!(!same_shape(
            &left,
            &[SegmentState::new(0, 0, 98), SegmentState::new(1, 100, 199)]
        ));
    }

    #[test]
    fn jitter_samples_are_in_range_and_varied() {
        let mut samples = Vec::new();
        for attempt in 1..40u32 {
            let sample = pseudo_random_unit("https://example.test/a", attempt);
            assert!((0.0..1.0).contains(&sample), "sample {sample}");
            samples.push(sample);
        }
        let first = samples[0];
        assert!(samples.iter().any(|sample| *sample != first));
    }

    #[test]
    fn error_tags_are_stable() {
        assert_eq!(EngineError::UnknownTask.tag(), "unknown_task");
        assert_eq!(
            EngineError::from(TransitionError::UnknownTask).tag(),
            "illegal_transition"
        );
        assert_eq!(
            EngineError::TargetExists {
                path: PathBuf::from("x")
            }
            .tag(),
            "target_exists"
        );
        let source = EngineError::Source(Box::new(Failure::new(
            FailureKind::SourceMissing,
            FailureScope::Probe,
            "http_404",
        )));
        assert_eq!(source.tag(), "source_failed");
        assert!(source.failure().is_some());
    }
}
