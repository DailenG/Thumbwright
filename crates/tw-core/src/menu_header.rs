//! The preview header of Thumbwright's menus: the file's thumbnail in a row of its own, above
//! an ordinary text row with the file's name and size. Shared by the classic submenu and the
//! `twctl menu` popup.
//!
//! A native menu sizes one shared bitmap column for every row from the widest item bitmap, so a
//! thumbnail attached to the text row (`MIIM_BITMAP`) pushes the text of every other row past
//! the thumbnail. An owner-drawn row avoids that but switches the whole menu to the classic,
//! non-themed look. So the thumbnail is a bitmap item (`MFT_BITMAP`): a row whose whole content
//! is one bitmap, in the item area and not in the bitmap column, and the menu stays themed. The
//! bitmap is composed here, in a premultiplied 32-bpp DIB that the menu blends over its own
//! background.
//!
//! A bitmap item has no text, so it has no accessible name. That is why it carries only the
//! picture: the name and size are the text row under it, a real menu item that every screen
//! reader reads, whether or not the thumbnail has arrived.
//!
//! A bitmap row narrower than the menu is not drawn as is: the menu repeats and stretches it
//! over the row and loses its transparency (black strips). The menu is as wide as its widest
//! row, so the bitmap is made at least as wide as the widest text row ([`widest_row`]), which
//! makes the bitmap the row that sets the width, and it is then drawn one to one.

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, AlphaBlend, BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    BLENDFUNCTION, CreateCompatibleDC, CreateDIBSection, CreateFontIndirectW, DIB_RGB_COLORS,
    DT_CALCRECT, DT_LEFT, DT_SINGLELINE, DeleteDC, DeleteObject, DrawTextW, GdiFlush, GetDC,
    GetObjectW, HBITMAP, HDC, HFONT, HGDIOBJ, ReleaseDC, SelectObject,
};
use windows::Win32::UI::HiDpi::{GetDpiForSystem, SystemParametersInfoForDpi};
use windows::Win32::UI::WindowsAndMessaging::{
    GetMenuItemCount, GetMenuStringW, HMENU, MENUITEMINFOW, MF_BYPOSITION, MFT_BITMAP, MIIM_TYPE,
    NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS,
};
use windows::core::PWSTR;

/// Longest side of the preview thumbnail at 96 dpi.
const PREVIEW_EDGE: i32 = 128;
/// Space above the thumbnail at 96 dpi.
const PAD_TOP: i32 = 8;
/// Space between the thumbnail and the text row under it at 96 dpi.
const PAD_BOTTOM: i32 = 4;

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

/// The menu font (`lfMenuFont`) for `dpi`, to be freed with `DeleteObject`.
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
    // SAFETY: a fully initialised LOGFONTW.
    unsafe { CreateFontIndirectW(&metrics.lfMenuFont) }
}

/// The width in pixels of the widest text row of `menu` in the menu font at `dpi` (menu
/// mnemonics `&` handled like the menu does); 0 for a menu with no text rows. Bitmap rows,
/// separators and the room the menu adds around every row are not counted.
pub fn widest_row(menu: HMENU, dpi: u32) -> i32 {
    let font = menu_font(dpi.max(96));
    // SAFETY: every GDI object created below is freed before returning; the buffers passed are
    // the sizes stated.
    let widest = unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let previous = SelectObject(dc, HGDIOBJ(font.0));
        let mut widest = 0;
        for pos in 0..GetMenuItemCount(Some(menu)).max(0) as u32 {
            let mut text = [0u16; 512];
            let len = GetMenuStringW(menu, pos, Some(&mut text), MF_BYPOSITION);
            if len <= 0 {
                continue;
            }
            let mut extent = RECT::default();
            DrawTextW(
                dc,
                &mut text[..len as usize],
                &mut extent,
                DT_CALCRECT | DT_SINGLELINE | DT_LEFT,
            );
            widest = widest.max(extent.right - extent.left);
        }
        SelectObject(dc, previous);
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
        widest
    };
    // SAFETY: created above and no longer selected into a device context.
    let _ = unsafe { DeleteObject(HGDIOBJ(font.0)) };
    widest
}

/// A bitmap owned by the menu code that built it: freed on drop, after the menu is gone.
pub struct HeaderBitmap {
    bitmap: HBITMAP,
}

impl HeaderBitmap {
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
        // SAFETY: created by `compose_row` and not selected into any device context.
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

/// Where a picture goes in a header row, and what part of its bitmap to draw.
struct Thumb {
    bitmap: HBITMAP,
    source: (i32, i32),
    /// Offset inside the picture box (below the top padding), and the size drawn.
    at: (i32, i32),
    size: (i32, i32),
}

/// Composes the thumbnail row: `thumbnail` (premultiplied 32-bpp, the shell's own thumbnail
/// bitmap, left to the caller) at `dpi`, at the left edge of the item area, where the other
/// rows' text starts. The row is as big as the thumbnail it shows, plus its padding, and at
/// least `min_width` wide (the [`widest_row`] of the menu it goes into).
/// `None` when the bitmap is unusable or a GDI object cannot be created.
pub fn compose(thumbnail: HBITMAP, dpi: u32, min_width: i32) -> Option<HeaderBitmap> {
    let dpi = dpi.max(96);
    let source = bitmap_size(thumbnail)?;
    let size = fit(source, dpi);
    let thumb = Thumb {
        bitmap: thumbnail,
        source,
        at: (0, 0),
        size,
    };
    compose_row(size, min_width, Some(&thumb), dpi)
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
/// `thumbnail_edge` square for it, so a row composed without one (`None`: the box stays empty)
/// and the row composed once it has arrived are the same size. The bitmap sits in the box as
/// `placement` says. `None` when a bitmap is unusable or a GDI object cannot be created.
pub fn compose_boxed(
    bitmap: Option<(HBITMAP, Placement)>,
    dpi: u32,
    min_width: i32,
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
    compose_row((edge, edge), min_width, thumb.as_ref(), dpi)
}

/// The row with a picture box of `thumb_box` (and `thumb` in it, if any), `min_width` wide at
/// least.
fn compose_row(
    thumb_box: (i32, i32),
    min_width: i32,
    thumb: Option<&Thumb>,
    dpi: u32,
) -> Option<HeaderBitmap> {
    let (top, bottom) = (scale(PAD_TOP, dpi), scale(PAD_BOTTOM, dpi));
    // SAFETY: every GDI object created below is freed on every path out of this block.
    let result = unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let row = (thumb_box.0.max(min_width), thumb_box.1);
        let result = compose_in(dc, row, thumb, (top, bottom));
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
        result
    };
    result.map(|bitmap| HeaderBitmap { bitmap })
}

/// The drawing of [`compose_row`] on the memory device context `dc`.
///
/// # Safety
/// `dc` must be a live memory device context, and the thumbnail (if any) a live bitmap not
/// selected into one.
unsafe fn compose_in(
    dc: HDC,
    (bw, bh): (i32, i32),
    thumb: Option<&Thumb>,
    (top, bottom): (i32, i32),
) -> Option<HBITMAP> {
    let height = bh + top + bottom;
    // The DIB: top-down 32-bpp, zeroed by the system (transparent black).
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: bw,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: per the contract; objects selected into the DCs are selected out again.
    unsafe {
        let mut bits = std::ptr::null_mut();
        let Ok(dib) = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) else {
            return None;
        };
        if bits.is_null() {
            let _ = DeleteObject(HGDIOBJ(dib.0));
            return None;
        }
        let Some(t) = thumb else {
            return Some(dib);
        };
        // The thumbnail, blended with its own alpha over the transparent pixels.
        let previous_bitmap = SelectObject(dc, HGDIOBJ(dib.0));
        let src = CreateCompatibleDC(Some(dc));
        let previous_thumbnail = SelectObject(src, HGDIOBJ(t.bitmap.0));
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let blended = AlphaBlend(
            dc,
            t.at.0,
            top + t.at.1,
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
        let _ = GdiFlush();
        SelectObject(dc, previous_bitmap);
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
    use windows::Win32::Graphics::Gdi::GetBitmapBits;

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

    /// Every pixel of `bitmap`, row by row.
    fn pixels(bitmap: &HeaderBitmap) -> Vec<u32> {
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

    fn free(bitmap: HBITMAP) {
        // SAFETY: created by the test, not selected anywhere.
        let _ = unsafe { DeleteObject(HGDIOBJ(bitmap.0)) };
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
    fn the_thumbnail_row_is_the_thumbnail_and_its_padding_only() {
        let thumb = solid(100, 60, 0xFF20_4060);
        let header = compose(thumb, 96, 0).expect("composes");
        let (w, h) = header.size();
        assert_eq!(w, 100, "as wide as the thumbnail when no text row is wider");
        assert_eq!(h, PAD_TOP + 60 + PAD_BOTTOM);
        let bits = pixels(&header);
        let at = |x: i32, y: i32| bits[(y * w + x) as usize];
        // The thumbnail opaque at the item's left edge, below the top padding...
        assert_eq!(at(0, PAD_TOP), 0xFF20_4060);
        assert_eq!(at(99, PAD_TOP + 59), 0xFF20_4060);
        // ...and transparent in the padding.
        assert_eq!(at(50, 0), 0);
        assert_eq!(at(50, h - 1), 0);
        free(thumb);
    }

    #[test]
    fn a_higher_dpi_gives_a_taller_row_for_an_upscaled_request_only() {
        // A thumbnail asked for at 192 dpi arrives up to 256 px long and is kept as is.
        let thumb = solid(256, 128, 0xFF00_FF00);
        let header = compose(thumb, 192, 0).expect("composes");
        assert_eq!(
            header.size(),
            (256, 128 + scale(PAD_TOP, 192) + scale(PAD_BOTTOM, 192))
        );
        free(thumb);
    }

    #[test]
    fn an_unusable_thumbnail_composes_nothing() {
        assert!(compose(HBITMAP::default(), 96, 0).is_none());
        assert!(compose_boxed(Some((HBITMAP::default(), Placement::Thumbnail)), 96, 0).is_none());
    }

    #[test]
    fn a_row_is_at_least_as_wide_as_the_widest_text_row_with_the_thumbnail_at_its_left() {
        let thumb = solid(100, 60, 0xFF20_4060);
        let row = compose(thumb, 96, 300).expect("composes");
        let (w, h) = row.size();
        assert_eq!((w, h), (300, PAD_TOP + 60 + PAD_BOTTOM));
        let bits = pixels(&row);
        assert_eq!(bits[(PAD_TOP * w) as usize], 0xFF20_4060);
        assert_eq!(
            bits[(PAD_TOP * w + 100) as usize],
            0,
            "transparent right of it"
        );
        let boxed = compose_boxed(None, 96, 300).expect("composes");
        assert_eq!(boxed.size(), (300, PAD_TOP + 128 + PAD_BOTTOM));
        let narrow = compose_boxed(None, 96, 50).expect("composes");
        assert_eq!(narrow.size().0, 128, "never narrower than the box");
        free(thumb);
    }

    #[test]
    fn the_widest_row_is_the_widest_text_and_ignores_bitmap_rows_and_separators() {
        use windows::Win32::UI::WindowsAndMessaging::{
            AppendMenuW, CreatePopupMenu, DestroyMenu, MF_SEPARATOR, MF_STRING,
        };
        use windows::core::{HSTRING, PCWSTR};
        let thumb = solid(400, 10, 0xFF00_0000);
        let bitmap = compose(thumb, 96, 0).expect("composes");
        // SAFETY: a fresh menu owned by this test, destroyed before it ends.
        unsafe {
            let menu = CreatePopupMenu().unwrap();
            assert_eq!(widest_row(menu, 96), 0, "no text rows");
            AppendMenuW(menu, MF_STRING, 0, &HSTRING::from("Copy")).unwrap();
            let short = widest_row(menu, 96);
            assert!(short > 10, "{short}");
            AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null()).unwrap();
            windows::Win32::UI::WindowsAndMessaging::InsertMenuItemW(
                menu,
                0,
                true,
                &bitmap.item_info(),
            )
            .unwrap();
            assert_eq!(
                widest_row(menu, 96),
                short,
                "bitmaps and separators are not text"
            );
            AppendMenuW(
                menu,
                MF_STRING,
                0,
                &HSTRING::from("a very much longer file name.png  878 KB"),
            )
            .unwrap();
            let long = widest_row(menu, 96);
            assert!(long > 3 * short, "{long} against {short}");
            assert!(
                widest_row(menu, 192) > long + long / 2,
                "scales with the dpi"
            );
            let _ = DestroyMenu(menu);
        }
        free(thumb);
    }

    #[test]
    fn a_boxed_row_has_the_same_size_with_or_without_its_thumbnail() {
        let empty = compose_boxed(None, 96, 0).expect("composes");
        let (w, h) = empty.size();
        assert_eq!((w, h), (128, PAD_TOP + 128 + PAD_BOTTOM), "the whole box");
        let at = |bits: &[u32], x: i32, y: i32| bits[(y * w + x) as usize];
        assert!(pixels(&empty).iter().all(|&p| p == 0), "an empty box");

        // A wide thumbnail, centred vertically in the box, at its left edge.
        let thumb = solid(200, 100, 0xFF20_4060);
        let full = compose_boxed(Some((thumb, Placement::Thumbnail)), 96, 0).expect("composes");
        assert_eq!(full.size(), (w, h), "the swap does not resize the row");
        let bits = pixels(&full);
        let top = PAD_TOP + (128 - 64) / 2;
        assert_eq!(at(&bits, 0, top), 0xFF20_4060);
        assert_eq!(at(&bits, 127, top + 63), 0xFF20_4060);
        assert_eq!(at(&bits, 50, top - 1), 0, "empty above the thumbnail");
        assert_eq!(at(&bits, 50, top + 64), 0, "and below it");
        free(thumb);
    }

    #[test]
    fn an_icon_is_centred_in_the_box_and_the_row_keeps_its_size() {
        let empty = compose_boxed(None, 96, 0).expect("composes");
        let icon = solid(48, 48, 0xFF80_8080);
        let full = compose_boxed(Some((icon, Placement::Icon)), 96, 0).expect("composes");
        assert_eq!(full.size(), empty.size());
        let (w, _) = full.size();
        let bits = pixels(&full);
        let at = |x: i32, y: i32| bits[(y * w + x) as usize];
        let (left, top) = ((128 - 48) / 2, PAD_TOP + (128 - 48) / 2);
        assert_eq!(at(left, top), 0xFF80_8080);
        assert_eq!(at(left + 47, top + 47), 0xFF80_8080);
        assert_eq!(at(left - 1, top), 0, "empty left of the icon");
        assert_eq!(at(left + 48, top), 0, "and right of it");
        assert_eq!(at(left, top - 1), 0, "and above it");
        free(icon);
    }

    #[test]
    fn the_row_is_a_bitmap_item_that_carries_the_bitmap_handle() {
        let thumb = solid(10, 10, 0xFF00_0000);
        let header = compose(thumb, 96, 0).expect("composes");
        let info = header.item_info();
        assert_eq!(info.fType, MFT_BITMAP);
        assert_eq!(info.dwTypeData.0 as usize, header.bitmap.0 as usize);
        free(thumb);
    }
}
