//! Shared append-only diagnostics log at `%LOCALAPPDATA%\Thumbwright.log`
//! (FUNCTIONAL_SPEC.md 3.6). Each process writes a header line on its first entry.
//! Logging never fails the caller.

use std::io::Write;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Instant;

static START: LazyLock<Instant> = LazyLock::new(Instant::now);
static COMPONENT: OnceLock<&'static str> = OnceLock::new();
static HEADER_WRITTEN: Mutex<bool> = Mutex::new(false);

/// Names this process in the log (`dll`, `worker`, `twctl`); first call wins.
pub fn init(component: &'static str) {
    let _ = COMPONENT.set(component);
    LazyLock::force(&START);
}

fn path() -> Option<std::path::PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| std::path::Path::new(&d).join("Thumbwright.log"))
}

fn write_line(level: &str, msg: &str) {
    let Some(path) = path() else { return };
    let Ok(mut header) = HEADER_WRITTEN.lock() else {
        return;
    };
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let pid = std::process::id();
    let ms = START.elapsed().as_millis();
    if !*header {
        let _ = writeln!(
            f,
            "[pid {pid} +0ms] ==== {} {} [{}] ====",
            crate::ids::PRODUCT_NAME,
            crate::ids::VERSION,
            COMPONENT.get().copied().unwrap_or("unknown")
        );
        *header = true;
    }
    let _ = writeln!(f, "[pid {pid} +{ms}ms] {level} {msg}");
}

pub fn error(msg: &str) {
    write_line("ERROR", msg);
}

pub fn info(msg: &str) {
    write_line("INFO", msg);
}
