//! Isolated decoder process. The shell DLL spawns one worker per request inside a Job
//! Object (memory, CPU-time, single-process and kill-on-close limits) and kills it on
//! timeout, so a decoder fault or hang never reaches the thumbnail surrogate or Explorer.
//!
//! Protocol: one request on stdin, one response on stdout (`tw_core::protocol`). The
//! worker performs no file-system or network access of its own.
#![cfg_attr(not(test), windows_subsystem = "windows")]

use std::io::{BufReader, BufWriter, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::ExitCode;

use windows::Win32::System::Com::{COINIT_DISABLE_OLE1DDE, COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::Diagnostics::Debug::{
    SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SEM_NOOPENFILEERRORBOX, SetErrorMode,
};

use tw_core::decode::{DecodeError, to_thumbnail};
use tw_core::{limits, log, protocol};

fn main() -> ExitCode {
    if matches!(std::env::args().nth(1).as_deref(), Some("--version" | "-V")) {
        println!("tw-worker {}", tw_core::ids::VERSION);
        return ExitCode::SUCCESS;
    }
    log::init("worker");
    // SAFETY: process-wide error-mode and COM initialisation at startup.
    unsafe {
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX);
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED | COINIT_DISABLE_OLE1DDE);
    }

    let request = match protocol::read_request(
        &mut BufReader::new(std::io::stdin().lock()),
        limits::MAX_INPUT_BYTES,
    ) {
        Ok(r) => r,
        Err(e) => {
            log::error(&format!("worker: bad request: {e}"));
            return ExitCode::FAILURE;
        }
    };
    let edge = request.edge.max(1);
    let result = catch_unwind(AssertUnwindSafe(|| {
        request
            .decoder
            .run_with(&request.data, &request.ext, edge, request.param)
            .map(|img| to_thumbnail(&img, edge))
    }))
    .unwrap_or_else(|_| Err(DecodeError::malformed("decoder panicked")));

    let mut out = BufWriter::new(std::io::stdout().lock());
    match protocol::write_response(&mut out, &result).and_then(|()| out.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}
