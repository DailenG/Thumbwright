//! Parent side of the decoder worker (REQ-ISO-2, REQ-DEC-1, REQ-DEC-2, REQ-TMO-2).
//!
//! One worker process per request, and a worker never serves a second request. Input goes
//! over an anonymous stdin pipe, the thumbnail comes back over stdout. The process is
//! placed in a Job Object with a per-process committed-memory limit, a user-mode CPU-time
//! limit, a one-process limit (no grandchildren), and kill-on-close; independently of
//! those, a wall-clock watchdog terminates it when the budget expires.
//!
//! Process start-up (CreateProcess, loader, antivirus scan of the image) costs far more
//! than decoding a small SVG or audio cover, so the shell may turn on pre-spawning
//! ([`set_prespawn`]): up to [`IDLE_MAX`] workers are created ahead of time, already inside
//! their Job Object, blocked reading stdin. They have not read any input, so they are as
//! untrusted-data-free as a fresh spawn; a request takes one, uses it once and it exits
//! with its request. Isolation per request is unchanged; only the start-up latency moves
//! off the request path.

use std::io::{BufReader, BufWriter};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOB_OBJECT_LIMIT_PROCESS_TIME,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::decode::route::Decoder;
use crate::decode::{DecodeError, Result, Thumbnail};
use crate::{limits, protocol};

/// Owned Job Object handle; closing it kills any process still inside (KILL_ON_JOB_CLOSE).
struct Job(HANDLE);

// SAFETY: a Job Object handle is a process-wide kernel handle; every use of it is a
// thread-safe system call.
unsafe impl Send for Job {}

impl Job {
    fn new(memory_limit: u64) -> windows::core::Result<Self> {
        // SAFETY: anonymous job object; handle ownership moves into `Job`.
        let job = Job(unsafe { CreateJobObjectW(None, None)? });
        job.set_memory_limit(memory_limit)?;
        Ok(job)
    }

    /// Applies every limit. Only the memory ceiling varies per request, so a pre-spawned
    /// worker's job is re-applied with the ceiling of the request that takes it.
    fn set_memory_limit(&self, memory_limit: u64) -> windows::core::Result<()> {
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_PROCESS_MEMORY
            | JOB_OBJECT_LIMIT_PROCESS_TIME
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
            | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        info.BasicLimitInformation.ActiveProcessLimit = 1;
        // 100 ns units.
        info.BasicLimitInformation.PerProcessUserTimeLimit =
            (limits::WORKER_CPU_TIME.as_nanos() / 100) as i64;
        info.ProcessMemoryLimit = usize::try_from(memory_limit).unwrap_or(usize::MAX);
        // SAFETY: `info` is a valid JOBOBJECT_EXTENDED_LIMIT_INFORMATION of the stated size.
        unsafe {
            SetInformationJobObject(
                self.0,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        }
    }

    fn terminate(&self) {
        // SAFETY: valid job handle owned by self.
        let _ = unsafe { TerminateJobObject(self.0, 1) };
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: handle owned by self, closed exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// A worker process inside its job that has not been sent a request yet.
struct Worker {
    child: Child,
    job: Job,
}

impl Worker {
    fn spawn(exe: &Path, memory_limit: u64) -> Result<Self> {
        let job = Job::new(memory_limit)
            .map_err(|e| DecodeError::malformed(format!("job object: {e}")))?;
        let mut child = Command::new(exe)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            // Normal priority on purpose: the wall-clock budget must measure decoding, not
            // starvation behind busy normal-priority processes (a below-normal worker timed
            // out on a fully loaded machine and the failure was then cached).
            .creation_flags(CREATE_NO_WINDOW.0)
            .spawn()
            .map_err(|e| DecodeError::malformed(format!("spawn worker: {e}")))?;
        SPAWNED.fetch_add(1, Ordering::Relaxed);
        // The worker blocks on stdin until the request arrives, so it cannot allocate or run
        // decoder code before it is inside the job.
        // SAFETY: both handles are valid for the duration of the call.
        let assigned = unsafe { AssignProcessToJobObject(job.0, HANDLE(child.as_raw_handle())) };
        if assigned.is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(DecodeError::malformed("could not confine worker"));
        }
        Ok(Self { child, job })
    }

    /// Kills the process and waits for it to be gone.
    fn kill(mut self) {
        self.job.terminate();
        let _ = self.child.wait();
    }
}

/// Most pre-spawned workers kept waiting at once (Explorer asks for a few thumbnails in
/// parallel; each taken worker is replaced).
pub const IDLE_MAX: usize = 2;
/// An idle worker older than this is discarded instead of used, so a quiet shell does not
/// keep `tw-worker.exe` mapped (and an upgrade blocked) indefinitely.
const IDLE_TTL: Duration = Duration::from_secs(30);

struct Idle {
    exe: PathBuf,
    since: Instant,
    worker: Worker,
}

/// Idle workers plus the number being spawned right now, so concurrent refills never exceed
/// [`IDLE_MAX`] (a spawn that loses the race would be pure waste: a process start and its
/// antivirus scan, then a kill).
struct Pool {
    idle: Vec<Idle>,
    spawning: usize,
}

static PRESPAWN: AtomicBool = AtomicBool::new(false);
static SPAWNED: AtomicUsize = AtomicUsize::new(0);
static POOL: Mutex<Pool> = Mutex::new(Pool {
    idle: Vec::new(),
    spawning: 0,
});

fn pool() -> MutexGuard<'static, Pool> {
    POOL.lock().unwrap_or_else(PoisonError::into_inner)
}

fn expired(since: Instant, now: Instant) -> bool {
    now.saturating_duration_since(since) >= IDLE_TTL
}

/// Turns pre-spawning on or off. Off (the default; `twctl` runs one request per process and
/// would only waste a spawn) also kills every idle worker.
pub fn set_prespawn(on: bool) {
    PRESPAWN.store(on, Ordering::Relaxed);
    if !on {
        drain();
    }
}

/// Kills every idle pre-spawned worker. The shell calls this before its DLL can be unloaded.
pub fn drain() {
    let all = std::mem::take(&mut pool().idle);
    for i in all {
        i.worker.kill();
    }
}

/// Process ids of the idle workers (diagnostics and tests).
pub fn idle_pids() -> Vec<u32> {
    pool().idle.iter().map(|i| i.worker.child.id()).collect()
}

/// Worker processes started by this module so far (diagnostics and tests).
pub fn spawn_count() -> usize {
    SPAWNED.load(Ordering::Relaxed)
}

/// Takes a live idle worker for `exe`, discarding expired ones and any that died while
/// waiting. The caller owns it exclusively; it is never returned to the pool.
fn take_idle(exe: &Path, now: Instant) -> Option<Worker> {
    let (taken, stale) = {
        let mut pool = pool();
        let (stale, fresh): (Vec<Idle>, Vec<Idle>) = std::mem::take(&mut pool.idle)
            .into_iter()
            .partition(|i| expired(i.since, now));
        pool.idle = fresh;
        let taken = pool
            .idle
            .iter()
            .position(|i| i.exe == exe)
            .map(|p| pool.idle.swap_remove(p));
        (taken, stale)
    };
    for s in stale {
        s.worker.kill();
    }
    let mut worker = taken?.worker;
    match worker.child.try_wait() {
        Ok(None) => Some(worker),
        _ => {
            worker.kill();
            None
        }
    }
}

/// Tops the pool up with one worker for `exe`; a failed spawn is left for the next real
/// request to report.
fn refill(exe: &Path) {
    {
        let mut pool = pool();
        if pool.idle.len() + pool.spawning >= IDLE_MAX {
            return;
        }
        pool.spawning += 1;
    }
    let spawned = Worker::spawn(exe, limits::WORKER_MEMORY_BYTES);
    let mut pool = pool();
    pool.spawning -= 1;
    let Ok(worker) = spawned else { return };
    if PRESPAWN.load(Ordering::Relaxed) {
        pool.idle.push(Idle {
            exe: exe.to_path_buf(),
            since: Instant::now(),
            worker,
        });
    } else {
        // Switched off while this one was starting.
        drop(pool);
        worker.kill();
    }
}

/// Wall-clock budget for a worker-placed decoder.
pub fn budget(decoder: Decoder) -> Duration {
    match decoder {
        Decoder::Svg => limits::VECTOR_RENDER_TIMEOUT + limits::VECTOR_PARSE_TIMEOUT,
        Decoder::Audio => limits::INPROC_DECODE_TIMEOUT,
        Decoder::Video => limits::VIDEO_FRAME_TIMEOUT,
        Decoder::Pdf => limits::PDF_RENDER_TIMEOUT,
        Decoder::Metafile | Decoder::OleMetafile => limits::METAFILE_RENDER_TIMEOUT,
        _ => limits::WORKER_THUMBNAIL_TIMEOUT,
    }
}

/// Job memory limit for a worker running `decoder` on `input_len` bytes. The audio decoder
/// holds the whole file in memory before parsing tags, and the video and PDF decoders keep it
/// next to the OS decoder's own buffers, so their allowance grows with the input (video also
/// gets a larger base, see `limits::WORKER_VIDEO_EXTRA_MEMORY_BYTES`); every other decoder
/// gets the fixed worker limit.
fn memory_limit(decoder: Decoder, input_len: usize) -> u64 {
    match decoder {
        Decoder::Audio | Decoder::Pdf | Decoder::OleMetafile => {
            limits::WORKER_MEMORY_BYTES.saturating_add(input_len as u64)
        }
        Decoder::Video => limits::WORKER_VIDEO_EXTRA_MEMORY_BYTES.saturating_add(input_len as u64),
        // The worker holds the file, and the RAR reader keeps a copy of it.
        Decoder::Archive => {
            limits::WORKER_MEMORY_BYTES.saturating_add((input_len as u64).saturating_mul(2))
        }
        _ => limits::WORKER_MEMORY_BYTES,
    }
}

/// Runs `decoder` in a worker process used for this request only. Never blocks longer than
/// `timeout` (plus process teardown); a worker that crashes, exceeds a job limit or sends an
/// invalid response yields `Malformed`; a timeout yields `Timeout`. The budget starts once
/// the worker is confined and holds the request, so process start-up is not charged to it. `param` is the generic
/// decoder parameter (video: frame offset in percent).
pub fn run(
    exe: &Path,
    decoder: Decoder,
    ext: &str,
    data: Arc<Vec<u8>>,
    edge: u32,
    param: u8,
    timeout: Duration,
) -> Result<Thumbnail> {
    let prespawn = PRESPAWN.load(Ordering::Relaxed);
    let memory = memory_limit(decoder, data.len());
    let reused = if prespawn {
        take_idle(exe, Instant::now()).and_then(|w| match w.job.set_memory_limit(memory) {
            Ok(()) => Some(w),
            Err(_) => {
                w.kill();
                None
            }
        })
    } else {
        None
    };
    let Worker { mut child, job } = match reused {
        Some(w) => w,
        None => Worker::spawn(exe, memory)?,
    };

    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let ext_owned = ext.to_owned();
    let writer = std::thread::spawn(move || {
        let mut w = BufWriter::new(&mut stdin);
        // A worker that dies mid-request breaks the pipe; the reader reports it.
        let _ = protocol::write_request(&mut w, decoder, edge, param, &ext_owned, &data);
    });
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut r = BufReader::new(stdout);
        let _ = tx.send(protocol::read_response(&mut r, edge.max(1)));
    });

    let deadline = Instant::now() + timeout;
    if prespawn {
        // The replacement starts while this worker decodes.
        refill(exe);
    }

    let outcome = match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Ok(result)) => result,
        Ok(Err(io)) => Err(DecodeError::malformed(format!("worker failed: {io}"))),
        Err(_) => {
            job.terminate();
            Err(DecodeError::Timeout)
        }
    };
    job.terminate();
    let _ = child.wait();
    let _ = writer.join();
    let _ = reader.join();
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_file_decoders_get_input_proportional_memory_and_others_stay_fixed() {
        let base = limits::WORKER_MEMORY_BYTES;
        for d in [Decoder::Audio, Decoder::Pdf] {
            assert_eq!(memory_limit(d, 0), base);
            assert_eq!(memory_limit(d, 300 << 20), base + (300 << 20));
            assert_eq!(memory_limit(d, usize::MAX), u64::MAX);
        }
        assert_eq!(memory_limit(Decoder::Metafile, 300 << 20), base);
        // Video decoding needs its own, larger allowance (4K frames and decoder surfaces).
        let video = limits::WORKER_VIDEO_EXTRA_MEMORY_BYTES;
        assert!(video > base);
        assert_eq!(memory_limit(Decoder::Video, 0), video);
        assert_eq!(memory_limit(Decoder::Video, 300 << 20), video + (300 << 20));
        assert_eq!(memory_limit(Decoder::Video, usize::MAX), u64::MAX);
        assert_eq!(memory_limit(Decoder::Wic, 300 << 20), base);
        assert_eq!(
            memory_limit(Decoder::Archive, 100 << 20),
            base + (200 << 20)
        );
        assert_eq!(memory_limit(Decoder::Archive, usize::MAX), u64::MAX);
        assert_eq!(memory_limit(Decoder::Svg, 300 << 20), base);
    }

    #[test]
    fn idle_workers_expire_exactly_at_the_ttl() {
        let t0 = Instant::now();
        assert!(!expired(t0, t0));
        assert!(!expired(t0, t0 + IDLE_TTL - Duration::from_millis(1)));
        assert!(expired(t0, t0 + IDLE_TTL));
        // A clock that appears to run backwards never expires a worker.
        assert!(!expired(t0 + IDLE_TTL, t0));
    }

    #[test]
    fn retargeting_a_job_changes_only_the_memory_ceiling() {
        use windows::Win32::System::JobObjects::QueryInformationJobObject;
        let job = Job::new(limits::WORKER_MEMORY_BYTES).unwrap();
        let raised = limits::WORKER_MEMORY_BYTES + (300 << 20);
        job.set_memory_limit(raised).unwrap();
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        // SAFETY: valid job handle and a correctly sized out-struct.
        unsafe {
            QueryInformationJobObject(
                Some(job.0),
                JobObjectExtendedLimitInformation,
                (&raw mut info).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                None,
            )
            .unwrap();
        }
        assert_eq!(info.ProcessMemoryLimit as u64, raised);
        let flags = info.BasicLimitInformation.LimitFlags;
        for f in [
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOB_OBJECT_LIMIT_PROCESS_MEMORY,
            JOB_OBJECT_LIMIT_PROCESS_TIME,
            JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
            JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
        ] {
            assert!(flags.contains(f), "{f:?} lost");
        }
        assert_eq!(info.BasicLimitInformation.ActiveProcessLimit, 1);
        assert_eq!(
            info.BasicLimitInformation.PerProcessUserTimeLimit,
            (limits::WORKER_CPU_TIME.as_nanos() / 100) as i64
        );
    }
}
