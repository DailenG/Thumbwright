//! Timeout and resource budgets (FUNCTIONAL_SPEC.md section 3.5). Ceilings, not targets.

use std::time::Duration;

const MIB: u64 = 1024 * 1024;

/// Worker process committed-memory ceiling (Job Object per-process limit).
pub const WORKER_MEMORY_BYTES: u64 = 512 * MIB;
/// Largest single decode buffer any in-process decoder may allocate.
pub const DECODE_ALLOC_BYTES: u64 = 512 * MIB;
/// Worker elapsed-time budget for a thumbnail; the parent kills the worker after this.
pub const WORKER_THUMBNAIL_TIMEOUT: Duration = Duration::from_secs(120);
/// Worker user-mode CPU budget (Job Object per-process CPU time limit).
pub const WORKER_CPU_TIME: Duration = Duration::from_secs(20);
/// Largest decoded pixel area any tier may allocate.
pub const MAX_PIXEL_AREA: u64 = 512 * 1_000_000;
/// Longest side of a decoded image any tier may declare, independent of the area: the resizer
/// allocates tables proportional to the long side (8 bytes per pixel), and a 500 000 000 x 1
/// image is within the area budget.
pub const MAX_IMAGE_AXIS: u32 = 1 << 20;
/// Pixel data the thumbnail conversion may hold beyond the decoded image itself (resizer
/// tables, output buffers): see `decode::Budget`.
pub const THUMBNAIL_SLACK_BYTES: u64 = 64 * MIB;
/// Vector render budget (the SVG decoder runs in the worker; the parent kills it after this).
pub const VECTOR_RENDER_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest input read into memory for one thumbnail (the shell only has an IStream).
pub const MAX_INPUT_BYTES: u64 = 2 * 1024 * MIB;
/// Wall-clock budget for one in-process (memory-safe) decoder tier.
pub const INPROC_DECODE_TIMEOUT: Duration = Duration::from_secs(15);
/// In-process decode threads that overran their budget and are still running, tolerated per
/// process. Each is a core the process cannot get back and a reason its module can never be
/// unloaded; beyond this, in-process work fails fast (see `pipeline::run_inproc`).
pub const MAX_ABANDONED_DECODE_THREADS: usize = 2;
/// Recursive-reference / pathological-structure guard for vector parsing.
pub const VECTOR_PARSE_TIMEOUT: Duration = Duration::from_secs(3);
/// Default "Max file size" setting (MB); checked from metadata before any read.
pub const DEFAULT_MAX_FILE_MB: u64 = 4096;
/// Default "Max thumbnail size" setting (px); hard ceiling on produced edge.
pub const DEFAULT_MAX_THUMB_EDGE: u32 = 1024;
/// Per-entry and total decompressed-size caps for archive/container parsers (REQ-SEC-1).
pub const ARCHIVE_ENTRY_MAX_BYTES: u64 = 256 * MIB;
pub const ARCHIVE_TOTAL_MAX_BYTES: u64 = 1024 * MIB;
/// Embedded image payloads one container decoder (DWG, ICNS, XMP) may try before giving up.
/// Every failed attempt can cost a full-size decode, and a hostile directory can list the
/// same bomb many times; a real file's first or second candidate decodes.
pub const MAX_PAYLOAD_ATTEMPTS: usize = 3;
/// Worker wall-clock budget for a video frame extraction (Media Foundation).
pub const VIDEO_FRAME_TIMEOUT: Duration = Duration::from_secs(30);
/// Worker wall-clock budget for rendering the first page of a PDF.
pub const PDF_RENDER_TIMEOUT: Duration = Duration::from_secs(30);
/// Worker wall-clock budget for playing back a Windows metafile.
pub const METAFILE_RENDER_TIMEOUT: Duration = Duration::from_secs(30);
/// Default video frame offset (percent of the duration).
pub const DEFAULT_VIDEO_PERCENT: u8 = 30;
/// Extra committed memory a video worker may use on top of the input: 4K H.264/HEVC decoding
/// measured about 0.9 GiB beyond the file (decoder surfaces plus RGB32 conversion buffers),
/// which the fixed 512 MiB worker ceiling cannot hold.
pub const WORKER_VIDEO_EXTRA_MEMORY_BYTES: u64 = 1536 * MIB;
