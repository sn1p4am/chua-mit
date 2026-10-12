//! The host half: where state lives, how the engine is configured, and the loop
//! that keeps this process alive while a transfer runs.
//!
//! This binary is a *host*, not a daemon. Each invocation opens the engine,
//! does one thing, and exits — so a command that starts or resumes work has to
//! stay in the foreground until the work reaches a terminal state, because this
//! process is what drives it. `--detach` is the exception, and its help text
//! says exactly what it does and does not do.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chua_engine::config::HARD_MAX_SEGMENTS;
use chua_engine::{
    Engine, EngineConfig, EngineError, EngineEvent, EngineHandle, TaskId, TaskSeed, TaskState,
    TaskView,
};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

use crate::cli::{Command, parse_limit};
use crate::error::{CliError, Code};
use crate::render;

/// How often the wait loop re-reads the full snapshot.
///
/// The event stream is the primary signal; this is the floor under it, so that
/// a missed or dropped event can never leave the host waiting for something
/// that already happened.
const TICK: Duration = Duration::from_millis(100);

/// How often the foreground progress line is redrawn. §4 caps progress output
/// at 10 Hz and this is deliberately well under it: a terminal redraw is not
/// worth a millisecond of a transfer's time.
const REDRAW: Duration = Duration::from_millis(500);

/// Run one command.
pub async fn run(command: Command) -> Result<Code, CliError> {
    match command {
        Command::Add {
            url,
            dest,
            detach,
            segments,
            limit,
        } => add(url, dest, detach, segments, limit).await,
        Command::List => list().await,
        Command::Status { id } => status(TaskId(id)).await,
        Command::Pause { id } => pause(TaskId(id)).await,
        Command::Resume { id, all } => resume(id.map(TaskId), all).await,
        Command::Remove { id, force } => remove(TaskId(id), force).await,
    }
}

/// Install a log subscriber if `CHUA_LOG` asks for one.
///
/// Off by default: the CLI's stdout is a contract and its diagnostics belong on
/// stderr, where they are only useful when somebody has actually asked for them.
pub fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("CHUA_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    if tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init()
        .is_err()
    {
        // A subscriber is already installed, which only happens under `cargo
        // test`, where one may already exist.
    }
}

/// The engine configuration this host uses.
///
/// `auto_resume` is the host's decision and it differs by command: a read-only
/// command must not start transferring bytes merely because somebody asked what
/// the state was, while `resume` exists precisely to restart the work.
fn host_config(auto_resume: bool) -> EngineConfig {
    EngineConfig {
        auto_resume,
        ..EngineConfig::default()
    }
}

/// Where persistent state lives.
///
/// `CHUA_DATA_DIR` wins when it is set, which is how the acceptance suite gives
/// every scenario a clean, isolated store (`spec/cli-v1.md` §5).
pub fn data_dir() -> PathBuf {
    if let Some(value) = std::env::var_os("CHUA_DATA_DIR")
        && !value.is_empty()
    {
        return PathBuf::from(value);
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return PathBuf::from(appdata).join("chua");
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(xdg).join("chua");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("chua");
    }
    std::env::temp_dir().join("chua")
}

async fn open_engine(config: EngineConfig) -> Result<Engine, CliError> {
    let dir = data_dir();
    Engine::open(&dir, config)
        .await
        .map_err(|failure| CliError::failure(&failure, "cannot open the state directory"))
}

// ---------------------------------------------------------------------------
// add
// ---------------------------------------------------------------------------

async fn add(
    url: String,
    dest: Option<PathBuf>,
    detach: bool,
    segments: Option<u32>,
    limit: Option<String>,
) -> Result<Code, CliError> {
    let rate = parse_limit(limit.as_deref())?;
    let mut config = host_config(false);
    if let Some(count) = segments {
        // §6.1: a segment count above the hard limit is refused rather than
        // silently truncated, and refusing it is a usage error, not a task one.
        if count == 0 || count > HARD_MAX_SEGMENTS {
            return Err(CliError::usage(format!(
                "--segments must be in 1..={HARD_MAX_SEGMENTS}, got {count}"
            )));
        }
        config.segment_policy.max_segments = count;
    }
    if let Err(failure) = config.validate() {
        return Err(CliError::usage(format!(
            "invalid configuration: {}",
            render::failure(&failure)
        )));
    }

    let engine = open_engine(config).await?;
    let handle = engine.handle();
    // Subscribing before the task exists is what makes the wait below able to
    // see every transition, including the first one.
    let mut events = handle.subscribe();
    let mut seed = seed_for(&url, dest.as_deref())?;
    seed.rate_limit_bps = rate;

    let id = match handle.add(seed).await {
        Ok(id) => id,
        // §7.4 leaves the decision to the host; M1's decision is to refuse.
        Err(EngineError::TargetExists { path }) => {
            return Err(CliError::conflict(format!(
                "target already exists, not overwriting: {}",
                path.display()
            )));
        }
        Err(error) => return Err(engine_error(error, "cannot create the task")),
    };

    let target = handle
        .task(id)
        .await
        .map(|view| view.target_path)
        .unwrap_or_default();
    emit(&format!("task {id} created: {target}\n"));

    if detach {
        emit(
            "registered, not waiting (--detach).\n\
             note: this process is what drives the transfer, so run \
             `chua-cli resume --all` to continue it.\n",
        );
        handle.shutdown().await;
        return Ok(Code::Success);
    }

    let views = drive(&handle, &mut events, &[id]).await?;
    handle.shutdown().await;
    finish(&views)
}

/// Build the seed for `add`.
///
/// The destination is made absolute before it is handed over: the store keeps
/// the path for the life of the task, and a later `resume` runs from whatever
/// directory the user happens to be in at the time.
fn seed_for(url: &str, dest: Option<&Path>) -> Result<TaskSeed, CliError> {
    let cwd = std::env::current_dir().map_err(|error| {
        CliError::usage(format!("cannot determine the working directory: {error}"))
    })?;
    let Some(dest) = dest else {
        // No destination: the engine derives a name from the response and the
        // final URL, and it lands in the current directory.
        return Ok(TaskSeed::new(url, cwd));
    };
    let absolute = if dest.is_absolute() {
        dest.to_path_buf()
    } else {
        cwd.join(dest)
    };
    let parent = absolute
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cwd.clone());
    let mut seed = TaskSeed::new(url, parent);
    seed.name_hint = absolute
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    seed.explicit_target = Some(absolute);
    Ok(seed)
}

// ---------------------------------------------------------------------------
// list / status
// ---------------------------------------------------------------------------

async fn list() -> Result<Code, CliError> {
    let engine = open_engine(host_config(false)).await?;
    let handle = engine.handle();
    let snapshot = handle
        .snapshot()
        .await
        .map_err(|error| engine_error(error, "cannot read the task list"))?;
    for view in &snapshot.tasks {
        emit(&format!("{}\n", render::list_line(view)));
    }
    Ok(Code::Success)
}

async fn status(id: TaskId) -> Result<Code, CliError> {
    let engine = open_engine(host_config(false)).await?;
    let handle = engine.handle();
    let snapshot = handle
        .snapshot()
        .await
        .map_err(|error| engine_error(error, "cannot read the task"))?;
    let Some(view) = snapshot.get(id) else {
        return Err(CliError::not_found(id));
    };
    emit(&render::status_block(view));
    Ok(Code::Success)
}

// ---------------------------------------------------------------------------
// pause / resume / remove
// ---------------------------------------------------------------------------

async fn pause(id: TaskId) -> Result<Code, CliError> {
    let engine = open_engine(host_config(false)).await?;
    let handle = engine.handle();
    match handle.pause(id).await {
        Ok(state) => {
            emit(&format!("task {id} paused ({state})\n"));
            handle.shutdown().await;
            Ok(Code::Success)
        }
        Err(EngineError::UnknownTask) => Err(CliError::not_found(id)),
        Err(EngineError::Transition(error)) => Err(CliError::task(format!(
            "task {id} cannot be paused from its current state: {error}"
        ))),
        Err(error) => Err(engine_error(error, "cannot pause the task")),
    }
}

async fn resume(only: Option<TaskId>, all: bool) -> Result<Code, CliError> {
    match (only, all) {
        (Some(_), true) => Err(CliError::usage(
            "resume takes one of: `resume <id>` or `resume --all`",
        )),
        (None, false) => Err(CliError::usage("resume needs <id> or --all")),
        (Some(id), false) => resume_tasks(Some(id)).await,
        (None, true) => resume_tasks(None).await,
    }
}

/// Resume work, and stay in the foreground until it is done.
///
/// The engine restores non-terminal tasks when it opens (§7.3) and, with
/// `auto_resume`, puts them back to work. What it will not do is lift `Paused`,
/// because §3.1 makes that state survive a restart — so an explicit resume is
/// what does it.
async fn resume_tasks(only: Option<TaskId>) -> Result<Code, CliError> {
    let engine = open_engine(host_config(true)).await?;
    let handle = engine.handle();
    let mut events = handle.subscribe();
    let snapshot = handle
        .snapshot()
        .await
        .map_err(|error| engine_error(error, "cannot read the task list"))?;

    let selected: Vec<TaskId> = match only {
        Some(id) => {
            if snapshot.get(id).is_none() {
                return Err(CliError::not_found(id));
            }
            vec![id]
        }
        None => snapshot
            .tasks
            .iter()
            .filter(|view| !view.state.is_terminal())
            .map(|view| view.id)
            .collect(),
    };

    if selected.is_empty() {
        emit("nothing to resume\n");
        handle.shutdown().await;
        return Ok(Code::Success);
    }

    for id in &selected {
        if snapshot.get(*id).map(|view| view.state) == Some(TaskState::Paused)
            && let Err(error) = handle.resume(*id).await
        {
            return Err(engine_error(error, "cannot resume the task"));
        }
    }

    let views = drive(&handle, &mut events, &selected).await?;
    handle.shutdown().await;
    finish(&views)
}

async fn remove(id: TaskId, force: bool) -> Result<Code, CliError> {
    let engine = open_engine(host_config(false)).await?;
    let handle = engine.handle();
    let snapshot = handle
        .snapshot()
        .await
        .map_err(|error| engine_error(error, "cannot read the task"))?;
    let Some(view) = snapshot.get(id) else {
        return Err(CliError::not_found(id));
    };
    if !force && has_unfinished_data(view) {
        return Err(CliError::usage(format!(
            "task {id} has unfinished data ({}); removing it needs --force",
            render::progress(view)
        )));
    }
    let target = view.target_path.clone();
    match handle.remove(id).await {
        Ok(state) => {
            // Invariant I3: removing a task never deletes the file it was
            // writing. Say so plainly, because the alternative is a user
            // assuming that it did.
            emit(&format!(
                "task {id} removed ({state}); the data file was left in place: {target}\n"
            ));
            handle.shutdown().await;
            Ok(Code::Success)
        }
        Err(EngineError::UnknownTask) => Err(CliError::not_found(id)),
        Err(EngineError::Transition(error)) => Err(CliError::task(format!(
            "task {id} cannot be removed from its current state: {error}"
        ))),
        Err(error) => Err(engine_error(error, "cannot remove the task")),
    }
}

/// Whether removing this task would strand bytes that were fetched for it.
fn has_unfinished_data(view: &TaskView) -> bool {
    match view.total_len {
        Some(total) => view.done_len < total,
        // Unknown length: any byte on disk at all is unfinished data.
        None => view.received_len > 0,
    }
}

// ---------------------------------------------------------------------------
// Waiting
// ---------------------------------------------------------------------------

/// Wait until every listed task is terminal, then report what happened.
///
/// The wait is driven by the event stream with the snapshot as the floor: a
/// lagged receiver is told to re-read the snapshot (ADR-0004) and this loop
/// does that at the top of every iteration, so neither a lost event nor a burst
/// of them can strand it.
async fn drive(
    handle: &EngineHandle,
    events: &mut broadcast::Receiver<EngineEvent>,
    ids: &[TaskId],
) -> Result<Vec<TaskView>, CliError> {
    let interactive = std::io::stdout().is_terminal();
    let mut last_draw: Option<Instant> = None;
    loop {
        let snapshot = handle
            .snapshot()
            .await
            .map_err(|error| engine_error(error, "cannot read the task state"))?;
        let mut views = Vec::with_capacity(ids.len());
        for id in ids {
            match snapshot.get(*id) {
                Some(view) => views.push(view.clone()),
                None => return Err(CliError::not_found(*id)),
            }
        }
        if views.iter().all(|view| view.state.is_terminal()) {
            if last_draw.is_some() {
                clear_line();
            }
            return Ok(views);
        }
        let due = match last_draw {
            Some(drawn) => drawn.elapsed() >= REDRAW,
            None => true,
        };
        // §4: nothing at all on a non-terminal stdout. The acceptance suite
        // captures output, and control characters would only pollute its log.
        if interactive && due {
            draw_progress(&views);
            last_draw = Some(Instant::now());
        }
        tokio::select! {
            received = events.recv() => match received {
                Ok(_) => {}
                // The documented recovery from a lagged receiver is a fresh
                // snapshot, which is the first thing this loop does.
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => {
                    return Err(CliError::task("the engine event stream closed"));
                }
            },
            () = tokio::time::sleep(TICK) => {}
        }
    }
}

/// Report the outcome of a finished batch and turn it into an exit code.
fn finish(views: &[TaskView]) -> Result<Code, CliError> {
    let mut failed = false;
    for view in views {
        match view.state {
            TaskState::Completed => emit(&format!(
                "task {} completed: {} ({})\n",
                view.id,
                view.target_path,
                render::bytes(view.done_len)
            )),
            TaskState::Failed => {
                failed = true;
                let reason = match &view.failure {
                    Some(failure) => render::failure(failure),
                    None => "no structured reason was reported".to_string(),
                };
                emit_err(&format!("task {} failed: {reason}\n", view.id));
            }
            _ => {}
        }
    }
    if failed {
        Ok(Code::TaskFailed)
    } else {
        Ok(Code::Success)
    }
}

/// Map an engine error onto the exit codes of `spec/cli-v1.md` §3.
fn engine_error(error: EngineError, context: &str) -> CliError {
    match error {
        EngineError::UnknownTask => CliError {
            code: Code::NotFound,
            message: "no such task".to_string(),
        },
        EngineError::TargetExists { path } => CliError::conflict(format!(
            "target already exists, not overwriting: {}",
            path.display()
        )),
        EngineError::Source(failure) | EngineError::Unavailable(failure) => {
            CliError::failure(&failure, context)
        }
        EngineError::Transition(error) => CliError::task(format!(
            "{context}: the engine refused the transition ({error})"
        )),
        EngineError::ActorGone => {
            CliError::task(format!("{context}: the engine stopped responding"))
        }
    }
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// Write to stdout.
///
/// Failures are deliberately not propagated: a host whose stdout has gone away
/// — a closed pipe, a detached console — has nowhere left to complain, and
/// turning that into a task failure would report a download as broken because a
/// reader stopped reading. The results are still *inspected*, never discarded,
/// which is what the lint policy asks for.
fn emit(text: &str) {
    push(std::io::stdout(), text);
}

fn emit_err(text: &str) {
    push(std::io::stderr(), text);
}

/// Write and flush, inspecting both outcomes.
fn push(mut sink: impl Write, text: &str) {
    if sink.write_all(text.as_bytes()).is_err() {
        return;
    }
    if sink.flush().is_err() {
        // Nothing further to try: this *is* the reporting path.
    }
}

/// One redrawable line of progress. Only ever written to a terminal.
fn draw_progress(views: &[TaskView]) {
    let line = views
        .iter()
        .map(|view| {
            format!(
                "#{} {} {}",
                view.id,
                view.state.as_str(),
                render::progress(view)
            )
        })
        .collect::<Vec<_>>()
        .join("  |  ");
    // Padded so the tail of a longer previous line cannot survive.
    emit(&format!("\r{line:<100}"));
}

/// Erase the progress line before the summary is printed.
fn clear_line() {
    emit(&format!("\r{:<100}\r", ""));
}
