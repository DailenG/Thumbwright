//! Small native dialogs for the interactive verbs: a one-line prompt and an editable text
//! window. They run in `twctl`, never in Explorer.

use std::cell::RefCell;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, COLOR_WINDOW, CreateFontW, DEFAULT_CHARSET,
    GetSysColorBrush, HBRUSH, HDC, HFONT, OUT_DEFAULT_PRECIS, SetBkMode, TRANSPARENT, UpdateWindow,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext;
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForSystem,
};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    BS_DEFPUSHBUTTON, BS_PUSHBUTTON, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, ES_AUTOHSCROLL, ES_AUTOVSCROLL, ES_MULTILINE, ES_WANTRETURN, GetClientRect,
    GetMessageW, GetSystemMetrics, GetWindowTextLengthW, GetWindowTextW, HICON, IDC_ARROW,
    IDCANCEL, IDOK, IMAGE_ICON, IsDialogMessageW, LR_DEFAULTCOLOR, LR_SHARED, LoadCursorW,
    LoadImageW, MSG, MoveWindow, PostQuitMessage, RegisterClassExW, SM_CXICON, SM_CXSMICON,
    SM_CYICON, SM_CYSMICON, SW_SHOW, SendMessageW, ShowWindow, TranslateMessage, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_CTLCOLORSTATIC, WM_DESTROY, WM_SETFOCUS, WM_SETFONT,
    WM_SIZE, WM_USER, WNDCLASSEXW, WS_BORDER, WS_CAPTION, WS_CHILD, WS_EX_CLIENTEDGE,
    WS_OVERLAPPEDWINDOW, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
};
use windows::core::{BOOL, HSTRING, PCWSTR, w};

/// Opts the process into per-monitor DPI awareness (first call wins; failure is harmless).
pub fn dpi_aware() {
    // SAFETY: no pointers; fails harmlessly when awareness was already set.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

/// Outer window size whose client area is `cw` x `ch` for `style`/`ex` at the system DPI,
/// so content laid out in client coordinates keeps its margins on every side.
pub fn window_size_for_client(
    cw: i32,
    ch: i32,
    style: WINDOW_STYLE,
    ex: WINDOW_EX_STYLE,
) -> (i32, i32) {
    let mut r = RECT {
        left: 0,
        top: 0,
        right: cw,
        bottom: ch,
    };
    // SAFETY: valid in/out rectangle; no other pointers.
    let ok = unsafe { AdjustWindowRectExForDpi(&mut r, style, false, ex, GetDpiForSystem()) };
    if ok.is_ok() {
        (r.right - r.left, r.bottom - r.top)
    } else {
        (cw, ch + scale(32))
    }
}

/// The product icon (resource id 1, linked by build.rs) at the given size, or a null handle.
/// The taskbar takes the exe's icon on its own; the caption only shows what the window class
/// (or WM_SETICON) supplies, so every titled window must set it.
pub fn product_icon(small: bool) -> HICON {
    let (cx, cy) = if small {
        (SM_CXSMICON, SM_CYSMICON)
    } else {
        (SM_CXICON, SM_CYICON)
    };
    // SAFETY: loads an icon resource of this module; shared, process-owned handle.
    unsafe {
        LoadImageW(
            Some(module()),
            PCWSTR(std::ptr::without_provenance(1)),
            IMAGE_ICON,
            GetSystemMetrics(cx),
            GetSystemMetrics(cy),
            LR_DEFAULTCOLOR | LR_SHARED,
        )
        .map_or(HICON::default(), |h| HICON(h.0))
    }
}

/// Title bar in the apps' dark or light mode, like Workbench.
pub fn themed_caption(hwnd: HWND) {
    let dark = BOOL::from(tw_core::menu_header::dark_mode());
    // SAFETY: documented DWM attribute on our own window; the value is a BOOL of the stated size.
    let _ = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            std::ptr::from_ref(&dark).cast(),
            size_of::<BOOL>() as u32,
        )
    };
}

fn scale(px: i32) -> i32 {
    // SAFETY: no arguments.
    let dpi = unsafe { GetDpiForSystem() }.max(96) as i32;
    px * dpi / 96
}

/// Segoe UI at about 9 pt (system DPI), the font of every twctl window. Created on first use;
/// never freed (the process is short-lived). Stored as `isize` because `HFONT` is not `Sync`.
static FONT: std::sync::LazyLock<isize> = std::sync::LazyLock::new(|| {
    // SAFETY: plain GDI font creation with a valid face name.
    unsafe {
        CreateFontW(
            -scale(14),
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            0,
            w!("Segoe UI"),
        )
    }
    .0 as isize
});

pub fn font() -> HFONT {
    HFONT(*FONT as *mut _)
}

/// `WM_CTLCOLORSTATIC` answer: labels drawn on the window colour instead of the grey
/// dialog face.
pub fn static_on_window(wparam: WPARAM) -> LRESULT {
    // SAFETY: the static control's DC passed by the system; stock brush.
    unsafe {
        SetBkMode(HDC(wparam.0 as *mut _), TRANSPARENT);
        LRESULT(GetSysColorBrush(COLOR_WINDOW).0 as isize)
    }
}

fn module() -> windows::Win32::Foundation::HINSTANCE {
    // SAFETY: the current module handle.
    windows::Win32::Foundation::HINSTANCE(
        unsafe { GetModuleHandleW(PCWSTR::null()) }
            .map(|h| h.0)
            .unwrap_or_default(),
    )
}

#[allow(clippy::too_many_arguments)]
fn child(
    parent: HWND,
    class: PCWSTR,
    text: &str,
    style: u32,
    ex: u32,
    rect: (i32, i32, i32, i32),
    id: i32,
) -> HWND {
    // SAFETY: a child control of a live window; the id is passed as the menu handle.
    let h = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(ex),
            class,
            &HSTRING::from(text),
            WINDOW_STYLE(style) | WS_CHILD | WS_VISIBLE,
            rect.0,
            rect.1,
            rect.2,
            rect.3,
            Some(parent),
            Some(windows::Win32::UI::WindowsAndMessaging::HMENU(
                id as isize as *mut _,
            )),
            Some(module()),
            None,
        )
    }
    .unwrap_or_default();
    // SAFETY: valid control handle (or null, which SendMessage tolerates).
    unsafe {
        SendMessageW(
            h,
            WM_SETFONT,
            Some(WPARAM(font().0 as usize)),
            Some(LPARAM(1)),
        )
    };
    h
}

fn read_text(edit: HWND) -> String {
    // SAFETY: the buffer is sized from the control's own length query.
    unsafe {
        let n = GetWindowTextLengthW(edit).max(0) as usize;
        let mut buf = vec![0u16; n + 1];
        let got = GetWindowTextW(edit, &mut buf).max(0) as usize;
        String::from_utf16_lossy(&buf[..got])
    }
}

/// Registers a plain twctl window class with the product icon (caption and Alt+Tab); a class
/// without one shows the generic window icon in the title bar. Registering twice fails
/// harmlessly.
pub fn register(
    class: PCWSTR,
    proc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
) {
    // SAFETY: a plain window class whose procedure and name are 'static.
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(proc),
            hInstance: module(),
            lpszClassName: class,
            hIcon: product_icon(false),
            hIconSm: product_icon(true),
            hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as isize as *mut _),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassExW(&wc);
    }
}

fn pump(dialog: HWND) {
    let mut msg = MSG::default();
    // SAFETY: a standard message loop on the thread that created `dialog`.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if !IsDialogMessageW(dialog, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

#[derive(Default)]
struct PromptState {
    edit: HWND,
    answer: Option<String>,
}

thread_local! {
    static PROMPT: RefCell<PromptState> = RefCell::new(PromptState::default());
}

const DM_GETDEFID: u32 = WM_USER;
const DC_HASDEFID: u32 = 0x534B;

unsafe extern "system" fn prompt_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        DM_GETDEFID => LRESULT(((DC_HASDEFID << 16) | IDOK.0 as u32) as isize),
        WM_CTLCOLORSTATIC => static_on_window(wparam),
        WM_COMMAND => {
            match (wparam.0 & 0xFFFF) as i32 {
                x if x == IDOK.0 => {
                    PROMPT.with(|p| {
                        let mut p = p.borrow_mut();
                        p.answer = Some(read_text(p.edit));
                    });
                    // SAFETY: the dialog's own window.
                    let _ = unsafe { DestroyWindow(hwnd) };
                }
                x if x == IDCANCEL.0 => {
                    // SAFETY: the dialog's own window.
                    let _ = unsafe { DestroyWindow(hwnd) };
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            // SAFETY: the dialog's own window.
            let _ = unsafe { DestroyWindow(hwnd) };
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

/// One-line text prompt. `None` when cancelled.
pub fn prompt(title: &str, label: &str, default: &str) -> Option<String> {
    dpi_aware();
    let class = w!("ThumbwrightPrompt");
    register(class, prompt_proc);
    // Client-area layout: 12 px margins on every side, buttons right-aligned.
    let (cw, ch) = (scale(424), scale(112));
    let style = WS_CAPTION | WS_SYSMENU;
    let (ww, wh) = window_size_for_client(cw, ch, style, WINDOW_EX_STYLE(0));
    // SAFETY: a top-level dialog-like window; children are created below.
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            &HSTRING::from(title),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            ww,
            wh,
            None,
            None,
            Some(module()),
            None,
        )
    }
    .ok()?;
    themed_caption(hwnd);
    let m = scale(12);
    let inner = cw - 2 * m;
    let (bw, bh, gap) = (scale(80), scale(26), scale(8));
    let by = ch - m - bh;
    let _ = child(hwnd, w!("STATIC"), label, 0, 0, (m, m, inner, scale(20)), 0);
    let edit = child(
        hwnd,
        w!("EDIT"),
        default,
        WS_TABSTOP.0 | WS_BORDER.0 | ES_AUTOHSCROLL as u32,
        WS_EX_CLIENTEDGE.0,
        (m, m + scale(26), inner, scale(24)),
        100,
    );
    let _ = child(
        hwnd,
        w!("BUTTON"),
        "OK",
        WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32,
        0,
        (cw - m - 2 * bw - gap, by, bw, bh),
        IDOK.0,
    );
    let _ = child(
        hwnd,
        w!("BUTTON"),
        "Cancel",
        WS_TABSTOP.0 | BS_PUSHBUTTON as u32,
        0,
        (cw - m - bw, by, bw, bh),
        IDCANCEL.0,
    );
    PROMPT.with(|p| *p.borrow_mut() = PromptState { edit, answer: None });
    // SAFETY: the window and edit control were just created on this thread.
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);
        let _ = SetFocus(Some(edit));
        // Select the whole default so typing replaces it (EM_SETSEL 0..-1).
        SendMessageW(edit, 0x00B1, Some(WPARAM(0)), Some(LPARAM(-1)));
    }
    pump(hwnd);
    PROMPT.with(|p| p.borrow_mut().answer.take())
}

thread_local! {
    static VIEW_EDIT: RefCell<HWND> = RefCell::new(HWND::default());
}

unsafe extern "system" fn view_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_SIZE => {
            let mut r = RECT::default();
            // SAFETY: the window's own client rectangle.
            unsafe {
                let _ = GetClientRect(hwnd, &mut r);
                let edit = VIEW_EDIT.with(|e| *e.borrow());
                let _ = MoveWindow(edit, 0, 0, r.right, r.bottom, true);
            }
            LRESULT(0)
        }
        WM_SETFOCUS => {
            let edit = VIEW_EDIT.with(|e| *e.borrow());
            // SAFETY: focus to the child edit control.
            let _ = unsafe { SetFocus(Some(edit)) };
            LRESULT(0)
        }
        WM_CLOSE => {
            // SAFETY: the window's own handle.
            let _ = unsafe { DestroyWindow(hwnd) };
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

/// Shows `text` in an editable window and returns when the user closes it.
pub fn text_window(title: &str, text: &str) {
    dpi_aware();
    let class = w!("ThumbwrightText");
    register(class, view_proc);
    // SAFETY: a top-level window with one multi-line edit child.
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            &HSTRING::from(title),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            scale(640),
            scale(420),
            None,
            None,
            Some(module()),
            None,
        )
    };
    let Ok(hwnd) = hwnd else { return };
    themed_caption(hwnd);
    // EDIT controls want CRLF line breaks.
    let body = text.replace("\r\n", "\n").replace('\n', "\r\n");
    let edit = child(
        hwnd,
        w!("EDIT"),
        &body,
        WS_TABSTOP.0
            | WS_VSCROLL.0
            | ES_MULTILINE as u32
            | ES_AUTOVSCROLL as u32
            | ES_WANTRETURN as u32,
        WS_EX_CLIENTEDGE.0,
        (0, 0, scale(620), scale(380)),
        100,
    );
    VIEW_EDIT.with(|e| *e.borrow_mut() = edit);
    // SAFETY: the window was just created on this thread.
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);
        let _ = SetFocus(Some(edit));
    }
    pump(hwnd);
}
