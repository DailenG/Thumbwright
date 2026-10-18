//! Parent side of the decoder worker (REQ-ISO-2, REQ-DEC-1, REQ-DEC-2, REQ-TMO-2).
//!
//! One worker process per request. Input goes over an anonymous stdin pipe, the
//! thumbnail comes back over stdout. The process is placed in a Job Object with a
//! per-process committed-memory limit, a user-mode CPU-time limit, a one-process limit
//! (no grandchildren), and kill-on-close; independently of those, a wall-clock watchdog
//! terminates it when the budget expires.

use std::io::{BufReader, BufWriter};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, mpsc};
use std::time::Duration;

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

impl Job {
    fn new(memory_limit: u64) -> windows::core::Result<Self> {
        // SAFETY: anonymous job object; handle ownership moves into `Job`.
        let job = Job(unsafe { CreateJobObjectW(None, None)? });
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
                job.0,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )?;
        }
        Ok(job)
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

/// Wall-clock budget for a worker-placed decoder.
pub fn budget(decoder: Decoder) -> Duration {
    match decoder {
        Decoder::Svg => limits::VECTOR_RENDER_TIMEOUT + limits::VECTOR_PARSE_TIMEOUT,
        Decoder::Audio => limits::INPROC_DECODE_TIMEOUT,
        _ => limits::WORKER_THUMBNAIL_TIMEOUT,
    }
}

/// Job memory limit for a worker running `decoder` on `input_len` bytes. The audio decoder
/// holds the whole file in memory before parsing tags, so its allowance grows with the input;
/// every other decoder gets the fixed worker limit.
fn memory_limit(decoder: Decoder, input_len: usize) -> u64 {
    match decoder {
        Decoder::Audio => limits::WORKER_MEMORY_BYTES.saturating_add(input_len as u64),
        // The worker holds the file, and the RAR reader keeps a copy of it.
        Decoder::Archive => {
            limits::WORKER_MEMORY_BYTES.saturating_add((input_len as u64).saturating_mul(2))
        }
        _ => limits::WORKER_MEMORY_BYTES,
    }
}

/// Runs `decoder` in a fresh worker process. Never blocks longer than `timeout` (plus
/// process teardown); a worker that crashes, exceeds a job limit or sends an invalid
/// response yields `Malformed`; a timeout yields `Timeout`.
pub fn run(
    exe: &Path,
    decoder: Decoder,
    ext: &str,
    data: Arc<Vec<u8>>,
    edge: u32,
    timeout: Duration,
) -> Result<Thumbnail> {
    let job = Job::new(memory_limit(decoder, data.len()))
        .map_err(|e| DecodeError::malformed(format!("job object: {e}")))?;
    let mut child = Command::new(exe)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Normal priority on purpose: the wall-clock budget must measure decoding, not
        // starvation behind busy normal-priority processes (a below-normal worker timed out
        // on a fully loaded machine and the failure was then cached).
        .creation_flags(CREATE_NO_WINDOW.0)
        .spawn()
        .map_err(|e| DecodeError::malformed(format!("spawn worker: {e}")))?;
    // The worker blocks on stdin until the request arrives, so it cannot allocate or run
    // decoder code before it is inside the job.
    // SAFETY: both handles are valid for the duration of the call.
    let assigned = unsafe { AssignProcessToJobObject(job.0, HANDLE(child.as_raw_handle())) };
    if assigned.is_err() {
        let _ = child.kill();
        let _ = child.wait();
        return Err(DecodeError::malformed("could not confine worker"));
    }

    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let ext_owned = ext.to_owned();
    let writer = std::thread::spawn(move || {
        let mut w = BufWriter::new(&mut stdin);
        // A worker that dies mid-request breaks the pipe; the reader reports it.
        let _ = protocol::write_request(&mut w, decoder, edge, &ext_owned, &data);
    });
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut r = BufReader::new(stdout);
        let _ = tx.send(protocol::read_response(&mut r, edge.max(1)));
    });

    let outcome = match rx.recv_timeout(timeout) {
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
    fn audio_memory_limit_grows_with_input_and_others_stay_fixed() {
        let base = limits::WORKER_MEMORY_BYTES;
        assert_eq!(memory_limit(Decoder::Audio, 0), base);
        assert_eq!(memory_limit(Decoder::Audio, 300 << 20), base + (300 << 20));
        assert_eq!(memory_limit(Decoder::Audio, usize::MAX), u64::MAX);
        assert_eq!(memory_limit(Decoder::Wic, 300 << 20), base);
        assert_eq!(
            memory_limit(Decoder::Archive, 100 << 20),
            base + (200 << 20)
        );
        assert_eq!(memory_limit(Decoder::Archive, usize::MAX), u64::MAX);
        assert_eq!(memory_limit(Decoder::Svg, 300 << 20), base);
    }
}
