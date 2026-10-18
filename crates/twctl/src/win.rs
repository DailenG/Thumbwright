//! Windows integration for the menu verbs: clipboard, wallpaper, message boxes.

use std::path::Path;

use image::DynamicImage;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::Graphics::Gdi::{BI_BITFIELDS, BITMAPV5HEADER};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::UI::Shell::{
    DESKTOP_WALLPAPER_POSITION, DWPOS_CENTER, DWPOS_FILL, DWPOS_FIT, DWPOS_SPAN, DWPOS_STRETCH,
    DWPOS_TILE, DesktopWallpaper, IDesktopWallpaper,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, HWND_MESSAGE, MB_ICONERROR, MB_ICONINFORMATION, MB_OK,
    MessageBoxW, WINDOW_EX_STYLE, WINDOW_STYLE,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::ops::Result;

const CF_DIBV5: u32 = 17;
const CF_UNICODETEXT: u32 = 13;
const LCS_SRGB: u32 = 0x7352_4742;

/// Absolute path in the plain `C:\...` form the shell APIs want (no `\\?\` prefix, no
/// forward slashes, no relative parts); the input is returned unchanged when it cannot be
/// resolved.
pub fn shell_path(path: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).map_or_else(
        |_| path.to_path_buf(),
        |p| {
            let s = p.to_string_lossy();
            std::path::PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s).to_owned())
        },
    )
}

pub fn message(text: &str, error: bool) {
    let style = MB_OK
        | if error {
            MB_ICONERROR
        } else {
            MB_ICONINFORMATION
        };
    // SAFETY: owner-less message box with valid strings.
    unsafe { MessageBoxW(None, &HSTRING::from(text), w!("Thumbwright"), style) };
}

fn global_from(bytes: &[u8]) -> Result<HGLOBAL> {
    // SAFETY: allocate, copy, unlock; ownership passes to the clipboard on success.
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)).map_err(|e| e.to_string())?;
        let p = GlobalLock(h);
        if p.is_null() {
            let _ = GlobalFree(Some(h));
            return Err("GlobalLock failed".into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast::<u8>(), bytes.len());
        let _ = GlobalUnlock(h);
        Ok(h)
    }
}

/// Places `items` (format, bytes) on the clipboard, replacing its contents. A message-only
/// window owns the clipboard while it is open, so EmptyClipboard has an owner to assign.
fn set_clipboard(items: &[(u32, Vec<u8>)]) -> Result<()> {
    // SAFETY: a message-only STATIC window with no parameters; destroyed below.
    let owner = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            PCWSTR::null(),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )
    }
    .map_err(|e| format!("clipboard owner window: {e}"))?;
    // SAFETY: standard open/empty/set/close sequence; handles given to SetClipboardData
    // belong to the system afterwards, the rest are freed.
    let result = unsafe {
        let mut opened = false;
        for _ in 0..10 {
            if OpenClipboard(Some(owner)).is_ok() {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if opened {
            let result = (|| {
                EmptyClipboard().map_err(|e| e.to_string())?;
                for (fmt, bytes) in items {
                    let h = global_from(bytes)?;
                    if SetClipboardData(*fmt, Some(HANDLE(h.0))).is_err() {
                        let _ = GlobalFree(Some(h));
                        return Err(format!("SetClipboardData({fmt}) failed"));
                    }
                }
                Ok(())
            })();
            let _ = CloseClipboard();
            result
        } else {
            Err("clipboard is busy".into())
        }
    };
    // SAFETY: `owner` was created above on this thread and the clipboard is closed.
    let _ = unsafe { DestroyWindow(owner) };
    result
}

/// 32-bpp CF_DIBV5 (bottom-up, straight alpha) plus a registered "PNG" format.
pub fn copy_image(img: &DynamicImage) -> Result<()> {
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let header = BITMAPV5HEADER {
        bV5Size: size_of::<BITMAPV5HEADER>() as u32,
        bV5Width: w as i32,
        bV5Height: h as i32,
        bV5Planes: 1,
        bV5BitCount: 32,
        bV5Compression: BI_BITFIELDS,
        bV5SizeImage: w * h * 4,
        bV5RedMask: 0x00FF_0000,
        bV5GreenMask: 0x0000_FF00,
        bV5BlueMask: 0x0000_00FF,
        bV5AlphaMask: 0xFF00_0000,
        bV5CSType: LCS_SRGB,
        ..Default::default()
    };
    let mut dib = Vec::with_capacity(size_of::<BITMAPV5HEADER>() + rgba.len());
    // SAFETY: BITMAPV5HEADER is plain old data.
    dib.extend_from_slice(unsafe {
        std::slice::from_raw_parts(
            (&raw const header).cast::<u8>(),
            size_of::<BITMAPV5HEADER>(),
        )
    });
    for row in rgba.rows().rev() {
        for px in row {
            dib.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
    }
    let png = crate::ops::encode(img, "png", None)?;
    // SAFETY: registering a named clipboard format.
    let png_fmt = unsafe { RegisterClipboardFormatW(w!("PNG")) };
    set_clipboard(&[(CF_DIBV5, dib), (png_fmt, png)])
}

pub fn copy_text(text: &str) -> Result<()> {
    let utf16: Vec<u8> = text
        .encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect();
    set_clipboard(&[(CF_UNICODETEXT, utf16)])
}

pub fn wallpaper_position(style: &str) -> Result<DESKTOP_WALLPAPER_POSITION> {
    Ok(match style {
        "center" => DWPOS_CENTER,
        "tile" => DWPOS_TILE,
        "stretch" => DWPOS_STRETCH,
        "fit" => DWPOS_FIT,
        "fill" => DWPOS_FILL,
        "span" => DWPOS_SPAN,
        _ => {
            return Err(format!(
                "bad --style {style} (stretch|tile|center|fill|fit|span)"
            ));
        }
    })
}

/// Applies `image_path` to all monitors with `position`.
pub fn set_wallpaper(image_path: &Path, position: DESKTOP_WALLPAPER_POSITION) -> Result<()> {
    // SAFETY: COM on this thread for the shell's DesktopWallpaper object.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dw: IDesktopWallpaper =
            CoCreateInstance(&DesktopWallpaper, None, CLSCTX_ALL).map_err(|e| e.to_string())?;
        dw.SetPosition(position).map_err(|e| e.to_string())?;
        dw.SetWallpaper(PCWSTR::null(), &HSTRING::from(image_path.as_os_str()))
            .map_err(|e| e.to_string())
    }
}
