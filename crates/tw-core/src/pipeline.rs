//! Thumbnail pipeline shared by the shell DLL and `twctl`: routes the input, then runs
//! each tier in its required placement until one succeeds (FUNCTIONAL_SPEC.md 3.4/3.5).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_PIN, GetModuleHandleExW,
};
use windows::core::PCWSTR;

use crate::decode::route::{self, Decoder, Placement};
use crate::decode::{DecodeError, Result, Thumbnail, to_thumbnail};
use crate::limits;

/// Pins the module containing this code for the life of the process. Called when an
/// over-budget decode thread is abandoned: it still executes this module's code, so the
/// DLL must never be unloaded under it.
pub fn pin_this_module() {
    let mut module = HMODULE::default();
    // SAFETY: FROM_ADDRESS interprets the "name" as an address inside this module.
    let _ = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
            PCWSTR(pin_this_module as *const u16),
            &mut module,
        )
    };
}

/// Which tier produced a thumbnail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trace {
    pub decoder: Decoder,
    /// 1-based position in the routed chain; above 1 means earlier tiers fell through.
    pub attempt: usize,
    /// The worker process that ran the tier; `None` for an in-process tier.
    pub worker_pid: Option<u32>,
}

impl std::fmt::Display for Trace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "decoder={:?} attempt={} worker_pid=",
            self.decoder, self.attempt
        )?;
        match self.worker_pid {
            Some(pid) => write!(f, "{pid}"),
            None => f.write_str("-"),
        }
    }
}

pub struct Engine {
    /// `tw-worker.exe`, expected next to the DLL.
    pub worker_exe: PathBuf,
    /// Upper bound applied on top of each tier's own budget.
    pub max_tier_time: Duration,
    /// Video frame offset in percent of the duration (0-100).
    pub video_percent: u8,
    /// `UseEmbeddedThumbs`: try a camera file's embedded preview before a full decode.
    pub prefer_embedded: bool,
}

impl Engine {
    pub fn new(worker_exe: PathBuf) -> Self {
        Self {
            worker_exe,
            max_tier_time: limits::WORKER_THUMBNAIL_TIMEOUT,
            video_percent: limits::DEFAULT_VIDEO_PERCENT,
            prefer_embedded: true,
        }
    }

    /// Sets whether a camera file's embedded preview is tried before a full decode.
    pub fn with_prefer_embedded(mut self, prefer: bool) -> Self {
        self.prefer_embedded = prefer;
        self
    }

    /// Sets the video frame offset in percent of the duration (clamped to 100).
    pub fn with_video_percent(mut self, percent: u8) -> Self {
        self.video_percent = percent.min(100);
        self
    }

    /// Produces a thumbnail whose longer edge is at most `edge`.
    pub fn thumbnail(&self, data: Arc<Vec<u8>>, ext: Option<&str>, edge: u32) -> Result<Thumbnail> {
        self.thumbnail_traced(data, ext, edge).map(|(t, _)| t)
    }

    /// [`thumbnail`](Self::thumbnail), also reporting which tier produced the result
    /// (diagnostics log).
    pub fn thumbnail_traced(
        &self,
        data: Arc<Vec<u8>>,
        ext: Option<&str>,
        edge: u32,
    ) -> Result<(Thumbnail, Trace)> {
        let head = &data[..data.len().min(4096)];
        let chain = route::ordered(route::plan(ext, head), self.prefer_embedded);
        let ext = ext.unwrap_or("").to_owned();
        let mut last = DecodeError::Unsupported;
        for (index, &decoder) in chain.iter().enumerate() {
            let (result, worker_pid) = match decoder.placement() {
                Placement::InProc => (
                    run_inproc(
                        decoder,
                        data.clone(),
                        &ext,
                        edge,
                        limits::INPROC_DECODE_TIMEOUT.min(self.max_tier_time),
                    ),
                    None,
                ),
                Placement::Worker => {
                    let run = crate::worker::run_reporting(
                        &self.worker_exe,
                        decoder,
                        &ext,
                        data.clone(),
                        edge,
                        self.video_percent,
                        crate::worker::budget(decoder).min(self.max_tier_time),
                    );
                    (run.result, run.pid)
                }
            };
            match result {
                Ok(t) => {
                    let trace = Trace {
                        decoder,
                        attempt: index + 1,
                        worker_pid,
                    };
                    return Ok((t, trace));
                }
                Err(e) if e.falls_through() => last = e,
                Err(e) => return Err(e),
            }
        }
        Err(last)
    }
}

/// In-process decode threads that outlived their budget and have not finished. A Rust thread
/// cannot be stopped, so an abandoned one keeps its core (and its input) until it returns on
/// its own, if it ever does; each is also a reason the module can never be unloaded.
struct Abandoned(AtomicUsize);

impl Abandoned {
    const fn new() -> Self {
        Self(AtomicUsize::new(0))
    }

    fn count(&self) -> usize {
        self.0.load(Ordering::Acquire)
    }
}

static ABANDONED: Abandoned = Abandoned::new();

/// Lifecycle of one contained thread, shared between it and its caller.
const RUNNING: u8 = 0;
const FINISHED: u8 = 1;
const ABANDONED_STATE: u8 = 2;

/// How a contained thread is named and how its failures are reported.
struct Kind {
    thread: &'static str,
    panicked: &'static str,
    spawn: &'static str,
}

const DECODE: Kind = Kind {
    thread: "tw-decode",
    panicked: "decoder panicked",
    spawn: "spawn decode thread",
};

const PROBE: Kind = Kind {
    thread: "tw-probe",
    panicked: "probe panicked",
    spawn: "spawn probe thread",
};

/// Runs `work` on its own thread: a panic becomes `Malformed` (REQ-ISO-3) and a budget
/// overrun becomes `Timeout` (REQ-TMO-1). A finished thread is joined so no thread of ours
/// outlives the call. An overrunning one is abandoned and the module is pinned in memory
/// under it; at most [`limits::MAX_ABANDONED_DECODE_THREADS`] such threads are tolerated per
/// process, after which in-process work fails fast with `Timeout` (the shell retries a
/// `Timeout` file after a few minutes, by when the threads may have returned) instead of
/// giving one more hostile file one more permanently busy core.
fn run_contained<T: Send + 'static>(
    registry: &'static Abandoned,
    kind: &Kind,
    timeout: Duration,
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    if registry.count() >= limits::MAX_ABANDONED_DECODE_THREADS {
        return Err(DecodeError::Timeout);
    }
    let (tx, rx) = mpsc::channel();
    let state = Arc::new(AtomicU8::new(RUNNING));
    let thread_state = state.clone();
    let panicked = kind.panicked;
    let handle = std::thread::Builder::new()
        .name(kind.thread.into())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let out = catch_unwind(AssertUnwindSafe(work))
                .unwrap_or_else(|_| Err(DecodeError::malformed(panicked)));
            let _ = tx.send(out);
            // Finished after the caller gave up: it is no longer a running abandoned thread.
            if thread_state
                .compare_exchange(RUNNING, FINISHED, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                registry.0.fetch_sub(1, Ordering::AcqRel);
            }
        })
        .map_err(|e| DecodeError::malformed(format!("{}: {e}", kind.spawn)))?;
    match rx.recv_timeout(timeout) {
        Ok(out) => {
            let _ = handle.join();
            out
        }
        Err(_) => {
            // Count first, then claim: the thread decrements only after it sees the claim.
            registry.0.fetch_add(1, Ordering::AcqRel);
            if state
                .compare_exchange(
                    RUNNING,
                    ABANDONED_STATE,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                registry.0.fetch_sub(1, Ordering::AcqRel);
            }
            pin_this_module();
            Err(DecodeError::Timeout)
        }
    }
}

/// Runs a memory-safe decoder on its own thread, see [`run_contained`].
pub fn run_inproc(
    decoder: Decoder,
    data: Arc<Vec<u8>>,
    ext: &str,
    edge: u32,
    timeout: Duration,
) -> Result<Thumbnail> {
    let ext = ext.to_owned();
    run_contained(&ABANDONED, &DECODE, timeout, move || {
        decoder
            .run(&data, &ext, edge)
            .and_then(|img| to_thumbnail(img, edge))
    })
}

/// Header-only [`probe`](crate::decode::probe::probe) under the same containment as
/// [`run_inproc`]: own thread, panic contained, budget enforced, module pinned under an
/// abandoned thread, and counted against the same abandoned-thread cap.
pub fn run_probe(
    data: Arc<Vec<u8>>,
    ext: Option<&str>,
    timeout: Duration,
) -> Result<crate::decode::probe::ImageInfo> {
    let ext = ext.map(str::to_owned);
    run_contained(&ABANDONED, &PROBE, timeout, move || {
        crate::decode::probe::probe(&data, ext.as_deref())
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;

    use super::*;

    /// Polls until `done` holds (the abandoned threads are released asynchronously).
    fn eventually(done: impl Fn() -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs(30);
        while Instant::now() < end {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        done()
    }

    /// Work that blocks until `release` is set, then returns `Unsupported`.
    fn blocked(release: Arc<AtomicBool>) -> impl FnOnce() -> Result<()> + Send + 'static {
        move || {
            while !release.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(DecodeError::Unsupported)
        }
    }

    /// U-9 / DA-2: every overrunning decode used to leave one more permanently busy thread.
    #[test]
    fn abandoned_threads_are_capped_and_released() {
        static REGISTRY: Abandoned = Abandoned::new();
        let release = Arc::new(AtomicBool::new(false));
        let short = Duration::from_millis(30);
        for n in 1..=limits::MAX_ABANDONED_DECODE_THREADS {
            let r = run_contained(&REGISTRY, &DECODE, short, blocked(release.clone()));
            assert_eq!(r, Err(DecodeError::Timeout));
            assert_eq!(REGISTRY.count(), n);
        }
        // At the cap: fail fast with `Timeout`, and the work is not even started.
        let started = Arc::new(AtomicBool::new(false));
        let flag = started.clone();
        let begin = Instant::now();
        let r: Result<()> = run_contained(&REGISTRY, &DECODE, Duration::from_secs(60), move || {
            flag.store(true, Ordering::Release);
            Ok(())
        });
        assert_eq!(r, Err(DecodeError::Timeout));
        assert!(!started.load(Ordering::Acquire));
        assert!(begin.elapsed() < Duration::from_secs(30));
        assert_eq!(REGISTRY.count(), limits::MAX_ABANDONED_DECODE_THREADS);
        // Once the abandoned threads return, they stop counting and work is accepted again.
        release.store(true, Ordering::Release);
        assert!(eventually(|| REGISTRY.count() == 0), "count stuck");
        let r = run_contained(&REGISTRY, &DECODE, Duration::from_secs(30), || Ok(7));
        assert_eq!(r, Ok(7));
    }

    /// A thread that finishes in time is joined and never counted.
    #[test]
    fn finished_and_panicking_threads_are_not_counted() {
        static REGISTRY: Abandoned = Abandoned::new();
        let r = run_contained(&REGISTRY, &DECODE, Duration::from_secs(30), || Ok(1));
        assert_eq!(r, Ok(1));
        let r: Result<()> = run_contained(&REGISTRY, &PROBE, Duration::from_secs(30), || {
            panic!("boom")
        });
        assert_eq!(r, Err(DecodeError::malformed("probe panicked")));
        assert_eq!(REGISTRY.count(), 0);
    }
}
