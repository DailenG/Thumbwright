//! Read-only `IStream` over a borrowed byte slice, so the OS decoders (Media Foundation,
//! `Windows.Data.Pdf`) can read the in-memory file without a second copy of it (a video can
//! be gigabytes and the worker's memory limit only leaves room for one copy).
//!
//! The stream does not own the bytes. [`SliceStream::new`] hands back the COM object plus a
//! [`Detach`] guard: dropping the guard (or the stream's owner finishing) severs the
//! borrowed pointer under a lock, so a late read from an OS worker thread that still holds a
//! reference fails with an error instead of touching freed memory.

use std::ffi::c_void;
use std::sync::{Arc, Mutex, MutexGuard};

use windows::Win32::Foundation::{
    E_FAIL, E_INVALIDARG, E_NOTIMPL, E_POINTER, S_FALSE, S_OK, STG_E_ACCESSDENIED,
    STG_E_INVALIDFUNCTION,
};
use windows::Win32::System::Com::{
    ISequentialStream_Impl, IStream, IStream_Impl, LOCKTYPE, STATFLAG, STATSTG, STGC, STGTY_STREAM,
    STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET,
};
use windows::core::{HRESULT, Ref, implement};

/// Lifetime-erased view of the borrowed slice. Only dereferenced while the `Shared` lock is
/// held and the span is still attached.
#[derive(Clone, Copy)]
struct Span {
    ptr: *const u8,
    len: usize,
}

// SAFETY: the span is only read under the mutex, and `Detach` clears it before the
// borrowed slice's lifetime ends.
unsafe impl Send for Span {}

struct Shared(Mutex<Option<Span>>);

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Option<Span>> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Severs the stream from the borrowed bytes when dropped.
pub struct Detach(Arc<Shared>);

impl Drop for Detach {
    fn drop(&mut self) {
        *self.0.lock() = None;
    }
}

#[implement(IStream)]
struct SliceStream {
    shared: Arc<Shared>,
    pos: Mutex<u64>,
}

/// Wraps `data` as a read-only seekable `IStream`. Keep the returned guard alive for as long
/// as `data` must stay readable; drop it before `data` goes away.
pub fn slice_stream(data: &[u8]) -> (IStream, Detach) {
    let shared = Arc::new(Shared(Mutex::new(Some(Span {
        ptr: data.as_ptr(),
        len: data.len(),
    }))));
    let stream = SliceStream {
        shared: shared.clone(),
        pos: Mutex::new(0),
    };
    (stream.into(), Detach(shared))
}

impl SliceStream {
    fn pos(&self) -> MutexGuard<'_, u64> {
        self.pos.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl ISequentialStream_Impl for SliceStream_Impl {
    fn Read(&self, pv: *mut c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
        if pv.is_null() && cb != 0 {
            return E_POINTER;
        }
        let guard = self.shared.lock();
        let Some(span) = *guard else {
            return E_FAIL;
        };
        let mut pos = self.pos();
        let start = usize::try_from(*pos).unwrap_or(usize::MAX).min(span.len);
        let n = (span.len - start).min(cb as usize);
        if n > 0 {
            // SAFETY: `start + n <= span.len` and the span is attached while the lock is
            // held; the caller guarantees `pv` is writable for `cb >= n` bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(span.ptr.add(start), pv.cast::<u8>(), n);
            }
        }
        *pos = (start + n) as u64;
        if !pcbread.is_null() {
            // SAFETY: non-null out pointer supplied by the COM caller.
            unsafe { *pcbread = n as u32 };
        }
        if n < cb as usize { S_FALSE } else { S_OK }
    }

    fn Write(&self, _pv: *const c_void, _cb: u32, _pcbwritten: *mut u32) -> HRESULT {
        STG_E_ACCESSDENIED
    }
}

impl IStream_Impl for SliceStream_Impl {
    fn Seek(
        &self,
        dlibmove: i64,
        dworigin: STREAM_SEEK,
        plibnewposition: *mut u64,
    ) -> windows::core::Result<()> {
        let len = self.shared.lock().map_or(0, |s| s.len) as i64;
        let mut pos = self.pos();
        let base = match dworigin {
            STREAM_SEEK_SET => 0,
            STREAM_SEEK_CUR => i64::try_from(*pos).unwrap_or(i64::MAX),
            STREAM_SEEK_END => len,
            _ => return Err(STG_E_INVALIDFUNCTION.into()),
        };
        let new = base
            .checked_add(dlibmove)
            .filter(|n| *n >= 0)
            .ok_or(windows::core::Error::from(E_INVALIDARG))?;
        *pos = new as u64;
        if !plibnewposition.is_null() {
            // SAFETY: non-null out pointer supplied by the COM caller.
            unsafe { *plibnewposition = *pos };
        }
        Ok(())
    }

    fn SetSize(&self, _libnewsize: u64) -> windows::core::Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }

    fn CopyTo(
        &self,
        _pstm: Ref<IStream>,
        _cb: u64,
        _pcbread: *mut u64,
        _pcbwritten: *mut u64,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Commit(&self, _grfcommitflags: &STGC) -> windows::core::Result<()> {
        Ok(())
    }

    fn Revert(&self) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn LockRegion(&self, _off: u64, _cb: u64, _ty: &LOCKTYPE) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn UnlockRegion(&self, _off: u64, _cb: u64, _ty: u32) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn Stat(&self, pstatstg: *mut STATSTG, _grfstatflag: &STATFLAG) -> windows::core::Result<()> {
        if pstatstg.is_null() {
            return Err(E_POINTER.into());
        }
        let len = self.shared.lock().map_or(0, |s| s.len) as u64;
        let stat = STATSTG {
            r#type: STGTY_STREAM.0 as u32,
            cbSize: len,
            ..Default::default()
        };
        // SAFETY: non-null out pointer supplied by the COM caller; no name is returned, so
        // there is nothing to free.
        unsafe { pstatstg.write(stat) };
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IStream> {
        let copy = SliceStream {
            shared: self.shared.clone(),
            pos: Mutex::new(*self.pos()),
        };
        Ok(copy.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Com::STREAM_SEEK_SET;

    #[test]
    fn reads_seeks_and_stats() {
        let data: Vec<u8> = (0..100).collect();
        let (stream, _guard) = slice_stream(&data);
        let mut buf = [0u8; 10];
        let mut n = 0u32;
        // SAFETY: valid buffer and out pointer.
        unsafe {
            assert_eq!(stream.Read(buf.as_mut_ptr().cast(), 10, Some(&mut n)), S_OK);
        }
        assert_eq!((n, buf[0], buf[9]), (10, 0, 9));
        let mut at = 0u64;
        // SAFETY: valid out pointer.
        unsafe {
            stream.Seek(95, STREAM_SEEK_SET, Some(&mut at)).unwrap();
            assert_eq!(
                stream.Read(buf.as_mut_ptr().cast(), 10, Some(&mut n)),
                S_FALSE
            );
        }
        assert_eq!((at, n, buf[0], buf[4]), (95, 5, 95, 99));
        // SAFETY: valid out pointer.
        let end = unsafe {
            stream.Seek(0, STREAM_SEEK_END, Some(&mut at)).unwrap();
            at
        };
        assert_eq!(end, 100);
        // SAFETY: as above; a negative absolute position is rejected.
        assert!(unsafe { stream.Seek(-1, STREAM_SEEK_SET, None) }.is_err());
    }

    #[test]
    fn detached_stream_fails_instead_of_reading_freed_memory() {
        let data = vec![7u8; 16];
        let (stream, guard) = slice_stream(&data);
        drop(guard);
        drop(data);
        let mut buf = [0u8; 4];
        // SAFETY: valid buffer; the stream must not touch the freed slice.
        let hr = unsafe { stream.Read(buf.as_mut_ptr().cast(), 4, None) };
        assert_eq!(hr, E_FAIL);
        assert_eq!(buf, [0; 4]);
    }
}
