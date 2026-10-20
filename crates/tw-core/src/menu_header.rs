//! The preview header of Thumbwright's menus: the file's thumbnail with its name and size to the
//! right, as one row shared by the classic submenu and the `twctl menu` popup.
//!
//! A native menu sizes one shared bitmap column for every row from the widest item bitmap, so a
//! thumbnail attached to the text row (`MIIM_BITMAP`) pushes the text of every other row past
//! the thumbnail. An owner-drawn row avoids that but switches the whole menu to the classic,
//! non-themed look. So the header is a bitmap item (`MFT_BITMAP`): a row whose whole content is
//! one bitmap, in the item area and not in the bitmap column, and the menu stays themed. The
//! bitmap is composed here: the thumbnail and the text, in the menu font and colours, in a
//! premultiplied 32-bpp DIB that the menu blends over its own background.
//!
//! A bitmap item has no text, so the row gets its accessible name from an MSAA dynamic
//! annotation (`IAccPropServices::SetHmenuPropStr`, `PROPID_ACC_NAME`), which oleacc reads in
//! the menu's own process; a header without a thumbnail stays an ordinary text row.

use std::cell::Cell;

use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY, AlphaBlend, BI_RGB, BITMAP, BITMAPINFO,
    BITMAPINFOHEADER, BLENDFUNCTION, COLOR_MENUTEXT, CreateCompatibleDC, CreateDIBSection,
    CreateFontIndirectW, DIB_RGB_COLORS, DT_CALCRECT, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_TOP,
    DeleteDC, DeleteObject, DrawTextW, GdiFlush, GetDC, GetObjectW, GetSysColor, HBITMAP, HDC,
    HFONT, HGDIOBJ, ReleaseDC, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize,
};
use windows::Win32::UI::Accessibility::{CLSID_AccPropServices, IAccPropServices, PROPID_ACC_NAME};
use windows::Win32::UI::HiDpi::{GetDpiForSystem, SystemParametersInfoForDpi};
use windows::Win32::UI::WindowsAndMessaging::{
    HMENU, MENUITEMINFOW, MFT_BITMAP, MIIM_TYPE, NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS,
    SetMenuItemInfoW,
};
use windows::core::{HSTRING, PWSTR};

/// Longest side of the preview thumbnail at 96 dpi.
const PREVIEW_EDGE: i32 = 128;
/// Space between the thumbnail and the text, and right of the text.
const GAP: i32 = 12;
/// Space above and below the thumbnail.
const PAD_V: i32 = 8;

/// Text colour of the dark menu (and the background it is drawn over, for reference).
const DARK_TEXT: (u8, u8, u8) = (0xFF, 0xFF, 0xFF);

/// Apps use the dark theme (Settings > Personalization > Colors).
pub fn dark_mode() -> bool {
    windows_registry::CURRENT_USER
        .open(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|k| k.get_u32("AppsUseLightTheme"))
        .is_ok_and(|v| v == 0)
}

/// Rounded `px * dpi / 96`.
fn scale(px: i32, dpi: u32) -> i32 {
    (px * dpi as i32 + 48) / 96
}

/// The system DPI (at least 96).
pub fn system_dpi() -> u32 {
    // SAFETY: no arguments, no state.
    unsafe { GetDpiForSystem() }.max(96)
}

/// The longest side of the thumbnail to ask the shell for at `dpi`.
pub fn thumbnail_edge(dpi: u32) -> i32 {
    scale(PREVIEW_EDGE, dpi.max(96))
}

/// The menu font (`lfMenuFont`) for `dpi`, to be freed with `DeleteObject`. Greyscale
/// antialiased: the text ends up in an alpha bitmap, where ClearType's per-channel coverage
/// cannot be expressed.
fn menu_font(dpi: u32) -> HFONT {
    let mut metrics = NONCLIENTMETRICSW {
        cbSize: size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    // SAFETY: `metrics` is a NONCLIENTMETRICSW of the size passed in `cbSize`.
    let ok = unsafe {
        SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            metrics.cbSize,
            Some((&raw mut metrics).cast()),
            0,
            dpi,
        )
    };
    if ok.is_err() {
        // A 9 pt Segoe UI rather than an empty LOGFONT (which would be a huge system font).
        metrics.lfMenuFont.lfHeight = -scale(12, dpi);
        for (d, s) in metrics
            .lfMenuFont
            .lfFaceName
            .iter_mut()
            .zip("Segoe UI".encode_utf16())
        {
            *d = s;
        }
    }
    metrics.lfMenuFont.lfQuality = ANTIALIASED_QUALITY;
    // SAFETY: a fully initialised LOGFONTW.
    unsafe { CreateFontIndirectW(&metrics.lfMenuFont) }
}

/// The MSAA child id of the header row: the 1-based index of the first item.
const HEADER_CHILD_ID: u32 = 1;

/// Runs `f` with the dynamic annotation service; `None` when it cannot be created.
fn with_annotations<T>(f: impl FnOnce(&IAccPropServices) -> T) -> Option<T> {
    // SAFETY: COM is initialised for this call (or already was on this thread) and uninitialised
    // again only when this call initialised it, after the service is released.
    unsafe {
        let inited = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let result = CoCreateInstance::<_, IAccPropServices>(
            &CLSID_AccPropServices,
            None,
            CLSCTX_INPROC_SERVER,
        )
        .ok()
        .map(|services| f(&services));
        if inited {
            CoUninitialize();
        }
        result
    }
}

/// A bitmap owned by the menu code that built it: freed on drop, after the menu is gone.
pub struct HeaderBitmap {
    bitmap: HBITMAP,
    /// The header text, the row's accessible name.
    label: HSTRING,
    /// The menu whose row 0 shows this bitmap and carries its name annotation.
    attached: Cell<Option<HMENU>>,
}

impl HeaderBitmap {
    /// Makes row 0 of `menu` show this bitmap as its whole content and names it for
    /// accessibility. [`HeaderBitmap::detach`] (or dropping) removes the name again.
    ///
    /// # Safety
    /// `menu` must be a valid menu this thread may modify, whose row 0 is the header, and it
    /// must outlive neither this bitmap nor a call to `detach` or drop.
    pub unsafe fn attach(&self, menu: HMENU) -> windows::core::Result<()> {
        // SAFETY: per the contract.
        unsafe { SetMenuItemInfoW(menu, 0, true, &self.item_info()) }?;
        // The row works without its name, so a missing annotation service is not an error.
        let _ = with_annotations(|services| {
            // SAFETY: valid menu; the string is copied by the service.
            unsafe { services.SetHmenuPropStr(menu, HEADER_CHILD_ID, PROPID_ACC_NAME, &self.label) }
        });
        self.attached.set(Some(menu));
        Ok(())
    }

    /// Makes row 0 of `menu` show this bitmap instead of `previous`, which `attach` put there
    /// (the menu may be open: the caller repaints it). The accessible name moves to this
    /// bitmap without being cleared and set again, so `previous` can be dropped afterwards.
    ///
    /// # Safety
    /// As [`HeaderBitmap::attach`]; `previous` must be attached to `menu` and is dropped by the
    /// caller only after this returns.
    pub unsafe fn replace(
        &self,
        menu: HMENU,
        previous: &HeaderBitmap,
    ) -> windows::core::Result<()> {
        // SAFETY: per the contract.
        unsafe { SetMenuItemInfoW(menu, 0, true, &self.item_info()) }?;
        if previous.attached.take() == Some(menu) {
            self.attached.set(Some(menu));
        } else {
            // SAFETY: per the contract.
            unsafe { self.attach(menu) }?;
        }
        Ok(())
    }

    /// Removes the name annotation from the menu this was attached to. Call before destroying
    /// the menu: annotations are keyed by menu handle, and handles are reused.
    pub fn detach(&self) {
        if let Some(menu) = self.attached.take() {
            let _ = with_annotations(|services| {
                // SAFETY: clearing one property of a row; the handle is only a key.
                unsafe { services.ClearHmenuProps(menu, HEADER_CHILD_ID, &[PROPID_ACC_NAME]) }
            });
        }
    }

    /// The `MENUITEMINFOW` that makes a row show this bitmap as its whole content.
    pub fn item_info(&self) -> MENUITEMINFOW {
        MENUITEMINFOW {
            cbSize: size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_TYPE,
            fType: MFT_BITMAP,
            dwTypeData: PWSTR(self.bitmap.0.cast()),
            ..Default::default()
        }
    }

    /// Width and height in pixels.
    pub fn size(&self) -> (i32, i32) {
        bitmap_size(self.bitmap).unwrap_or((0, 0))
    }
}

impl Drop for HeaderBitmap {
    fn drop(&mut self) {
        self.detach();
        // SAFETY: created by `compose` and not selected into any device context.
        let _ = unsafe { DeleteObject(HGDIOBJ(self.bitmap.0)) };
    }
}

fn bitmap_size(bitmap: HBITMAP) -> Option<(i32, i32)> {
    let mut info = BITMAP::default();
    // SAFETY: `info` is a BITMAP and the size passed is its size.
    let got = unsafe {
        GetObjectW(
            HGDIOBJ(bitmap.0),
            size_of::<BITMAP>() as i32,
            Some((&raw mut info).cast()),
        )
    };
    (got != 0 && info.bmWidth > 0 && info.bmHeight != 0)
        .then_some((info.bmWidth, info.bmHeight.abs()))
}

/// The thumbnail's size on screen: its own size, shrunk to fit the edge at `dpi`.
fn fit((w, h): (i32, i32), dpi: u32) -> (i32, i32) {
    let edge = thumbnail_edge(dpi);
    let long = w.max(h);
    if long <= edge {
        return (w, h);
    }
    ((w * edge / long).max(1), (h * edge / long).max(1))
}

/// Where a thumbnail goes in a header row, and what part of its bitmap to draw.
struct Thumb {
    bitmap: HBITMAP,
    source: (i32, i32),
    /// Offset inside the thumbnail box (below the top padding), and the size drawn.
    at: (i32, i32),
    size: (i32, i32),
}

/// Composes the header row: `thumbnail` (premultiplied 32-bpp, the shell's own thumbnail
/// bitmap, left to the caller) with `label` to its right, vertically centred, at `dpi`. The
/// thumbnail starts at the left edge of the item area, where the other rows' text starts. The
/// row is as tall as the thumbnail it shows.
/// `None` when the bitmap is unusable or a GDI object cannot be created.
pub fn compose(label: &str, thumbnail: HBITMAP, dpi: u32) -> Option<HeaderBitmap> {
    let dpi = dpi.max(96);
    let source = bitmap_size(thumbnail)?;
    let size = fit(source, dpi);
    let thumb = Thumb {
        bitmap: thumbnail,
        source,
        at: (0, 0),
        size,
    };
    compose_row(label, size, Some(&thumb), dpi)
}

/// How a bitmap sits in the box of [`compose_boxed`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// A thumbnail: at the left edge of the box, centred vertically in it.
    Thumbnail,
    /// A file icon, the stand-in for a missing thumbnail: centred in the box both ways.
    Icon,
}

/// Like [`compose`], but the row's size does not depend on the bitmap: it reserves the full
/// `thumbnail_edge` square for it, so a row composed without one (`None`: the box stays empty,
/// the label is drawn) and the row composed once it has arrived are the same size, and the text
/// sits in the same place. The bitmap sits in the box as `placement` says. `None` when a bitmap
/// is unusable or a GDI object cannot be created.
pub fn compose_boxed(
    label: &str,
    bitmap: Option<(HBITMAP, Placement)>,
    dpi: u32,
) -> Option<HeaderBitmap> {
    let dpi = dpi.max(96);
    let edge = thumbnail_edge(dpi);
    let thumb = match bitmap {
        Some((bitmap, placement)) => {
            let source = bitmap_size(bitmap)?;
            let size = fit(source, dpi);
            let x = match placement {
                Placement::Thumbnail => 0,
                Placement::Icon => (edge - size.0).max(0) / 2,
            };
            Some(Thumb {
                bitmap,
                source,
                at: (x, (edge - size.1).max(0) / 2),
                size,
            })
        }
        None => None,
    };
    compose_row(label, (edge, edge), thumb.as_ref(), dpi)
}

/// The row of `label` with a thumbnail box of `thumb_box` (and `thumb` in it, if any).
fn compose_row(
    label: &str,
    thumb_box: (i32, i32),
    thumb: Option<&Thumb>,
    dpi: u32,
) -> Option<HeaderBitmap> {
    let font = menu_font(dpi);
    let mut text: Vec<u16> = label.encode_utf16().collect();
    let (gap, pad_v) = (scale(GAP, dpi), scale(PAD_V, dpi));
    let fg = if dark_mode() {
        DARK_TEXT
    } else {
        // SAFETY: a plain system colour lookup.
        let c = unsafe { GetSysColor(COLOR_MENUTEXT) };
        (c as u8, (c >> 8) as u8, (c >> 16) as u8)
    };
    // SAFETY: every GDI object created below is freed on every path out of this block, and
    // the DIB's memory is only touched between `GdiFlush` and its deletion.
    let result = unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let result = compose_in(dc, &mut text, thumb_box, thumb, font, (gap, pad_v), fg);
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
        let _ = DeleteObject(HGDIOBJ(font.0));
        result
    };
    result.map(|bitmap| HeaderBitmap {
        bitmap,
        label: HSTRING::from(label),
        attached: Cell::new(None),
    })
}

/// The drawing of [`compose_row`] on the memory device context `dc`.
///
/// # Safety
/// `dc` must be a live memory device context, the thumbnail (if any) a live bitmap not selected
/// into one, and `font` a live font.
unsafe fn compose_in(
    dc: HDC,
    text: &mut [u16],
    (bw, bh): (i32, i32),
    thumb: Option<&Thumb>,
    font: HFONT,
    (gap, pad_v): (i32, i32),
    fg: (u8, u8, u8),
) -> Option<HBITMAP> {
    // SAFETY: per the contract; objects selected into the DCs are selected out again.
    unsafe {
        // Text extent.
        let previous_font = SelectObject(dc, HGDIOBJ(font.0));
        let mut extent = RECT::default();
        DrawTextW(
            dc,
            text,
            &mut extent,
            DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX | DT_LEFT,
        );
        let (tw, th) = (extent.right - extent.left, extent.bottom - extent.top);
        let (width, height) = (bw + gap + tw + gap, bh + 2 * pad_v);

        // The DIB: top-down 32-bpp, zeroed by the system (transparent black).
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let Ok(dib) = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) else {
            SelectObject(dc, previous_font);
            return None;
        };
        if bits.is_null() {
            SelectObject(dc, previous_font);
            let _ = DeleteObject(HGDIOBJ(dib.0));
            return None;
        }
        let previous_bitmap = SelectObject(dc, HGDIOBJ(dib.0));

        // The text, black on white (GDI's greyscale smoothing of dark-on-light text is the
        // lighter weight that matches the menu's own text; white on black looks bold)...
        let top = (height - th) / 2;
        let mut area = RECT {
            left: bw + gap,
            top,
            right: bw + gap + tw,
            bottom: top + th,
        };
        let pixels = std::slice::from_raw_parts_mut(bits.cast::<u32>(), (width * height) as usize);
        let rows = area.top.max(0)..area.bottom.min(height);
        let columns = area.left.max(0)..area.right.min(width);
        for y in rows.clone() {
            for x in columns.clone() {
                pixels[(y * width + x) as usize] = 0x00FF_FFFF;
            }
        }
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(0));
        DrawTextW(
            dc,
            text,
            &mut area,
            DT_SINGLELINE | DT_NOPREFIX | DT_LEFT | DT_TOP,
        );
        let _ = GdiFlush();
        // ...turned into premultiplied pixels of the text colour with that coverage as alpha.
        for y in rows {
            for x in columns.clone() {
                let px = &mut pixels[(y * width + x) as usize];
                let coverage = 255 - ((*px >> 8) & 0xFF);
                let channel = |c: u8| u32::from(c) * coverage / 255;
                *px =
                    (coverage << 24) | (channel(fg.0) << 16) | (channel(fg.1) << 8) | channel(fg.2);
            }
        }

        // The thumbnail, blended with its own alpha over the transparent pixels.
        let mut blended = true;
        if let Some(t) = thumb {
            let src = CreateCompatibleDC(Some(dc));
            let previous_thumbnail = SelectObject(src, HGDIOBJ(t.bitmap.0));
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            blended = AlphaBlend(
                dc,
                t.at.0,
                pad_v + t.at.1,
                t.size.0,
                t.size.1,
                src,
                0,
                0,
                t.source.0,
                t.source.1,
                blend,
            )
            .as_bool();
            SelectObject(src, previous_thumbnail);
            let _ = DeleteDC(src);
        }
        let _ = GdiFlush();

        SelectObject(dc, previous_bitmap);
        SelectObject(dc, previous_font);
        if !blended {
            let _ = DeleteObject(HGDIOBJ(dib.0));
            return None;
        }
        Some(dib)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 32-bpp DIB section, `w` x `h`, of one premultiplied pixel value, as the shell hands out.
    fn solid(w: i32, h: i32, argb: u32) -> HBITMAP {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        // SAFETY: a fresh DIB section; its memory is `w * h` 32-bit pixels, filled below.
        unsafe {
            let dib = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).unwrap();
            std::slice::from_raw_parts_mut(bits.cast::<u32>(), (w * h) as usize).fill(argb);
            dib
        }
    }

    fn pixel(bitmap: &HeaderBitmap, x: i32, y: i32) -> u32 {
        use windows::Win32::Graphics::Gdi::GetBitmapBits;
        let (w, h) = bitmap.size();
        let mut bits = vec![0u32; (w * h) as usize];
        // SAFETY: the buffer holds `w * h` 32-bit pixels, the bitmap's whole content.
        unsafe {
            GetBitmapBits(
                bitmap.bitmap,
                (bits.len() * 4) as i32,
                bits.as_mut_ptr().cast(),
            );
        }
        bits[(y * w + x) as usize]
    }

    #[test]
    fn dpi_scaling_rounds() {
        assert_eq!(scale(12, 96), 12);
        assert_eq!(scale(12, 144), 18);
        assert_eq!(thumbnail_edge(192), 256);
        assert_eq!(thumbnail_edge(0), 128, "never below 96 dpi");
    }

    #[test]
    fn a_thumbnail_is_shrunk_to_the_edge_and_never_enlarged() {
        assert_eq!(fit((200, 100), 96), (128, 64));
        assert_eq!(fit((100, 200), 96), (64, 128));
        assert_eq!(fit((64, 40), 96), (64, 40));
        assert_eq!(fit((64, 40), 192), (64, 40));
        assert_eq!(fit((400, 100), 192), (256, 64));
    }

    #[test]
    fn the_header_row_is_the_thumbnail_with_its_padding_and_the_text_to_its_right() {
        let thumb = solid(100, 60, 0xFF20_4060);
        let header = compose("photo.png  12 KB", thumb, 96).expect("composes");
        let (w, h) = header.size();
        assert_eq!(h, 60 + 2 * PAD_V, "padding above and below the thumbnail");
        assert!(w > 100 + 2 * GAP, "room for the text: {w}");
        // The thumbnail opaque at the item's left edge, below the top padding...
        assert_eq!(pixel(&header, 0, PAD_V), 0xFF20_4060);
        assert_eq!(pixel(&header, 99, PAD_V + 59), 0xFF20_4060);
        // ...and transparent in the padding and in the gap.
        assert_eq!(pixel(&header, 50, 0), 0);
        assert_eq!(pixel(&header, 100 + GAP / 2, h / 2), 0);
        // Some text pixel inside the text area carries coverage.
        let inked = (100 + GAP..w)
            .flat_map(|x| (0..h).map(move |y| (x, y)))
            .any(|(x, y)| pixel(&header, x, y) >> 24 != 0);
        assert!(inked, "the label is drawn");
        // SAFETY: created above, not selected anywhere.
        let _ = unsafe { DeleteObject(HGDIOBJ(thumb.0)) };
    }

    #[test]
    fn a_higher_dpi_gives_a_taller_row_for_an_upscaled_request_only() {
        // A thumbnail asked for at 192 dpi arrives up to 256 px long and is kept as is.
        let thumb = solid(256, 128, 0xFF00_FF00);
        let header = compose("a", thumb, 192).expect("composes");
        assert_eq!(header.size().1, 128 + 2 * scale(PAD_V, 192));
        // SAFETY: created above, not selected anywhere.
        let _ = unsafe { DeleteObject(HGDIOBJ(thumb.0)) };
    }

    #[test]
    fn an_unusable_thumbnail_composes_nothing() {
        assert!(compose("a", HBITMAP::default(), 96).is_none());
    }

    /// Every pixel of `bitmap`, row by row.
    fn pixels(bitmap: &HeaderBitmap) -> Vec<u32> {
        use windows::Win32::Graphics::Gdi::GetBitmapBits;
        let (w, h) = bitmap.size();
        let mut bits = vec![0u32; (w * h) as usize];
        // SAFETY: the buffer holds `w * h` 32-bit pixels, the bitmap's whole content.
        unsafe {
            GetBitmapBits(
                bitmap.bitmap,
                (bits.len() * 4) as i32,
                bits.as_mut_ptr().cast(),
            );
        }
        bits
    }

    #[test]
    fn a_boxed_row_has_the_same_size_with_or_without_its_thumbnail() {
        let empty = compose_boxed("photo.png  12 KB", None, 96).expect("composes");
        let (w, h) = empty.size();
        assert_eq!(h, 128 + 2 * PAD_V, "the whole edge box is reserved");
        assert!(w > 128 + 2 * GAP, "room for the text: {w}");
        let at = |bits: &[u32], x: i32, y: i32| bits[(y * w + x) as usize];
        let empty_bits = pixels(&empty);
        // The box is empty (transparent) and the label is drawn right of it.
        assert!((0..128).all(|x| (0..h).all(|y| at(&empty_bits, x, y) == 0)));
        let inked = |bits: &[u32]| {
            (128 + GAP..w)
                .flat_map(|x| (0..h).map(move |y| (x, y)))
                .any(|(x, y)| at(bits, x, y) >> 24 != 0)
        };
        assert!(inked(&empty_bits), "the label is drawn without a thumbnail");

        // A wide thumbnail, centred vertically in the box, at its left edge.
        let thumb = solid(200, 100, 0xFF20_4060);
        let full = compose_boxed("photo.png  12 KB", Some((thumb, Placement::Thumbnail)), 96)
            .expect("composes");
        assert_eq!(full.size(), (w, h), "the swap does not resize the row");
        let full_bits = pixels(&full);
        let top = PAD_V + (128 - 64) / 2;
        assert_eq!(at(&full_bits, 0, top), 0xFF20_4060);
        assert_eq!(at(&full_bits, 127, top + 63), 0xFF20_4060);
        assert_eq!(at(&full_bits, 50, top - 1), 0, "empty above the thumbnail");
        assert_eq!(at(&full_bits, 50, top + 64), 0, "and below it");
        assert!(inked(&full_bits));
        // The text is in the same place in both.
        for y in 0..h {
            for x in 128 + GAP..w {
                assert_eq!(
                    at(&empty_bits, x, y),
                    at(&full_bits, x, y),
                    "text pixel {x},{y}"
                );
            }
        }
        // SAFETY: created above, not selected anywhere.
        let _ = unsafe { DeleteObject(HGDIOBJ(thumb.0)) };
    }

    #[test]
    fn an_icon_is_centred_in_the_box_and_the_row_keeps_its_size() {
        let empty = compose_boxed("a.txt  1 KB", None, 96).expect("composes");
        let icon = solid(48, 48, 0xFF80_8080);
        let full =
            compose_boxed("a.txt  1 KB", Some((icon, Placement::Icon)), 96).expect("composes");
        assert_eq!(full.size(), empty.size());
        let (w, _) = full.size();
        let bits = pixels(&full);
        let at = |x: i32, y: i32| bits[(y * w + x) as usize];
        let (left, top) = ((128 - 48) / 2, PAD_V + (128 - 48) / 2);
        assert_eq!(at(left, top), 0xFF80_8080);
        assert_eq!(at(left + 47, top + 47), 0xFF80_8080);
        assert_eq!(at(left - 1, top), 0, "empty left of the icon");
        assert_eq!(at(left + 48, top), 0, "and right of it");
        assert_eq!(at(left, top - 1), 0, "and above it");
        // SAFETY: created above, not selected anywhere.
        let _ = unsafe { DeleteObject(HGDIOBJ(icon.0)) };
    }

    #[test]
    fn replacing_moves_the_name_to_the_new_bitmap() {
        use windows::Win32::UI::WindowsAndMessaging::{
            AppendMenuW, CreatePopupMenu, DestroyMenu, GetMenuItemInfoW, MF_STRING,
        };
        let first = compose_boxed("name.png  1 KB", None, 96).expect("composes");
        let second = compose_boxed("name.png  1 KB", None, 96).expect("composes");
        // SAFETY: a fresh menu owned by this test, detached before it is destroyed.
        unsafe {
            let menu = CreatePopupMenu().unwrap();
            AppendMenuW(menu, MF_STRING, 0, &HSTRING::from("name.png  1 KB")).unwrap();
            first.attach(menu).expect("row 0 takes the bitmap");
            second.replace(menu, &first).expect("row 0 takes the next");
            assert!(first.attached.get().is_none());
            assert_eq!(second.attached.get(), Some(menu));
            let mut info = MENUITEMINFOW {
                cbSize: size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_TYPE,
                ..Default::default()
            };
            GetMenuItemInfoW(menu, 0, true, &mut info).unwrap();
            assert_eq!(info.dwTypeData.0 as usize, second.bitmap.0 as usize);
            second.detach();
            let _ = DestroyMenu(menu);
        }
    }

    #[test]
    fn attaching_turns_row_0_into_the_bitmap_row_and_detaching_is_repeatable() {
        use windows::Win32::UI::WindowsAndMessaging::{
            AppendMenuW, CreatePopupMenu, DestroyMenu, GetMenuItemInfoW, MF_STRING,
        };
        let thumb = solid(40, 30, 0xFF10_2030);
        let header = compose("name.png  1 KB", thumb, 96).expect("composes");
        // SAFETY: a fresh menu owned by this test, detached before it is destroyed.
        unsafe {
            let menu = CreatePopupMenu().unwrap();
            AppendMenuW(menu, MF_STRING, 0, &HSTRING::from("name.png  1 KB")).unwrap();
            header.attach(menu).expect("row 0 takes the bitmap");
            let mut info = MENUITEMINFOW {
                cbSize: size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_TYPE,
                ..Default::default()
            };
            GetMenuItemInfoW(menu, 0, true, &mut info).unwrap();
            assert_eq!(info.fType.0 & MFT_BITMAP.0, MFT_BITMAP.0);
            header.detach();
            header.detach();
            let _ = DestroyMenu(menu);
            let _ = DeleteObject(HGDIOBJ(thumb.0));
        }
    }

    #[test]
    fn the_row_is_a_bitmap_item_that_carries_the_bitmap_handle() {
        let thumb = solid(10, 10, 0xFF00_0000);
        let header = compose("a", thumb, 96).expect("composes");
        let info = header.item_info();
        assert_eq!(info.fType, MFT_BITMAP);
        assert_eq!(info.dwTypeData.0 as usize, header.bitmap.0 as usize);
        // SAFETY: created above, not selected anywhere.
        let _ = unsafe { DeleteObject(HGDIOBJ(thumb.0)) };
    }
}
