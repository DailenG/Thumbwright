//! Cloud-folder thumbnail provider: `IThumbnailProvider` + `IInitializeWithItem` only
//! (REQ-COM-2). Windows asks only the sync root's registered provider for thumbnails in a
//! cloud folder, and hands it a shell item rather than a stream. COM activates the class
//! out of process through its AppID's empty `DllSurrogate` (see `registration`).
//!
//! The class is off until the "cloud folders" setting is on (spec 4.3) and never reads a
//! file a sync provider marks online-only (REQ-ISO-5): opening one would make the provider
//! download it. Decoding then goes through the same pipeline as the stream class.

use std::cell::RefCell;

use windows::Win32::Foundation::{E_FAIL, E_POINTER, E_UNEXPECTED, ERROR_ALREADY_INITIALIZED};
use windows::Win32::Graphics::Gdi::HBITMAP;
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
    GetFileAttributesW, INVALID_FILE_ATTRIBUTES,
};
use windows::Win32::System::Com::{CoTaskMemFree, IStream, STGM_READ};
use windows::Win32::UI::Shell::{
    IInitializeWithItem, IInitializeWithItem_Impl, IShellItem, IThumbnailProvider,
    IThumbnailProvider_Impl, SHCreateStreamOnFileEx, SIGDN_FILESYSPATH, WTS_ALPHATYPE,
};
use windows::core::{HRESULT, HSTRING, Ref, Result, implement};

use tw_core::config::Settings;
use tw_core::log;

use crate::ModuleRef;

/// Attributes a sync provider sets on a placeholder whose content is not on disk.
const ONLINE_ONLY: u32 = FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS.0
    | FILE_ATTRIBUTE_RECALL_ON_OPEN.0
    | FILE_ATTRIBUTE_OFFLINE.0;

/// REQ-ISO-5: whether reading a file with these attributes would fetch it from the cloud.
pub(crate) fn is_online_only(attributes: u32) -> bool {
    attributes & ONLINE_ONLY != 0
}

#[implement(IThumbnailProvider, IInitializeWithItem)]
pub(crate) struct CloudThumbnailProvider {
    item: RefCell<Option<IShellItem>>,
    _module: ModuleRef,
}

impl CloudThumbnailProvider {
    pub(crate) fn new() -> Self {
        log::init("dll");
        Self {
            item: RefCell::new(None),
            _module: ModuleRef::new(),
        }
    }
}

impl IInitializeWithItem_Impl for CloudThumbnailProvider_Impl {
    fn Initialize(&self, item: Ref<IShellItem>, _mode: u32) -> Result<()> {
        let item = item.ok()?.clone();
        let mut slot = self.item.borrow_mut();
        if slot.is_some() {
            return Err(HRESULT::from_win32(ERROR_ALREADY_INITIALIZED.0).into());
        }
        *slot = Some(item);
        Ok(())
    }
}

/// The item's file-system path.
fn fs_path(item: &IShellItem) -> Result<String> {
    // SAFETY: the returned PWSTR is CoTaskMem-allocated; copied, then freed.
    unsafe {
        let p = item.GetDisplayName(SIGDN_FILESYSPATH)?;
        let s = p.to_string();
        CoTaskMemFree(Some(p.0 as *const _));
        s.map_err(|_| E_FAIL.into())
    }
}

/// Attributes of the file at `path`, read from its directory entry only: this never opens
/// the file, so it cannot trigger a recall.
fn attributes(path: &str) -> Result<u32> {
    // SAFETY: plain query on a NUL-terminated path.
    let a = unsafe { GetFileAttributesW(&HSTRING::from(path)) };
    if a == INVALID_FILE_ATTRIBUTES {
        Err(windows::core::Error::from_thread())
    } else {
        Ok(a)
    }
}

fn open(item: &IShellItem, cx: u32) -> Result<(IStream, String)> {
    if !Settings::load(&crate::install_dir()).cloud_thumbnails {
        log::info(&format!("cloud GetThumbnail({cx}): setting is off"));
        return Err(E_FAIL.into());
    }
    let path = fs_path(item)?;
    let attrs = attributes(&path)?;
    if is_online_only(attrs) {
        log::info(&format!(
            "cloud GetThumbnail: skipped online-only file {path} (attributes 0x{attrs:08X})"
        ));
        return Err(E_FAIL.into());
    }
    // SAFETY: opens the file we just verified is local, read-only.
    let stream = unsafe {
        SHCreateStreamOnFileEx(&HSTRING::from(path.as_str()), STGM_READ.0, 0, false, None)?
    };
    Ok((stream, path))
}

impl IThumbnailProvider_Impl for CloudThumbnailProvider_Impl {
    fn GetThumbnail(
        &self,
        cx: u32,
        phbmp: *mut HBITMAP,
        pdwalpha: *mut WTS_ALPHATYPE,
    ) -> Result<()> {
        if phbmp.is_null() || pdwalpha.is_null() {
            return Err(E_POINTER.into());
        }
        let item = self.item.borrow().clone().ok_or(E_UNEXPECTED)?;
        // REQ-COM-7: every refusal is a failure HRESULT, never a placeholder.
        let (stream, path) = open(&item, cx).map_err(|e| {
            if e.code() != E_FAIL {
                log::error(&format!("cloud GetThumbnail: cannot open item: {e}"));
            }
            windows::core::Error::from(E_FAIL)
        })?;
        crate::thumbnail::write_thumbnail(&stream, Some(&path), cx, phbmp, pdwalpha)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn online_only_attributes_are_refused() {
        for attr in [
            FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS.0,
            FILE_ATTRIBUTE_RECALL_ON_OPEN.0,
            FILE_ATTRIBUTE_OFFLINE.0,
            // Combined with ordinary attributes, as providers set them.
            FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS.0 | 0x20 | 0x400,
        ] {
            assert!(is_online_only(attr), "0x{attr:X}");
        }
    }

    #[test]
    fn local_files_are_readable() {
        // ARCHIVE, NORMAL, READONLY, HIDDEN, SYSTEM, REPARSE_POINT, SPARSE_FILE, PINNED,
        // UNPINNED: none of these means "content is in the cloud".
        for attr in [
            0x20u32,
            0x80,
            0x01,
            0x02,
            0x04,
            0x400,
            0x200,
            0x0008_0000,
            0x0010_0000,
        ] {
            assert!(!is_online_only(attr), "0x{attr:X}");
        }
    }
}
