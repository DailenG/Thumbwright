//! Shared append-only diagnostics log at `%LOCALAPPDATA%\Thumbwright.log`
//! (FUNCTIONAL_SPEC.md 3.6). Each process writes a header line on its first entry.
//! Logging never fails the caller. Failures are always logged; with `VerboseLog=1` the
//! shell DLL also traces each successful request via [`verbose`].
//!
//! The log is capped: when a process opens it and it has grown past 1 MiB, everything but
//! the last 256 KiB is dropped first. Explorer asks for thumbnails constantly, so an
//! uncapped append-only file grows without bound on a machine that logs refusals.

use std::io::Write;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Instant;

static START: LazyLock<Instant> = LazyLock::new(Instant::now);
static COMPONENT: OnceLock<&'static str> = OnceLock::new();
static HEADER_WRITTEN: Mutex<bool> = Mutex::new(false);

/// Past this size the log is trimmed on the next open.
const TRIM_ABOVE: u64 = 1024 * 1024;
/// What is kept when the log is trimmed.
const KEEP: u64 = 256 * 1024;

/// Names this process in the log (`dll`, `worker`, `twctl`); first call wins.
pub fn init(component: &'static str) {
    let _ = COMPONENT.set(component);
    LazyLock::force(&START);
}

/// Drops all but the last `KEEP` bytes of `path`, best effort. Called with the log lock
/// held, so no interleaving write is lost (a concurrent process may lose its last lines;
/// acceptable for a diagnostics file that is about to be discarded anyway).
fn trim(path: &std::path::Path) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    let size = meta.len();
    if size <= TRIM_ABOVE {
        return;
    }
    let Ok(keep) = usize::try_from(KEEP) else {
        return;
    };
    let Ok(data) = std::fs::read(path) else {
        return;
    };
    let Some(start) = data.len().checked_sub(keep) else {
        return;
    };
    // Start at the beginning of a line.
    let start = start
        + data[start..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(0);
    let _ = std::fs::write(path, &data[start..]);
}

fn path() -> Option<std::path::PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| std::path::Path::new(&d).join("Thumbwright.log"))
}

fn write_line(level: &str, msg: &str) {
    let Some(path) = path() else { return };
    trim(&path);
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

/// A `VerboseLog=1` trace of a successful request. `line` runs only when `enabled`, so a
/// disabled log costs one branch and no formatting.
pub fn verbose(enabled: bool, line: impl FnOnce() -> String) {
    if enabled {
        write_line("TRACE", &line());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_keeps_only_the_tail_on_a_line_boundary() {
        let dir = std::env::temp_dir().join(format!("tw-log-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Thumbwright.log");
        let lines = 40_000; // ~1.4 MiB with a 36-byte line
        let mut body = String::new();
        for i in 0..lines {
            body.push_str(&format!("[pid 1 +0ms] ERROR line number {i:06}\n"));
        }
        std::fs::write(&path, &body).unwrap();
        trim(&path);
        let kept = std::fs::read_to_string(&path).unwrap();
        assert!(
            kept.len() <= KEEP as usize + 64,
            "kept {} bytes",
            kept.len()
        );
        assert!(
            kept.starts_with('['),
            "cut mid-line: {}",
            &kept[..kept.len().min(40)]
        );
        assert!(kept.contains(&format!("line number {:06}", lines - 1)));
        assert!(!kept.contains("line number 000000"));
        // Below the cap, nothing is touched.
        std::fs::write(&path, "short\n").unwrap();
        trim(&path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "short\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
