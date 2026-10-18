//! Pre-spawned workers (`worker::set_prespawn`) keep per-request isolation: a worker serves
//! one request and dies with it, a dead or expired idle worker is never used, the pool is
//! bounded, and a pre-spawned worker is still killed at its wall-clock budget. Driven through
//! the real `tw-worker.exe` and the same `Engine` the DLL uses.
#![cfg(windows)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tw_core::decode::DecodeError;
use tw_core::pipeline::Engine;
use tw_core::worker;
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
};

fn worker_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tw-worker"))
}

/// The pool is process-global, so the tests take turns; the guard leaves it off and empty.
struct Pool {
    _turn: MutexGuard<'static, ()>,
}

fn pool() -> Pool {
    static TURN: Mutex<()> = Mutex::new(());
    let guard = TURN.lock().unwrap_or_else(PoisonError::into_inner);
    // A freshly built executable is scanned by antivirus on its first launch; warm it.
    Command::new(worker_exe())
        .arg("--version")
        .status()
        .unwrap();
    worker::set_prespawn(false);
    worker::set_prespawn(true);
    Pool { _turn: guard }
}

impl Drop for Pool {
    fn drop(&mut self) {
        worker::set_prespawn(false);
    }
}

const SVG: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="20"><rect width="10" height="20" fill="#0000ff"/></svg>"##;

fn request(engine: &Engine) {
    let t = engine
        .thumbnail(Arc::new(SVG.to_vec()), Some("svg"), 40)
        .unwrap();
    assert_eq!((t.width, t.height), (20, 40));
    // Premultiplied BGRA of opaque blue.
    assert_eq!(&t.bgra[..4], &[255, 0, 0, 255]);
}

/// Whether process `pid` has exited (or never existed) within `ms`.
fn exited(pid: u32, ms: u32) -> bool {
    // SAFETY: plain handle calls; the handle is closed before returning.
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else {
            return true;
        };
        let r = WaitForSingleObject(h, ms);
        let _ = CloseHandle(h);
        r == WAIT_OBJECT_0
    }
}

fn kill_from_outside(pid: u32) {
    // SAFETY: plain handle calls on a process this test spawned.
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, false, pid).unwrap();
        TerminateProcess(h, 1).unwrap();
        let _ = CloseHandle(h);
    }
}

#[test]
fn a_prespawned_worker_serves_exactly_one_request() {
    let _pool = pool();
    let engine = Engine::new(worker_exe());

    request(&engine); // cold spawn; a replacement is left waiting
    let [first] = worker::idle_pids()[..] else {
        panic!("expected one idle worker, got {:?}", worker::idle_pids());
    };

    request(&engine); // takes `first`, which must not be offered again
    let after = worker::idle_pids();
    assert!(!after.contains(&first), "{first} still pooled: {after:?}");
    assert_eq!(after.len(), 1, "{after:?}");
    assert!(exited(first, 10_000), "worker {first} outlived its request");
}

#[test]
fn a_worker_that_died_while_idle_is_not_used() {
    let _pool = pool();
    let engine = Engine::new(worker_exe());
    request(&engine);
    let [dead] = worker::idle_pids()[..] else {
        panic!("expected one idle worker");
    };
    kill_from_outside(dead);
    assert!(exited(dead, 10_000));

    request(&engine); // must fall back to a fresh worker instead of failing
    assert!(!worker::idle_pids().contains(&dead));
}

#[test]
fn a_prespawned_worker_still_hits_the_wall_clock_budget() {
    let _pool = pool();
    request(&Engine::new(worker_exe())); // leaves an idle worker
    assert_eq!(worker::idle_pids().len(), 1);

    let mut slow = String::from(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="4000" height="4000"><defs>"#,
    );
    for i in 0..400 {
        slow += &format!(r#"<filter id="f{i}"><feGaussianBlur stdDeviation="200"/></filter>"#);
    }
    slow += "</defs>";
    for i in 0..400 {
        slow += &format!(r#"<rect width="4000" height="4000" fill="red" filter="url(#f{i})"/>"#);
    }
    slow += "</svg>";

    let engine = Engine {
        worker_exe: worker_exe(),
        max_tier_time: Duration::from_millis(300),
        video_percent: tw_core::limits::DEFAULT_VIDEO_PERCENT,
        prefer_embedded: true,
    };
    let start = Instant::now();
    let r = engine.thumbnail(Arc::new(slow.into_bytes()), Some("svg"), 1024);
    assert_eq!(r, Err(DecodeError::Timeout));
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn drain_and_switching_off_end_every_idle_worker() {
    let _pool = pool();
    let engine = Engine::new(worker_exe());
    request(&engine);
    let pids = worker::idle_pids();
    assert_eq!(pids.len(), 1);

    worker::drain();
    assert!(worker::idle_pids().is_empty());
    assert!(exited(pids[0], 10_000));

    request(&engine);
    let pids = worker::idle_pids();
    assert_eq!(pids.len(), 1);
    worker::set_prespawn(false);
    assert!(worker::idle_pids().is_empty());
    assert!(exited(pids[0], 10_000));
}

#[test]
fn concurrent_requests_never_share_a_worker_and_the_pool_stays_bounded() {
    let _pool = pool();
    let engine = Arc::new(Engine::new(worker_exe()));
    let before = worker::spawn_count();
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let engine = engine.clone();
            std::thread::spawn(move || request(&engine))
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let mut pids = worker::idle_pids();
    let count = pids.len();
    assert!(count <= worker::IDLE_MAX, "{pids:?}");
    pids.sort_unstable();
    pids.dedup();
    assert_eq!(pids.len(), count, "a worker is pooled twice");
    // Racing refills must not start workers that the cap then throws away.
    let started = worker::spawn_count() - before;
    assert!(
        started <= 8 + worker::IDLE_MAX,
        "{started} workers for 8 requests"
    );
}

#[test]
fn prespawning_costs_one_extra_process_not_one_per_request() {
    let _pool = pool();
    let engine = Engine::new(worker_exe());
    let before = worker::spawn_count();
    for _ in 0..4 {
        request(&engine);
    }
    // The first request starts its own worker; every request then starts its successor.
    assert_eq!(worker::spawn_count() - before, 4 + 1);
}

#[test]
fn prespawning_is_off_unless_enabled() {
    let _pool = pool();
    worker::set_prespawn(false);
    request(&Engine::new(worker_exe()));
    assert!(worker::idle_pids().is_empty());
}
