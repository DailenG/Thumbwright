//! "Pick color": a system-wide eyedropper with a 10x loupe (FUNCTIONAL_SPEC.md 6.4).
//!
//! The virtual screen is captured once and shown in a topmost full-screen window, so the
//! picked pixel is exactly what was on screen when the verb started. Click picks, Esc or
//! right-click cancels, arrow keys nudge the pointer by one pixel.

use std::cell::RefCell;
use std::ffi::c_void;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BeginPaint, BitBlt, CAPTUREBLT, COLORONCOLOR,
    CreateCompatibleBitmap, CreateCompatibleDC, CreateDIBSection, CreatePen, CreateSolidBrush,
    DEFAULT_GUI_FONT, DIB_RGB_COLORS, DeleteDC, DeleteObject, EndPaint, FillRect, GetDC,
    GetStockObject, HBRUSH, HDC, HGDIOBJ, InvalidateRect, NULL_BRUSH, PAINTSTRUCT, PS_SOLID,
    Rectangle, ReleaseDC, SRCCOPY, ScreenToClient, SelectObject, SetBkMode, SetStretchBltMode,
    SetTextColor, StretchBlt, TRANSPARENT, TextOutW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    VK_DOWN, VK_ESCAPE, VK_LEFT, VK_RETURN, VK_RIGHT, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW,
    GetSystemMetrics, IDC_CROSS, LoadCursorW, MSG, PostQuitMessage, RegisterClassW,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_SHOW,
    SetCursorPos, SetForegroundWindow, ShowWindow, TranslateMessage, WM_DESTROY, WM_ERASEBKGND,
    WM_KEYDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT, WM_RBUTTONUP, WNDCLASSW, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{PCWSTR, w};

use crate::ops::Result;

/// Source pixels shown by the loupe (odd, so one pixel sits in the middle).
const LOUPE_PIXELS: i32 = 15;
const ZOOM: i32 = 10;
const LOUPE: i32 = LOUPE_PIXELS * ZOOM;
const CAPTION_H: i32 = 26;
const OFFSET: i32 = 24;

struct State {
    /// Top-down BGRA copy of the virtual screen, owned by the selected DIB section.
    bits: *const u8,
    width: i32,
    height: i32,
    shot: HDC,
    cursor: POINT,
    shown: RECT,
    picked: Option<(u8, u8, u8)>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn pixel(s: &State, x: i32, y: i32) -> Option<(u8, u8, u8)> {
    if x < 0 || y < 0 || x >= s.width || y >= s.height {
        return None;
    }
    let i = (y as usize * s.width as usize + x as usize) * 4;
    // SAFETY: the index is inside the width*height*4 byte DIB section.
    let px = unsafe { std::slice::from_raw_parts(s.bits.add(i), 4) };
    Some((px[2], px[1], px[0]))
}

/// Where the loupe + caption sit for a pointer at `p` (flipped near the screen edges).
fn loupe_rect(p: POINT, width: i32, height: i32) -> RECT {
    let total_h = LOUPE + CAPTION_H;
    let left = if p.x + OFFSET + LOUPE > width {
        p.x - OFFSET - LOUPE
    } else {
        p.x + OFFSET
    };
    let top = if p.y + OFFSET + total_h > height {
        p.y - OFFSET - total_h
    } else {
        p.y + OFFSET
    };
    RECT {
        left,
        top,
        right: left + LOUPE,
        bottom: top + total_h,
    }
}

fn colorref(r: u8, g: u8, b: u8) -> windows::Win32::Foundation::COLORREF {
    windows::Win32::Foundation::COLORREF(u32::from(r) | (u32::from(g) << 8) | (u32::from(b) << 16))
}

/// Draws the loupe into `dc`, whose origin is `(-ox, -oy)` of the window.
///
/// # Safety
/// `dc` must be a valid device context and `s.shot` a valid memory DC.
unsafe fn draw_loupe(dc: HDC, s: &State, ox: i32, oy: i32) {
    let r = loupe_rect(s.cursor, s.width, s.height);
    let (x, y) = (r.left - ox, r.top - oy);
    // SAFETY: GDI calls on valid DCs; every object created here is deleted below.
    unsafe {
        let black = CreateSolidBrush(colorref(0, 0, 0));
        FillRect(
            dc,
            &RECT {
                left: x - 2,
                top: y - 2,
                right: x + LOUPE + 2,
                bottom: y + LOUPE + CAPTION_H + 2,
            },
            black,
        );
        let _ = DeleteObject(HGDIOBJ(black.0));
        SetStretchBltMode(dc, COLORONCOLOR);
        let half = LOUPE_PIXELS / 2;
        let _ = StretchBlt(
            dc,
            x,
            y,
            LOUPE,
            LOUPE,
            Some(s.shot),
            s.cursor.x - half,
            s.cursor.y - half,
            LOUPE_PIXELS,
            LOUPE_PIXELS,
            SRCCOPY,
        );
        // Frame the centre pixel in white over black so it shows on any colour.
        let hollow = GetStockObject(NULL_BRUSH);
        let old_brush = SelectObject(dc, hollow);
        for (inset, rgb) in [(0, 0u8), (1, 255u8)] {
            let pen = CreatePen(PS_SOLID, 1, colorref(rgb, rgb, rgb));
            let old_pen = SelectObject(dc, HGDIOBJ(pen.0));
            let c = half * ZOOM;
            let _ = Rectangle(
                dc,
                x + c - inset,
                y + c - inset,
                x + c + ZOOM + 1 + inset,
                y + c + ZOOM + 1 + inset,
            );
            SelectObject(dc, old_pen);
            let _ = DeleteObject(HGDIOBJ(pen.0));
        }
        SelectObject(dc, old_brush);
        // Caption: swatch + hex + decimal.
        let (pr, pg, pb) = pixel(s, s.cursor.x, s.cursor.y).unwrap_or((0, 0, 0));
        let swatch = CreateSolidBrush(colorref(pr, pg, pb));
        FillRect(
            dc,
            &RECT {
                left: x,
                top: y + LOUPE,
                right: x + CAPTION_H,
                bottom: y + LOUPE + CAPTION_H,
            },
            swatch,
        );
        let _ = DeleteObject(HGDIOBJ(swatch.0));
        let font = GetStockObject(DEFAULT_GUI_FONT);
        let old_font = SelectObject(dc, font);
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, colorref(255, 255, 255));
        let text: Vec<u16> = format!("#{pr:02X}{pg:02X}{pb:02X}  {pr},{pg},{pb}")
            .encode_utf16()
            .collect();
        let _ = TextOutW(dc, x + CAPTION_H + 6, y + LOUPE + 5, &text);
        SelectObject(dc, old_font);
    }
}

fn invalidate(hwnd: HWND, r: &RECT) {
    let padded = RECT {
        left: r.left - 4,
        top: r.top - 4,
        right: r.right + 4,
        bottom: r.bottom + 4,
    };
    // SAFETY: a rectangle in this window's client space.
    let _ = unsafe { InvalidateRect(Some(hwnd), Some(&padded), false) };
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_MOUSEMOVE => {
            let (x, y) = (
                (lparam.0 & 0xFFFF) as i16 as i32,
                ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
            );
            STATE.with(|st| {
                if let Some(s) = st.borrow_mut().as_mut() {
                    let old = s.shown;
                    s.cursor = POINT { x, y };
                    s.shown = loupe_rect(s.cursor, s.width, s.height);
                    invalidate(hwnd, &old);
                    invalidate(hwnd, &s.shown);
                }
            });
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            STATE.with(|st| {
                if let Some(s) = st.borrow_mut().as_mut() {
                    s.picked = pixel(s, s.cursor.x, s.cursor.y);
                }
            });
            // SAFETY: the window's own handle.
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            // SAFETY: the window's own handle.
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let key = wparam.0 as u16;
            if key == VK_ESCAPE.0 {
                // SAFETY: the window's own handle.
                let _ = unsafe { DestroyWindow(hwnd) };
            } else if key == VK_RETURN.0 {
                STATE.with(|st| {
                    if let Some(s) = st.borrow_mut().as_mut() {
                        s.picked = pixel(s, s.cursor.x, s.cursor.y);
                    }
                });
                // SAFETY: the window's own handle.
                let _ = unsafe { DestroyWindow(hwnd) };
            } else {
                let (dx, dy) = match key {
                    k if k == VK_LEFT.0 => (-1, 0),
                    k if k == VK_RIGHT.0 => (1, 0),
                    k if k == VK_UP.0 => (0, -1),
                    k if k == VK_DOWN.0 => (0, 1),
                    _ => (0, 0),
                };
                if dx != 0 || dy != 0 {
                    let mut p = POINT::default();
                    // SAFETY: moves the pointer by one pixel; the resulting WM_MOUSEMOVE updates the loupe.
                    unsafe {
                        if GetCursorPos(&mut p).is_ok() {
                            let _ = SetCursorPos(p.x + dx, p.y + dy);
                        }
                    }
                }
            }
            LRESULT(0)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            // SAFETY: standard paint cycle; the back buffer is freed before EndPaint.
            unsafe {
                let dc = BeginPaint(hwnd, &mut ps);
                let r = ps.rcPaint;
                let (pw, ph) = (r.right - r.left, r.bottom - r.top);
                if pw > 0 && ph > 0 {
                    let back = CreateCompatibleDC(Some(dc));
                    let bmp = CreateCompatibleBitmap(dc, pw, ph);
                    let old = SelectObject(back, HGDIOBJ(bmp.0));
                    STATE.with(|st| {
                        if let Some(s) = st.borrow().as_ref() {
                            let _ =
                                BitBlt(back, 0, 0, pw, ph, Some(s.shot), r.left, r.top, SRCCOPY);
                            draw_loupe(back, s, r.left, r.top);
                        }
                    });
                    let _ = BitBlt(dc, r.left, r.top, pw, ph, Some(back), 0, 0, SRCCOPY);
                    SelectObject(back, old);
                    let _ = DeleteObject(HGDIOBJ(bmp.0));
                    let _ = DeleteDC(back);
                }
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Runs the eyedropper. `Ok(None)` when cancelled.
pub fn pick() -> Result<Option<(u8, u8, u8)>> {
    crate::ui::dpi_aware();
    // SAFETY: GDI/USER setup on this thread; every created object is released below.
    unsafe {
        let (vx, vy) = (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
        );
        let (w, h) = (
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        );
        if w <= 0 || h <= 0 {
            return Err("no screen to pick from".into());
        }
        let screen = GetDC(None);
        let shot = CreateCompatibleDC(Some(screen));
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
        let mut bits: *mut c_void = std::ptr::null_mut();
        let dib = match CreateDIBSection(Some(screen), &info, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(d) if !bits.is_null() => d,
            _ => {
                let _ = DeleteDC(shot);
                ReleaseDC(None, screen);
                return Err("cannot allocate the screen capture".into());
            }
        };
        let old = SelectObject(shot, HGDIOBJ(dib.0));
        let copied = BitBlt(shot, 0, 0, w, h, Some(screen), vx, vy, SRCCOPY | CAPTUREBLT);
        ReleaseDC(None, screen);
        let cleanup = |hwnd: Option<HWND>| {
            if let Some(h) = hwnd {
                let _ = DestroyWindow(h);
            }
            SelectObject(shot, old);
            let _ = DeleteObject(HGDIOBJ(dib.0));
            let _ = DeleteDC(shot);
        };
        if copied.is_err() {
            cleanup(None);
            return Err("cannot capture the screen".into());
        }
        let class = w!("ThumbwrightPick");
        let module = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: windows::Win32::Foundation::HINSTANCE(module.0),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_CROSS).unwrap_or_default(),
            hbrBackground: HBRUSH::default(),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class,
            w!("Thumbwright color picker"),
            WS_POPUP,
            vx,
            vy,
            w,
            h,
            None,
            None,
            Some(windows::Win32::Foundation::HINSTANCE(module.0)),
            None,
        );
        let Ok(hwnd) = hwnd else {
            cleanup(None);
            return Err("cannot open the picker window".into());
        };
        let mut p = POINT::default();
        let _ = GetCursorPos(&mut p);
        let _ = ScreenToClient(hwnd, &mut p);
        STATE.with(|st| {
            *st.borrow_mut() = Some(State {
                bits: bits.cast_const().cast(),
                width: w,
                height: h,
                shot,
                cursor: p,
                shown: loupe_rect(p, w, h),
                picked: None,
            });
        });
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let picked = STATE.with(|st| st.borrow_mut().take().and_then(|s| s.picked));
        cleanup(None);
        Ok(picked)
    }
}

/// `#RRGGBB`.
pub fn hex((r, g, b): (u8, u8, u8)) -> String {
    format!("#{r:02X}{g:02X}{b:02X}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loupe_flips_near_screen_edges_and_stays_on_screen() {
        let (w, h) = (1920, 1080);
        let mid = loupe_rect(POINT { x: 500, y: 500 }, w, h);
        assert_eq!((mid.left, mid.top), (500 + OFFSET, 500 + OFFSET));
        let corner = loupe_rect(POINT { x: w - 1, y: h - 1 }, w, h);
        assert!(corner.right <= w && corner.bottom <= h, "{corner:?}");
        assert_eq!(hex((255, 0, 128)), "#FF0080");
    }
}
