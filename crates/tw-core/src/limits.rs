//! Timeout and resource budgets (FUNCTIONAL_SPEC.md section 3.5). Ceilings, not targets.

use std::time::Duration;

const MIB: u64 = 1024 * 1024;

/// Worker process committed-memory ceiling (Job Object per-process limit).
pub const WORKER_MEMORY_BYTES: u64 = 512 * MIB;
/// Largest single decode buffer any in-process decoder may allocate.
pub const DECODE_ALLOC_BYTES: u64 = 512 * MIB;
/// Worker elapsed-time budget for a thumbnail; the parent kills the worker after this.
pub const WORKER_THUMBNAIL_TIMEOUT: Duration = Duration::from_secs(120);
/// Worker elapsed-time budget for a conversion.
pub const WORKER_CONVERT_TIMEOUT: Duration = Duration::from_secs(600);
/// Worker user-mode CPU budget (Job Object per-process CPU time limit).
pub const WORKER_CPU_TIME: Duration = Duration::from_secs(20);
/// Largest decoded pixel area any tier may allocate.
pub const MAX_PIXEL_AREA: u64 = 512 * 1_000_000;
/// Largest frame count read from a multi-image container.
pub const MAX_FRAMES: u32 = 256;
/// Vector render budget (the SVG decoder runs in the worker; the parent kills it after this).
pub const VECTOR_RENDER_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest input read into memory for one thumbnail (the shell only has an IStream).
pub const MAX_INPUT_BYTES: u64 = 2 * 1024 * MIB;
/// Wall-clock budget for one in-process (memory-safe) decoder tier.
pub const INPROC_DECODE_TIMEOUT: Duration = Duration::from_secs(15);
/// Recursive-reference / pathological-structure guard for vector parsing.
pub const VECTOR_PARSE_TIMEOUT: Duration = Duration::from_secs(3);
/// Default "Max file size" setting (MB); checked from metadata before any read.
pub const DEFAULT_MAX_FILE_MB: u64 = 4096;
/// Default "Max thumbnail size" setting (px); hard ceiling on produced edge.
pub const DEFAULT_MAX_THUMB_EDGE: u32 = 1024;
/// Per-entry and total decompressed-size caps for archive/container parsers (REQ-SEC-1).
pub const ARCHIVE_ENTRY_MAX_BYTES: u64 = 256 * MIB;
pub const ARCHIVE_TOTAL_MAX_BYTES: u64 = 1024 * MIB;
