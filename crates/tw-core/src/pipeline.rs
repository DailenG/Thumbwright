//! Thumbnail pipeline shared by the shell DLL and `twctl`: routes the input, then runs
//! each tier in its required placement until one succeeds (FUNCTIONAL_SPEC.md 3.4/3.5).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
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

pub struct Engine {
    /// `tw-worker.exe`, expected next to the DLL.
    pub worker_exe: PathBuf,
    /// Upper bound applied on top of each tier's own budget.
    pub max_tier_time: Duration,
    /// Video frame offset in percent of the duration (0-100).
    pub video_percent: u8,
}

impl Engine {
    pub fn new(worker_exe: PathBuf) -> Self {
        Self {
            worker_exe,
            max_tier_time: limits::WORKER_THUMBNAIL_TIMEOUT,
            video_percent: limits::DEFAULT_VIDEO_PERCENT,
        }
    }

    /// Sets the video frame offset in percent of the duration (clamped to 100).
    pub fn with_video_percent(mut self, percent: u8) -> Self {
        self.video_percent = percent.min(100);
        self
    }

    /// Produces a thumbnail whose longer edge is at most `edge`.
    pub fn thumbnail(&self, data: Arc<Vec<u8>>, ext: Option<&str>, edge: u32) -> Result<Thumbnail> {
        let head = &data[..data.len().min(4096)];
        let chain = route::plan(ext, head);
        let ext = ext.unwrap_or("").to_owned();
        let mut last = DecodeError::Unsupported;
        for &decoder in chain {
            let result = match decoder.placement() {
                Placement::InProc => run_inproc(
                    decoder,
                    data.clone(),
                    &ext,
                    edge,
                    limits::INPROC_DECODE_TIMEOUT.min(self.max_tier_time),
                ),
                Placement::Worker => crate::worker::run(
                    &self.worker_exe,
                    decoder,
                    &ext,
                    data.clone(),
                    edge,
                    self.video_percent,
                    crate::worker::budget(decoder).min(self.max_tier_time),
                ),
            };
            match result {
                Ok(t) => return Ok(t),
                Err(e) if e.falls_through() => last = e,
                Err(e) => return Err(e),
            }
        }
        Err(last)
    }
}

/// Runs a memory-safe decoder on its own thread: a panic becomes `Malformed` (REQ-ISO-3)
/// and a budget overrun becomes `Timeout` (REQ-TMO-1). A finished thread is joined so no
/// thread of ours outlives the call; an overrunning one is abandoned (its allocations are
/// already bounded by the decoder limits) and the module is pinned in memory under it.
pub fn run_inproc(
    decoder: Decoder,
    data: Arc<Vec<u8>>,
    ext: &str,
    edge: u32,
    timeout: Duration,
) -> Result<Thumbnail> {
    let (tx, rx) = mpsc::channel();
    let ext = ext.to_owned();
    let handle = std::thread::Builder::new()
        .name("tw-decode".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let out = catch_unwind(AssertUnwindSafe(|| {
                decoder
                    .run(&data, &ext, edge)
                    .map(|img| to_thumbnail(&img, edge))
            }))
            .unwrap_or_else(|_| Err(DecodeError::malformed("decoder panicked")));
            let _ = tx.send(out);
        })
        .map_err(|e| DecodeError::malformed(format!("spawn decode thread: {e}")))?;
    match rx.recv_timeout(timeout) {
        Ok(out) => {
            let _ = handle.join();
            out
        }
        Err(_) => {
            pin_this_module();
            Err(DecodeError::Timeout)
        }
    }
}

/// Header-only [`probe`](crate::decode::probe::probe) under the same containment as
/// [`run_inproc`]: own thread, panic contained, budget enforced, module pinned under an
/// abandoned thread.
pub fn run_probe(
    data: Arc<Vec<u8>>,
    ext: Option<&str>,
    timeout: Duration,
) -> Result<crate::decode::probe::ImageInfo> {
    let (tx, rx) = mpsc::channel();
    let ext = ext.map(str::to_owned);
    let handle = std::thread::Builder::new()
        .name("tw-probe".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let out = catch_unwind(AssertUnwindSafe(|| {
                crate::decode::probe::probe(&data, ext.as_deref())
            }))
            .unwrap_or_else(|_| Err(DecodeError::malformed("probe panicked")));
            let _ = tx.send(out);
        })
        .map_err(|e| DecodeError::malformed(format!("spawn probe thread: {e}")))?;
    match rx.recv_timeout(timeout) {
        Ok(out) => {
            let _ = handle.join();
            out
        }
        Err(_) => {
            pin_this_module();
            Err(DecodeError::Timeout)
        }
    }
}
