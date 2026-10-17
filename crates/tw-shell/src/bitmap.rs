//! Thumbnail -> top-down 32-bpp premultiplied BGRA DIB section (REQ-COM-6).

use std::ffi::c_void;

use windows::Win32::Foundation::E_OUTOFMEMORY;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateDIBSection, DIB_RGB_COLORS, HBITMAP,
};
use windows::core::Result;

use tw_core::decode::Thumbnail;

pub(crate) fn create(t: &Thumbnail) -> Result<HBITMAP> {
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: t.width as i32,
            // Negative height = top-down rows.
            biHeight: -(t.height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    // SAFETY: `info` describes a 32-bpp DIB; `bits` receives the section's pixel pointer.
    let hbmp = unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)? };
    if bits.is_null() {
        return Err(E_OUTOFMEMORY.into());
    }
    // SAFETY: a w*h 32-bpp DIB section has exactly w*h*4 bytes with no row padding.
    unsafe {
        std::ptr::copy_nonoverlapping(t.bgra.as_ptr(), bits.cast::<u8>(), t.bgra.len());
    }
    Ok(hbmp)
}
