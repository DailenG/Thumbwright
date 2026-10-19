//! Thumbnail provider: `IThumbnailProvider` + `IInitializeWithStream` only (REQ-COM-1),
//! so Windows hosts it in the isolated thumbnail surrogate (REQ-ISO-1).

use std::cell::RefCell;
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{E_FAIL, E_POINTER, E_UNEXPECTED, ERROR_ALREADY_INITIALIZED};
use windows::Win32::Graphics::Gdi::HBITMAP;
use windows::Win32::System::Com::{
    CoTaskMemFree, IStream, STATFLAG_DEFAULT, STATSTG, STREAM_SEEK_SET,
};
use windows::Win32::UI::Shell::PropertiesSystem::{
    IInitializeWithStream, IInitializeWithStream_Impl,
};
use windows::Win32::UI::Shell::{
    IThumbnailProvider, IThumbnailProvider_Impl, WTS_ALPHATYPE, WTSAT_ARGB,
};
use windows::core::HRESULT;
use windows::core::{Ref, Result, implement};

use tw_core::config::Settings;
use tw_core::decode::{DecodeError, Thumbnail};
use tw_core::pipeline::{Engine, Trace};
use tw_core::{ids, limits, log, worker};

use crate::ModuleRef;

type FailKey = (String, u64, u64);

/// Negative cache keyed by (stream name, size, mtime) for the life of the surrogate
/// (REQ-TMO-3), so Explorer's multi-size requests don't repeat an expensive failure.
/// The value is the expiry: `None` is permanent; timeouts expire after
/// [`TIMEOUT_CACHE_TTL`] since they may be load-dependent, yet caching them at all stops
/// each retry from starting another runaway decode thread.
static FAILED: LazyLock<Mutex<HashMap<FailKey, Option<Instant>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const FAILED_CAP: usize = 4096;
const TIMEOUT_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// The expiry to record for a failure observed at `now`.
fn failure_expiry(e: &DecodeError, now: Instant) -> Option<Instant> {
    matches!(e, DecodeError::Timeout).then(|| now + TIMEOUT_CACHE_TTL)
}

/// Whether a cached entry with `expiry` still suppresses a retry at `now`.
fn still_failed(expiry: Option<Instant>, now: Instant) -> bool {
    expiry.is_none_or(|t| now < t)
}

#[implement(IThumbnailProvider, IInitializeWithStream)]
pub(crate) struct ThumbnailProvider {
    stream: RefCell<Option<IStream>>,
    _module: ModuleRef,
}

impl ThumbnailProvider {
    pub(crate) fn new() -> Self {
        log::init("dll");
        // Worker start-up dominates small SVG/audio/WIC requests; see docs/performance.md.
        worker::set_prespawn(true);
        Self {
            stream: RefCell::new(None),
            _module: ModuleRef::new(),
        }
    }
}

impl IInitializeWithStream_Impl for ThumbnailProvider_Impl {
    fn Initialize(&self, stream: Ref<IStream>, _mode: u32) -> Result<()> {
        let stream = stream.ok()?.clone();
        let mut slot = self.stream.borrow_mut();
        if slot.is_some() {
            return Err(HRESULT::from_win32(ERROR_ALREADY_INITIALIZED.0).into());
        }
        *slot = Some(stream);
        Ok(())
    }
}

impl IThumbnailProvider_Impl for ThumbnailProvider_Impl {
    fn GetThumbnail(
        &self,
        cx: u32,
        phbmp: *mut HBITMAP,
        pdwalpha: *mut WTS_ALPHATYPE,
    ) -> Result<()> {
        if phbmp.is_null() || pdwalpha.is_null() {
            return Err(E_POINTER.into());
        }
        let stream = self.stream.borrow().clone().ok_or(E_UNEXPECTED)?;
        write_thumbnail(
            &stream,
            None,
            cx,
            phbmp,
            pdwalpha,
            ("thumbnail", Instant::now()),
        )
    }
}

/// Produces the thumbnail for `stream` and hands it out per the `IThumbnailProvider`
/// contract (REQ-COM-6/7). `name_hint` supplies the file name for streams whose `Stat`
/// reports none. `request` is the diagnostics-log class and the instant the request began,
/// for the `VerboseLog` success line.
///
/// `phbmp` and `pdwalpha` must be non-null; callers check.
pub(crate) fn write_thumbnail(
    stream: &IStream,
    name_hint: Option<&str>,
    cx: u32,
    phbmp: *mut HBITMAP,
    pdwalpha: *mut WTS_ALPHATYPE,
    request: (&'static str, Instant),
) -> Result<()> {
    let outcome = catch_unwind(AssertUnwindSafe(|| produce(stream, name_hint, cx)))
        .unwrap_or_else(|_| Err(Failure::new("panic", None, 0)));
    match outcome {
        Ok(done) => {
            let hbmp = crate::bitmap::create(&done.thumb)?;
            // SAFETY: the caller guarantees both out-pointers are non-null.
            unsafe {
                *phbmp = hbmp;
                *pdwalpha = WTSAT_ARGB;
            }
            let (class, started) = request;
            log::verbose(done.verbose, || {
                format!(
                    "GetThumbnail: ok class={class} ext={} size={} req={cx} {} ms={} out={}x{}",
                    done.ext.as_deref().unwrap_or("?"),
                    done.size,
                    done.trace,
                    started.elapsed().as_millis(),
                    done.thumb.width,
                    done.thumb.height
                )
            });
            Ok(())
        }
        Err(f) => {
            // Settings-driven refusals are expected behaviour, not errors: a user who turns
            // thumbnails off would otherwise generate one ERROR line per file per view.
            // Everything else (decode failure, read failure) stays an error.
            let level = if f.cause == "thumbnails disabled"
                || f.cause == "type disabled"
                || f.cause == "cached failure"
            {
                "INFO"
            } else {
                "ERROR"
            };
            if level == "ERROR" {
                log::error(&format!(
                    "GetThumbnail: failed hr=0x{:08X} ext={} size={} cause={}",
                    E_FAIL.0 as u32,
                    f.ext.as_deref().unwrap_or("?"),
                    f.size,
                    f.cause
                ));
            } else {
                log::info(&format!(
                    "GetThumbnail: refused ext={} size={} cause={}",
                    f.ext.as_deref().unwrap_or("?"),
                    f.size,
                    f.cause
                ));
            }
            // REQ-COM-7: a failure HRESULT, never a placeholder bitmap.
            Err(E_FAIL.into())
        }
    }
}

/// A successful decode and what the `VerboseLog` line reports about it.
struct Produced {
    thumb: Thumbnail,
    trace: Trace,
    ext: Option<String>,
    size: u64,
    verbose: bool,
}

struct Failure {
    cause: String,
    ext: Option<String>,
    size: u64,
}

impl Failure {
    fn new(cause: impl Into<String>, ext: Option<&str>, size: u64) -> Self {
        Self {
            cause: cause.into(),
            ext: ext.map(str::to_owned),
            size,
        }
    }
}

pub(crate) struct StreamInfo {
    pub(crate) name: String,
    pub(crate) size: u64,
    pub(crate) mtime: u64,
}

pub(crate) fn stat(stream: &IStream) -> Result<StreamInfo> {
    let mut st = STATSTG::default();
    // SAFETY: `st` is a valid out-parameter; pwcsName is CoTaskMem-allocated and freed here.
    unsafe {
        stream.Stat(&mut st, STATFLAG_DEFAULT)?;
        let name = if st.pwcsName.is_null() {
            String::new()
        } else {
            let n = st.pwcsName.to_string().unwrap_or_default();
            CoTaskMemFree(Some(st.pwcsName.0 as *const _));
            n
        };
        Ok(StreamInfo {
            name,
            size: st.cbSize,
            mtime: (u64::from(st.mtime.dwHighDateTime) << 32) | u64::from(st.mtime.dwLowDateTime),
        })
    }
}

/// Reads the whole stream straight into the result; `size` comes from Stat and bounds the
/// allocation. A stream that yields more than it declared is rejected. (A 1 MiB bounce
/// buffer cost 0.4-0.7 ms per call, ten times the read itself on small files:
/// docs/performance.md.)
pub(crate) fn read_all(stream: &IStream, size: u64) -> Result<Vec<u8>> {
    // SAFETY: seeking a stream we hold a reference to.
    unsafe { stream.Seek(0, STREAM_SEEK_SET, None)? };
    // One byte beyond the declared size, so a stream that yields more is noticed.
    let cap = usize::try_from(size)
        .ok()
        .and_then(|n| n.checked_add(1))
        .ok_or(E_FAIL)?;
    let mut buf: Vec<u8> = Vec::with_capacity(cap);
    loop {
        // `buf.len() <= size` here, so there is always room for at least one byte.
        let want = (cap - buf.len()).min(1 << 20) as u32;
        let mut read = 0u32;
        // SAFETY: the stream writes at most `want` bytes into the vector's spare capacity,
        // which holds `cap - buf.len() >= want` bytes.
        let hr = unsafe {
            stream.Read(
                buf.as_mut_ptr().add(buf.len()).cast(),
                want,
                Some(&mut read),
            )
        };
        hr.ok()?;
        if read == 0 {
            break;
        }
        if read > want {
            return Err(E_FAIL.into());
        }
        // SAFETY: the stream initialised the first `read <= want` spare bytes.
        unsafe { buf.set_len(buf.len() + read as usize) };
        if buf.len() as u64 > size {
            return Err(E_FAIL.into());
        }
    }
    Ok(buf)
}

fn produce(
    stream: &IStream,
    name_hint: Option<&str>,
    cx: u32,
) -> std::result::Result<Produced, Failure> {
    let dir = crate::install_dir();
    let settings = Settings::load(&dir);
    let mut info = stat(stream).map_err(|e| Failure::new(format!("stat: {e}"), None, 0))?;
    if info.name.is_empty()
        && let Some(hint) = name_hint
    {
        info.name = hint.to_owned();
    }
    let ext = Path::new(&info.name)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase());
    let fail = |cause: &str| Failure::new(cause, ext.as_deref(), info.size);

    if !settings.thumbnails_enabled {
        return Err(fail("thumbnails disabled"));
    }
    if ext.as_deref().is_some_and(|e| !settings.type_enabled(e)) {
        return Err(fail("type disabled"));
    }
    // REQ-TMO-4: size gate from metadata, before any read.
    if info.size > settings.max_file_bytes() || info.size > limits::MAX_INPUT_BYTES {
        return Err(fail("file exceeds max file size"));
    }
    let key = (info.name.clone(), info.size, info.mtime);
    if !info.name.is_empty()
        && FAILED.lock().is_ok_and(|f| {
            f.get(&key)
                .is_some_and(|&t| still_failed(t, Instant::now()))
        })
    {
        return Err(fail("cached failure"));
    }

    let data = read_all(stream, info.size).map_err(|e| fail(&format!("read: {e}")))?;
    let engine = Engine::new(dir.join(ids::WORKER_EXE_NAME))
        .with_video_percent(settings.video_frame_percent)
        .with_prefer_embedded(settings.use_embedded_thumbs);
    let edge = cx.clamp(1, settings.max_thumb_px);
    let (thumb, trace) = engine
        .thumbnail_traced(Arc::new(data), ext.as_deref(), edge)
        .map_err(|e: DecodeError| {
            if !info.name.is_empty()
                && let Ok(mut f) = FAILED.lock()
            {
                if f.len() >= FAILED_CAP {
                    f.clear();
                }
                f.insert(key.clone(), failure_expiry(&e, Instant::now()));
            }
            fail(&e.to_string())
        })?;
    Ok(Produced {
        thumb,
        trace,
        ext: ext.clone(),
        size: info.size,
        verbose: settings.verbose_log,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeouts_are_not_negatively_cached_forever() {
        let now = Instant::now();
        let t = failure_expiry(&DecodeError::Timeout, now);
        assert!(still_failed(t, now));
        assert!(!still_failed(t, now + TIMEOUT_CACHE_TTL));
        let m = failure_expiry(&DecodeError::TooLarge, now);
        assert!(still_failed(m, now + Duration::from_secs(86_400)));
    }

    fn mem_stream(data: &[u8]) -> IStream {
        // SAFETY: the slice is valid for the call; the stream copies it.
        unsafe { windows::Win32::UI::Shell::SHCreateMemStream(Some(data)) }.unwrap()
    }

    fn pattern(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 + i / 251) as u8).collect()
    }

    #[test]
    fn read_all_returns_every_byte_across_read_chunk_boundaries() {
        const MIB: usize = 1 << 20;
        for n in [0, 1, MIB - 1, MIB, MIB + 1, 2 * MIB + 5] {
            let data = pattern(n);
            let got = read_all(&mem_stream(&data), n as u64).unwrap();
            assert!(got == data, "{n} bytes differ");
        }
    }

    #[test]
    fn read_all_rejects_a_stream_longer_than_it_declared() {
        let data = pattern(3000);
        assert!(read_all(&mem_stream(&data), 2999).is_err());
        // Exactly one byte over the declared size, at a chunk boundary.
        let data = pattern(1 << 20);
        assert!(read_all(&mem_stream(&data), (1 << 20) - 1).is_err());
        // A stream shorter than declared is read to its end.
        let data = pattern(100);
        assert!(read_all(&mem_stream(&data), 5000).unwrap() == data);
    }

    #[test]
    fn read_all_restarts_from_the_beginning() {
        let data = pattern(500);
        let s = mem_stream(&data);
        assert!(read_all(&s, 500).unwrap() == data);
        assert!(read_all(&s, 500).unwrap() == data);
    }
}
