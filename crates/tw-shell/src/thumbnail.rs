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
use tw_core::pipeline::Engine;
use tw_core::{ids, limits, log};

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
        let outcome = catch_unwind(AssertUnwindSafe(|| produce(&stream, cx)))
            .unwrap_or_else(|_| Err(Failure::new("panic", None, 0)));
        match outcome {
            Ok(thumb) => {
                let hbmp = crate::bitmap::create(&thumb)?;
                // SAFETY: both out-pointers checked non-null above.
                unsafe {
                    *phbmp = hbmp;
                    *pdwalpha = WTSAT_ARGB;
                }
                Ok(())
            }
            Err(f) => {
                log::error(&format!(
                    "GetThumbnail: failed hr=0x{:08X} ext={} size={} cause={}",
                    E_FAIL.0 as u32,
                    f.ext.as_deref().unwrap_or("?"),
                    f.size,
                    f.cause
                ));
                // REQ-COM-7: a failure HRESULT, never a placeholder bitmap.
                Err(E_FAIL.into())
            }
        }
    }
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

struct StreamInfo {
    name: String,
    size: u64,
    mtime: u64,
}

fn stat(stream: &IStream) -> Result<StreamInfo> {
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

/// Reads the whole stream; `size` comes from Stat and bounds the allocation. A stream
/// that yields more than it declared is rejected.
fn read_all(stream: &IStream, size: u64) -> Result<Vec<u8>> {
    // SAFETY: seeking a stream we hold a reference to.
    unsafe { stream.Seek(0, STREAM_SEEK_SET, None)? };
    let mut buf = Vec::with_capacity(size as usize);
    let mut chunk = vec![0u8; 1 << 20];
    loop {
        let mut read = 0u32;
        // SAFETY: `chunk` is writable for `chunk.len()` bytes.
        let hr = unsafe {
            stream.Read(
                chunk.as_mut_ptr().cast(),
                chunk.len() as u32,
                Some(&mut read),
            )
        };
        hr.ok()?;
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read as usize]);
        if buf.len() as u64 > size {
            return Err(E_FAIL.into());
        }
    }
    Ok(buf)
}

fn produce(stream: &IStream, cx: u32) -> std::result::Result<Thumbnail, Failure> {
    let dir = crate::install_dir();
    let settings = Settings::load(&dir);
    let info = stat(stream).map_err(|e| Failure::new(format!("stat: {e}"), None, 0))?;
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
        .with_video_percent(settings.video_frame_percent);
    let edge = cx.clamp(1, settings.max_thumb_px);
    engine
        .thumbnail(Arc::new(data), ext.as_deref(), edge)
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
}
