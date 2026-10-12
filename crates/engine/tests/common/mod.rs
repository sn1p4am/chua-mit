//! Shared scaffolding for the end-to-end tests: a hand-written HTTP/1.1 server
//! and the small helpers the tests drive the engine with.
//!
//! The server is deliberately built on `tokio::net::TcpListener` rather than
//! pulled in as a dependency. The whole reason these tests exist is to observe
//! what the engine does with a server that misbehaves, and every off-the-shelf
//! test server treats "answer a range request with the whole file" as a bug to
//! be fixed rather than as a behaviour to be served on demand.
//!
//! Nothing here inspects engine internals: it speaks HTTP on one side and
//! `EngineHandle` on the other, so the assertions in the tests are about bytes
//! on disk and requests on the wire.

#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chua_engine::{EngineConfig, EngineHandle, SegmentPolicy, TaskSeed, TaskView};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// One mebibyte, so the test sizes read like the payloads they describe.
pub const MIB: usize = 1024 * 1024;

/// What the server does with a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Behaviour {
    /// Honest server: `Range` gets `206` and the exact interval asked for.
    HonourRange,
    /// Answers every request `200` with the whole body, whatever range was
    /// asked for. A probe against this server concludes that ranges are not
    /// supported, which is the safe conclusion.
    IgnoreRange,
    /// Answers the one-byte probe with `206` and every larger range with `200`
    /// plus the whole body. This is the dangerous shape: the probe proves range
    /// support, then the transfer requests are silently answered with the whole
    /// file, so writing that body at the requested offset would corrupt it.
    IgnoreRangeExceptProbe,
    /// `200` with no `Content-Length`, framed by closing the connection.
    NoLength,
    /// Promises the full length, sends half of it, closes.
    ShortBody,
    /// Honours ranges but writes only the first `keep` bytes of the body for
    /// the first `drops` segment responses, then closes the connection.
    DropMidBody {
        /// Bytes of the body to write before giving up.
        keep: usize,
        /// How many segment responses to cut short.
        drops: usize,
    },
}

/// State shared with the connection tasks.
struct State {
    payload: Vec<u8>,
    behaviour: Mutex<Behaviour>,
    /// Requests whose `Range` was exactly `bytes=0-0`, which is what the
    /// engine's metadata probe asks for.
    probes: AtomicUsize,
    /// Ranged requests that asked for more than the probe's single byte.
    segment_requests: AtomicUsize,
    /// Body bytes actually served for segment requests, excluding probes.
    served_bytes: AtomicUsize,
    /// How many responses have been cut short so far.
    drops_used: AtomicUsize,
    /// Most recent `Range` header seen on a segment request.
    last_range: Mutex<Option<String>>,
}

/// A running server.
pub struct Server {
    state: Arc<State>,
    addr: std::net::SocketAddr,
}

impl Server {
    /// Start a server on an ephemeral port of the loopback interface.
    pub async fn start(payload: Vec<u8>, behaviour: Behaviour) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the test server could not bind a loopback port");
        let addr = listener
            .local_addr()
            .expect("a bound listener always has an address");
        let state = Arc::new(State {
            payload,
            behaviour: Mutex::new(behaviour),
            probes: AtomicUsize::new(0),
            segment_requests: AtomicUsize::new(0),
            served_bytes: AtomicUsize::new(0),
            drops_used: AtomicUsize::new(0),
            last_range: Mutex::new(None),
        });
        let accept_state = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                let Ok((socket, _peer)) = listener.accept().await else {
                    return;
                };
                let connection_state = Arc::clone(&accept_state);
                tokio::spawn(async move {
                    serve(socket, connection_state).await;
                });
            }
        });
        Self { state, addr }
    }

    /// The URL of the one payload this server serves.
    pub fn url(&self) -> String {
        format!("http://{}/payload.bin", self.addr)
    }

    /// How many metadata probes arrived.
    pub fn probes(&self) -> usize {
        self.state.probes.load(Ordering::SeqCst)
    }

    /// How many range requests asked for more than one byte.
    pub fn segment_requests(&self) -> usize {
        self.state.segment_requests.load(Ordering::SeqCst)
    }

    /// Body bytes served for segment requests.
    pub fn served_bytes(&self) -> usize {
        self.state.served_bytes.load(Ordering::SeqCst)
    }

    /// The most recent range header seen on a segment request.
    pub fn last_range(&self) -> Option<String> {
        self.state
            .last_range
            .lock()
            .expect("the range log mutex is never poisoned in a test")
            .clone()
    }

    /// Change what the server does from the next request onwards.
    pub fn set_behaviour(&self, behaviour: Behaviour) {
        *self
            .state
            .behaviour
            .lock()
            .expect("the behaviour mutex is never poisoned in a test") = behaviour;
    }
}

/// Serve exactly one request on this connection and then close it.
///
/// HTTP/1.1 lets a server close after any response, and `Connection: close`
/// says so explicitly. One request per connection keeps the framing logic
/// trivial, which matters when the point of the server is to be wrong in
/// controlled ways.
async fn serve(mut socket: TcpStream, state: Arc<State>) {
    let Some(head) = read_head(&mut socket).await else {
        return;
    };
    let behaviour = *state
        .behaviour
        .lock()
        .expect("the behaviour mutex is never poisoned in a test");
    let raw_range = header(&head, "range").map(str::to_string);
    let range = raw_range.as_deref().and_then(parse_range);
    // The metadata probe is `bytes=0-0`: exactly one byte at offset zero.
    let is_probe = matches!(range, Some((0, 0)));
    if is_probe {
        state.probes.fetch_add(1, Ordering::SeqCst);
    } else if range.is_some() {
        state.segment_requests.fetch_add(1, Ordering::SeqCst);
        *state
            .last_range
            .lock()
            .expect("the range log mutex is never poisoned in a test") = raw_range.clone();
    }

    let total = state.payload.len() as u64;
    // `Some(interval)` means this response carries an interval the server is
    // willing to honour; `None` means the whole representation with `200`.
    let honoured = match behaviour {
        Behaviour::HonourRange | Behaviour::DropMidBody { .. } => range,
        Behaviour::IgnoreRange => None,
        Behaviour::IgnoreRangeExceptProbe => range.filter(|_| is_probe),
        Behaviour::NoLength | Behaviour::ShortBody => None,
    };

    if behaviour == Behaviour::NoLength {
        let head = response_head(200, &[("Content-Type", "application/octet-stream")]);
        if !send(&mut socket, head.as_bytes()).await {
            return;
        }
        if !send(&mut socket, &state.payload).await {
            return;
        }
        if socket.flush().await.is_err() {
            return;
        }
        return;
    }

    if behaviour == Behaviour::ShortBody {
        // Promise everything, deliver half, then close. The client has to
        // notice that the body ended early.
        let promised = state.payload.len().to_string();
        let half = state.payload.len() / 2;
        let head = response_head(
            200,
            &[
                ("Content-Type", "application/octet-stream"),
                ("Content-Length", &promised),
            ],
        );
        if !send(&mut socket, head.as_bytes()).await {
            return;
        }
        if !send(&mut socket, &state.payload[..half]).await {
            return;
        }
        if socket.flush().await.is_err() {
            return;
        }
        return;
    }

    let (body, head) = match honoured {
        Some((start, end)) if start < total => {
            let end = end.min(total - 1);
            let body = &state.payload[start as usize..=end as usize];
            let content_range = format!("bytes {start}-{end}/{total}");
            let length = body.len().to_string();
            let head = response_head(
                206,
                &[
                    ("Content-Type", "application/octet-stream"),
                    ("Accept-Ranges", "bytes"),
                    ("Content-Range", &content_range),
                    ("Content-Length", &length),
                ],
            );
            (body, head)
        }
        _ => {
            let length = state.payload.len().to_string();
            let head = response_head(
                200,
                &[
                    ("Content-Type", "application/octet-stream"),
                    ("Accept-Ranges", "bytes"),
                    ("Content-Length", &length),
                ],
            );
            (state.payload.as_slice(), head)
        }
    };

    // How much of the body actually goes out.
    let keep = match behaviour {
        Behaviour::DropMidBody { keep, drops } => {
            let used = state.drops_used.load(Ordering::SeqCst);
            if used < drops && body.len() > keep {
                state.drops_used.fetch_add(1, Ordering::SeqCst);
                keep
            } else {
                body.len()
            }
        }
        _ => body.len(),
    };

    if !is_probe && honoured.is_some() {
        state.served_bytes.fetch_add(keep, Ordering::SeqCst);
    }

    if !send(&mut socket, head.as_bytes()).await {
        return;
    }
    if !send(&mut socket, &body[..keep]).await {
        return;
    }
    // The flush is the last step: dropping the socket after it closes the
    // connection, which is how a truncated body is signalled when
    // `Content-Length` promised more.
    if socket.flush().await.is_err() {
        tracing::trace!("the client went away before the response was flushed");
    }
}

/// A response head. The connection always closes after one response.
fn response_head(status: u16, headers: &[(&str, &str)]) -> String {
    let reason = match status {
        200 => "OK",
        206 => "Partial Content",
        _ => "Status",
    };
    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    for (name, value) in headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("Connection: close\r\n\r\n");
    head
}

/// Write, reporting whether the peer is still there. A client that has gone
/// away mid-response is not this server's problem.
async fn send(socket: &mut TcpStream, bytes: &[u8]) -> bool {
    socket.write_all(bytes).await.is_ok()
}

/// Read up to the end of the request head.
async fn read_head(socket: &mut TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match socket.read(&mut chunk).await {
            Ok(0) => return None,
            Ok(read) => {
                buffer.extend_from_slice(&chunk[..read]);
                if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                    return Some(String::from_utf8_lossy(&buffer).into_owned());
                }
                if buffer.len() > 64 * 1024 {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
}

/// Case-insensitive header lookup over a raw request head.
fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// `bytes=start-end`, with an open-ended `bytes=start-` reported as `u64::MAX`.
fn parse_range(raw: &str) -> Option<(u64, u64)> {
    let spec = raw.trim().strip_prefix("bytes=")?;
    let spec = spec.split(',').next()?.trim();
    let (first, last) = spec.split_once('-')?;
    let start: u64 = first.trim().parse().ok()?;
    let end = if last.trim().is_empty() {
        u64::MAX
    } else {
        last.trim().parse().ok()?
    };
    (end >= start).then_some((start, end))
}

/// Deterministic, incompressible-looking bytes. A constant fill would let a
/// file that was never written at all pass a comparison.
pub fn payload(len: usize) -> Vec<u8> {
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// The engine configuration the tests use.
///
/// The flush window is far shorter than the default so that durable progress
/// exists within a fraction of a second: the resume test kills the engine and
/// depends on there being something durable to resume from.
pub fn test_config() -> EngineConfig {
    EngineConfig {
        progress_flush_interval: Duration::from_millis(100),
        segment_policy: SegmentPolicy {
            max_segments: 4,
            granularity: MIB as u64,
            single_segment_up_to: 4 * MIB as u64,
            min_segment_bytes: MIB as u64,
        },
        auto_resume: false,
        ..EngineConfig::default()
    }
}

/// The configuration a host uses to pick up where an interrupted run left off.
pub fn resume_config() -> EngineConfig {
    EngineConfig {
        auto_resume: true,
        ..test_config()
    }
}

/// A seed that writes to exactly `dest`.
pub fn seed(url: &str, dest: &std::path::Path) -> TaskSeed {
    let parent = dest
        .parent()
        .expect("a test destination always has a parent directory");
    let mut seed = TaskSeed::new(url, parent);
    seed.explicit_target = Some(dest.to_path_buf());
    seed.name_hint = dest
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    seed
}

/// Register a task and wait for it to reach a terminal state.
///
/// The wait is bounded: an engine that deadlocks has to fail the test rather
/// than hang the suite, which is exactly what happened the first time the
/// transfer loop was driven against a real socket.
pub async fn add_and_wait(handle: &EngineHandle, seed: TaskSeed, budget: Duration) -> TaskView {
    let mut events = handle.subscribe();
    let id = handle
        .add(seed)
        .await
        .expect("the engine refused to register the task");
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        // The snapshot is both the documented recovery path for a lagged
        // receiver and simply cheaper than trusting the stream alone.
        let latest = handle.snapshot().await.expect("snapshot").get(id).cloned();
        if let Some(view) = latest.as_ref()
            && view.state.is_terminal()
        {
            return view.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            // Printing where it stopped is the difference between "it hung" and
            // a diagnosis: a stuck probe, a task that never got a slot and a
            // transfer whose completion never fired look nothing alike.
            "task {id} did not reach a terminal state within {budget:?}; last seen: {latest:#?}"
        );
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                panic!("the event stream closed while task {id} was still running")
            }
            // The deadline fired; the assertion at the top of the loop reports it.
            Err(_) => {}
        }
    }
}

/// Wait until every task the engine knows about is terminal.
pub async fn wait_all_terminal(handle: &EngineHandle, budget: Duration) -> Vec<TaskView> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let snapshot = handle.snapshot().await.expect("snapshot");
        if snapshot.tasks.iter().all(|task| task.state.is_terminal()) {
            return snapshot.tasks;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "tasks did not settle within {budget:?}: {:?}",
            snapshot
                .tasks
                .iter()
                .map(|task| (task.id, task.state))
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Compare two byte strings without printing megabytes of them on failure.
pub fn assert_same_bytes(written: &[u8], source: &[u8]) {
    assert_eq!(
        written.len(),
        source.len(),
        "the file on disk is {} bytes, the source is {}",
        written.len(),
        source.len()
    );
    if let Some(offset) = written
        .iter()
        .zip(source.iter())
        .position(|(left, right)| left != right)
    {
        panic!(
            "the file on disk differs from the source at byte {offset} of {}",
            source.len()
        );
    }
}
