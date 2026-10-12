//! End-to-end tests: the engine against a real HTTP server over a real socket.
//!
//! Every test here asserts on **bytes on disk**. A test that passes because it
//! checked a flag rather than the file would not be evidence, and the failure
//! this suite exists to catch — a completed download whose bytes are wrong — is
//! invisible to any check but a byte comparison.
//!
//! The one thing each test has in common is the sentence every acceptance
//! scenario reduces to: *either the engine fails loudly, or the bytes on disk
//! are correct.* Never "reported success while the file is silently wrong".

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::{Duration, Instant};

use chua_engine::{Engine, FailureKind, TaskId, TaskState};
use common::{
    Behaviour, MIB, Server, add_and_wait, assert_same_bytes, payload, resume_config, seed,
    test_config, wait_all_terminal,
};
use tempfile::TempDir;

/// A clean multi-segment download must reproduce the source byte for byte.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_multi_segment_download_is_byte_for_byte_equal_to_the_source() {
    let source = payload(8 * MIB);
    let server = Server::start(source.clone(), Behaviour::HonourRange).await;
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("clean.bin");

    let engine = Engine::open(&dir.path().join("state"), test_config())
        .await
        .expect("the engine opens against a fresh state directory");
    let handle = engine.handle();
    let view = add_and_wait(&handle, seed(&server.url(), &dest), Duration::from_secs(60)).await;

    assert_eq!(view.state, TaskState::Completed, "{:?}", view.failure);
    assert_same_bytes(&std::fs::read(&dest).unwrap(), &source);
    // More than one segment really was requested: a single-request download
    // would not exercise the per-offset write path at all.
    assert!(
        server.segment_requests() >= 2,
        "expected several segment requests, saw {}",
        server.segment_requests()
    );
    handle.shutdown().await;
}

/// The most dangerous bug class in the project: a server that answers a ranged
/// request with `200` and the whole file.
///
/// Writing that body at the requested offset would produce a full-size file of
/// wrong bytes that the engine then reports as complete. §6.3 (v1.0.4) forbids
/// exactly that and nothing more: failing, or recovering by discarding the
/// segment plan and taking the whole file from offset 0, are both compliant.
/// The hard requirement is on the *result*, so that is what is asserted.
///
/// This server proves range support with the probe and then ignores every
/// larger range, which is the shape that leaves no way to recover: the source
/// still claims to serve intervals, so "[re-probe and] re-plan" cannot fix it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_server_that_answers_a_range_with_200_never_yields_a_plausible_wrong_file() {
    let source = payload(8 * MIB);
    let server = Server::start(source.clone(), Behaviour::IgnoreRangeExceptProbe).await;
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("ignored.bin");

    let engine = Engine::open(&dir.path().join("state"), test_config())
        .await
        .expect("the engine opens against a fresh state directory");
    let handle = engine.handle();
    let view = add_and_wait(&handle, seed(&server.url(), &dest), Duration::from_secs(60)).await;

    match view.state {
        // Compliant route one: refuse the source, loudly. This is the route the
        // engine takes here, because the source never stops claiming that it can
        // serve intervals.
        TaskState::Failed => {
            let failure = view
                .failure
                .as_ref()
                .expect("a failed task carries a structured reason");
            assert_eq!(failure.kind, FailureKind::DataMismatch, "{failure:?}");
            // ... and do not leave a plausible-looking file behind.
            match std::fs::read(&dest) {
                Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::NotFound),
                Ok(bytes) => assert!(
                    !(bytes.len() == source.len() && bytes != source),
                    "SILENT CORRUPTION: {} bytes on disk claiming to be the whole file",
                    bytes.len()
                ),
            }
        }
        // Compliant route two: abandon the segment plan and take the whole
        // response from offset 0 — which is only acceptable because the bytes
        // then are the real thing.
        TaskState::Completed => assert_same_bytes(&std::fs::read(&dest).unwrap(), &source),
        other => panic!("a degraded response left the task in {other}"),
    }
    handle.shutdown().await;
}

/// The same danger, one step earlier: a server that ignores ranges from the
/// very first request, so the probe never sees a `206`.
///
/// There is no interval to be wrong about here — the engine must conclude that
/// the source cannot be segmented at all and fetch the whole thing as one
/// sequential request, ending with correct bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_source_that_never_honours_a_range_is_downloaded_as_one_whole_file() {
    let source = payload(4 * MIB);
    let server = Server::start(source.clone(), Behaviour::IgnoreRange).await;
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("whole.bin");

    let engine = Engine::open(&dir.path().join("state"), test_config())
        .await
        .expect("the engine opens against a fresh state directory");
    let handle = engine.handle();
    let view = add_and_wait(&handle, seed(&server.url(), &dest), Duration::from_secs(60)).await;

    assert_eq!(view.state, TaskState::Completed, "{:?}", view.failure);
    assert!(!view.range_supported, "the probe saw no 206");
    assert_same_bytes(&std::fs::read(&dest).unwrap(), &source);
    assert_eq!(
        server.segment_requests(),
        1,
        "a source without range support must be fetched in one request"
    );
    handle.shutdown().await;
}

/// Kill the engine mid-transfer and resume it: the final bytes must be correct,
/// and the already-durable bytes must not be fetched again.
///
/// This is where "`done_len` counts bytes that are *durable*, not bytes that
/// were *received*" earns its keep. A transfer that completed normally would
/// look identical either way; only a kill in the middle distinguishes them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_interrupted_download_resumes_and_the_final_bytes_are_correct() {
    const SIZE: usize = 4 * MIB;
    const RATE: u64 = 1024 * 1024;
    const THRESHOLD: u64 = 512 * 1024;

    let source = payload(SIZE);
    let server = Server::start(source.clone(), Behaviour::HonourRange).await;
    let dir = TempDir::new().unwrap();
    let state_dir = dir.path().join("state");
    let dest = dir.path().join("resume.bin");

    // Phase one runs on a runtime of its own, which is dropped without a
    // shutdown. That is as close to a power loss as an in-process test gets:
    // no final flush, no orderly stop, no notification to anyone.
    let engine_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (progress_tx, progress_rx) = tokio::sync::oneshot::channel::<TaskId>();
    let url = server.url();
    let phase_one_state = state_dir.clone();
    let phase_one_dest = dest.clone();
    engine_rt.spawn(async move {
        let engine = Engine::open(&phase_one_state, test_config())
            .await
            .expect("open");
        let handle = engine.handle();
        let mut task = seed(&url, &phase_one_dest);
        // Slow enough that the kill lands mid-transfer rather than after it.
        task.rate_limit_bps = Some(RATE);
        let id = handle.add(task).await.expect("add");
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let snapshot = handle.snapshot().await.expect("snapshot");
            match snapshot.get(id) {
                Some(view) if view.done_len >= THRESHOLD => {
                    if progress_tx.send(id).is_err() {
                        return;
                    }
                    break;
                }
                Some(view) if view.state.is_terminal() => return,
                _ => {}
            }
            if Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Hold the engine open until the runtime is torn down from outside.
        std::future::pending::<()>().await;
    });

    let id = tokio::time::timeout(Duration::from_secs(60), progress_rx)
        .await
        .expect("the first phase never made durable progress")
        .expect("the first phase ended before it could be interrupted");

    // Power loss. The runtime is torn down from a thread of its own because
    // tokio refuses to drop a runtime inside an async context; what matters is
    // that the engine never gets a graceful shutdown, so nothing is flushed on
    // the way out and the store keeps only what the periodic flush wrote.
    let teardown = std::thread::Builder::new()
        .name("engine-teardown".to_string())
        .spawn(move || drop(engine_rt))
        .expect("the teardown thread starts");
    assert!(
        teardown.join().is_ok(),
        "the teardown thread panicked while dropping the runtime"
    );

    let served_before = server.served_bytes();

    // Phase two is a fresh host opening the same state directory. Nothing is
    // passed in except the configuration: the task, its layout and its durable
    // byte count all come out of the store.
    let engine = Engine::open(&state_dir, resume_config())
        .await
        .expect("the engine reopens the state it was killed with");
    let handle = engine.handle();
    let tasks = wait_all_terminal(&handle, Duration::from_secs(120)).await;
    let view = tasks
        .iter()
        .find(|task| task.id == id)
        .expect("the interrupted task was restored");
    assert_eq!(view.state, TaskState::Completed, "{:?}", view.failure);
    assert_same_bytes(&std::fs::read(&dest).unwrap(), &source);

    let re_served = server.served_bytes() - served_before;
    assert!(
        re_served <= SIZE - THRESHOLD as usize,
        "the resume fetched {re_served} of {SIZE} bytes: the durable progress was thrown away"
    );
    handle.shutdown().await;
}

/// Without a `Content-Length` the engine has to degrade to one sequential
/// segment, and it must still produce the right bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_length_source_degrades_to_a_single_sequential_request() {
    let source = payload(3 * MIB);
    let server = Server::start(source.clone(), Behaviour::NoLength).await;
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("nolen.bin");

    let engine = Engine::open(&dir.path().join("state"), test_config())
        .await
        .expect("the engine opens against a fresh state directory");
    let handle = engine.handle();
    let view = add_and_wait(&handle, seed(&server.url(), &dest), Duration::from_secs(60)).await;

    assert_eq!(view.state, TaskState::Completed, "{:?}", view.failure);
    assert_same_bytes(&std::fs::read(&dest).unwrap(), &source);
    // Exactly one segment request, and it asked for everything: §4.3 forbids
    // attempting to segment a representation whose length is unknown.
    assert_eq!(
        server.segment_requests(),
        1,
        "an unknown-length source must be fetched as one sequential request"
    );
    assert_eq!(
        server.last_range().as_deref(),
        Some("bytes=0-"),
        "the single segment must ask for the whole representation"
    );
    handle.shutdown().await;
}

/// A connection dropped part-way through a body is a re-plan, not a failure:
/// the engine re-probes, keeps what it durably has, and finishes correctly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_body_cut_short_by_a_dropped_connection_is_recovered() {
    let source = payload(8 * MIB);
    let server = Server::start(
        source.clone(),
        Behaviour::DropMidBody {
            keep: 64 * 1024,
            drops: 2,
        },
    )
    .await;
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("dropped.bin");

    let engine = Engine::open(&dir.path().join("state"), test_config())
        .await
        .expect("the engine opens against a fresh state directory");
    let handle = engine.handle();
    let view = add_and_wait(&handle, seed(&server.url(), &dest), Duration::from_secs(60)).await;

    assert_eq!(view.state, TaskState::Completed, "{:?}", view.failure);
    assert_same_bytes(&std::fs::read(&dest).unwrap(), &source);
    handle.shutdown().await;
}

/// `--limit` has to have an effect on the bytes that actually move.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_limited_transfer_stays_under_its_ceiling() {
    const SIZE: usize = 4 * MIB;
    const RATE: u64 = 2 * 1024 * 1024;

    let source = payload(SIZE);
    let server = Server::start(source.clone(), Behaviour::HonourRange).await;
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("limited.bin");

    let engine = Engine::open(&dir.path().join("state"), test_config())
        .await
        .expect("the engine opens against a fresh state directory");
    let handle = engine.handle();
    let mut task = seed(&server.url(), &dest);
    task.rate_limit_bps = Some(RATE);
    let started = Instant::now();
    let view = add_and_wait(&handle, task, Duration::from_secs(120)).await;
    let elapsed = started.elapsed();

    assert_eq!(view.state, TaskState::Completed, "{:?}", view.failure);
    assert_same_bytes(&std::fs::read(&dest).unwrap(), &source);
    // Four mebibytes at two mebibytes per second is two seconds. The lower
    // bound is what matters — an unlimited transfer finishes in milliseconds —
    // and the upper bound only has to catch a pathological stall.
    assert!(
        elapsed >= Duration::from_millis(1200),
        "a capped transfer finished in {elapsed:?}, which is faster than its ceiling allows"
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "transfer took {elapsed:?}"
    );
    handle.shutdown().await;
}
