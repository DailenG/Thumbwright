//! What a menu user sees after a verb ran (`--notify`): the new files selected in Explorer,
//! a short auto-closing confirmation for clipboard-style verbs, a progress window for long
//! jobs, and a plain-language message box for failures. All windows are plain Win32 and
//! live in `twctl`, never in Explorer.
//!
//! Verbs report into process-wide state (`created`, `confirm`); `main` shows the outcome
//! once, after the verb returned ([`finish`]).

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, COLOR_WINDOW, CreateFontW,
    CreateSolidBrush, DEFAULT_CHARSET, DT_CALCRECT, DT_CENTER, DT_NOPREFIX, DT_SINGLELINE,
    DT_VCENTER, DeleteObject, DrawTextW, EndPaint, FillRect, GetDC, GetMonitorInfoW,
    GetSysColorBrush, HBRUSH, HFONT, HGDIOBJ, InvalidateRect, MONITOR_DEFAULTTONEAREST,
    MONITORINFO, MonitorFromPoint, OUT_DEFAULT_PRECIS, PAINTSTRUCT, ReleaseDC, SelectObject,
    SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use windows::Win32::UI::WindowsAndMessaging::{
    BS_PUSHBUTTON, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    GetCursorPos, GetMessageW, GetSystemMetrics, HMENU, HWND_TOPMOST, IDC_ARROW, IDCANCEL,
    KillTimer, LoadCursorW, MSG, PostQuitMessage, RegisterClassW, SM_CXSCREEN, SM_CYSCREEN,
    SW_SHOWNORMAL, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SendMessageW, SetForegroundWindow,
    SetTimer, SetWindowPos, SetWindowTextW, ShowWindow, TranslateMessage, WA_INACTIVE,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_ACTIVATE, WM_CLOSE, WM_COMMAND, WM_CTLCOLORSTATIC,
    WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN, WM_PAINT, WM_RBUTTONDOWN, WM_SETFONT,
    WM_TIMER, WNDCLASSW, WS_BORDER, WS_CAPTION, WS_CHILD, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::ops::Result;
use crate::{ui, win};

/// How long the clipboard confirmation stays up.
const CONFIRM_MS: u32 = 1500;
/// A progress window appears when a job is still running after this long...
const PROGRESS_DELAY: Duration = Duration::from_millis(1500);
/// ...or after this long when it has more than [`BIG_JOB`] files.
const PROGRESS_DELAY_BIG: Duration = Duration::from_millis(400);
const BIG_JOB: usize = 3;

#[derive(Default)]
struct State {
    notify: bool,
    created: Vec<PathBuf>,
    confirm: Option<String>,
}

static STATE: Mutex<State> = Mutex::new(State {
    notify: false,
    created: Vec::new(),
    confirm: None,
});

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Turns the user-visible feedback on (menu invocations).
pub fn set_notify(on: bool) {
    state().notify = on;
}

pub fn notify() -> bool {
    state().notify
}

/// Records files a verb created or changed; [`finish`] selects them in Explorer.
pub fn created(paths: impl IntoIterator<Item = PathBuf>) {
    let mut st = state();
    for p in paths {
        if !st.created.contains(&p) {
            st.created.push(p);
        }
    }
}

/// Records the text of the confirmation shown for verbs that produce no file (clipboard,
/// lock screen, folder icon...).
pub fn confirm(text: impl Into<String>) {
    state().confirm = Some(text.into());
}

/// Snapshot of the recorded files without clearing them (tests).
#[cfg(test)]
pub fn recorded() -> Vec<PathBuf> {
    state().created.clone()
}

fn take_created() -> Vec<PathBuf> {
    std::mem::take(&mut state().created)
}

fn take_confirm() -> Option<String> {
    state().confirm.take()
}

/// Shows the outcome of a verb when `--notify` is on: the error message (blocking, first),
/// then the created files selected in Explorer, or the confirmation for verbs without
/// output files.
pub fn finish(result: &Result<String>) {
    if !notify() {
        return;
    }
    if let Err(e) = result {
        win::message(e, true);
    }
    let files = take_created();
    if !files.is_empty() {
        win::reveal(&files);
    } else if result.is_ok()
        && let Some(text) = take_confirm()
    {
        confirm_window(&text);
    }
}

// ---------------------------------------------------------------- shared window helpers

fn module() -> windows::Win32::Foundation::HINSTANCE {
    // SAFETY: the current module handle.
    windows::Win32::Foundation::HINSTANCE(
        unsafe { GetModuleHandleW(PCWSTR::null()) }
            .map(|h| h.0)
            .unwrap_or_default(),
    )
}

fn scale(px: i32) -> i32 {
    use windows::Win32::UI::HiDpi::GetDpiForSystem;
    // SAFETY: no arguments.
    let dpi = unsafe { GetDpiForSystem() }.max(96) as i32;
    px * dpi / 96
}

/// Segoe UI at roughly 9 pt (never freed: the process is short-lived).
fn font() -> HFONT {
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
}

fn register(class: PCWSTR, proc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT) {
    // SAFETY: a plain window class; registering twice fails harmlessly.
    unsafe {
        let wc = WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: module(),
            lpszClassName: class,
            hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as isize as *mut _),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc);
    }
}

/// Shows `hwnd` on top and takes the foreground. SWP_SHOWWINDOW makes it visible even when
/// the process was started with a hidden show command (the first `ShowWindow` call of a
/// process obeys the STARTUPINFO instead of its argument).
fn show_topmost(hwnd: HWND) {
    // SAFETY: `hwnd` is a live window of this thread.
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
        let _ = SetForegroundWindow(hwnd);
    }
}

fn pump() {
    let mut msg = MSG::default();
    // SAFETY: a standard message loop on the thread that owns the window.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn rgb(r: u32, g: u32, b: u32) -> COLORREF {
    COLORREF(r | (g << 8) | (b << 16))
}

/// Apps use the dark theme (Settings > Personalization > Colors).
fn dark_mode() -> bool {
    windows_registry::CURRENT_USER
        .open(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|k| k.get_u32("AppsUseLightTheme"))
        .is_ok_and(|v| v == 0)
}

fn to_wide(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

// ---------------------------------------------------------------- confirmation window

struct ConfirmState {
    text: Vec<u16>,
    font: HFONT,
    dark: bool,
}

thread_local! {
    static CONFIRM: RefCell<Option<ConfirmState>> = const { RefCell::new(None) };
}

const CONFIRM_CLASS: PCWSTR = w!("ThumbwrightConfirm");

unsafe extern "system" fn confirm_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            // SAFETY: paint the window's own client area during WM_PAINT.
            unsafe {
                let hdc = BeginPaint(hwnd, &mut ps);
                let mut r = RECT::default();
                let _ = GetClientRect(hwnd, &mut r);
                CONFIRM.with(|c| {
                    if let Some(s) = c.borrow_mut().as_mut() {
                        let (bg, fg) = if s.dark {
                            (rgb(0x2b, 0x2b, 0x2b), rgb(0xff, 0xff, 0xff))
                        } else {
                            (rgb(0xf9, 0xf9, 0xf9), rgb(0x1a, 0x1a, 0x1a))
                        };
                        let brush = CreateSolidBrush(bg);
                        FillRect(hdc, &r, brush);
                        let _ = DeleteObject(HGDIOBJ(brush.0));
                        SetBkMode(hdc, TRANSPARENT);
                        SetTextColor(hdc, fg);
                        SelectObject(hdc, HGDIOBJ(s.font.0));
                        DrawTextW(
                            hdc,
                            &mut s.text,
                            &mut r,
                            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                        );
                    }
                });
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_TIMER | WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
            // SAFETY: the window's own handle.
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 == usize::from(VK_ESCAPE.0) => {
            // SAFETY: the window's own handle.
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        // Clicking anywhere else dismisses it too.
        WM_ACTIVATE if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE => {
            // SAFETY: the window's own handle.
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: ends this thread's message loop.
            unsafe {
                let _ = KillTimer(Some(hwnd), 1);
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        // SAFETY: default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Width in pixels of `text` in `font`.
fn text_width(text: &[u16], font: HFONT) -> i32 {
    let mut r = RECT::default();
    let mut buf = text.to_vec();
    // SAFETY: measuring on the screen DC, which is released again.
    unsafe {
        let dc = GetDC(None);
        let old = SelectObject(dc, HGDIOBJ(font.0));
        DrawTextW(
            dc,
            &mut buf,
            &mut r,
            DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
        );
        SelectObject(dc, old);
        ReleaseDC(None, dc);
    }
    r.right - r.left
}

/// A small topmost window near the cursor that closes itself after ~1.5 s, on a click or
/// on Esc. Returns when it is gone.
pub fn confirm_window(text: &str) {
    ui::dpi_aware();
    register(CONFIRM_CLASS, confirm_proc);
    let font = font();
    let wide = to_wide(text);
    let (width, height) = (
        (text_width(&wide, font) + scale(48)).clamp(scale(200), scale(640)),
        scale(44),
    );
    let (x, y) = near_cursor(width, height);
    CONFIRM.with(|c| {
        *c.borrow_mut() = Some(ConfirmState {
            text: wide,
            font,
            dark: dark_mode(),
        });
    });
    // SAFETY: a popup window on this thread, destroyed by its own procedure.
    unsafe {
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            CONFIRM_CLASS,
            w!("Thumbwright"),
            WS_POPUP | WS_BORDER,
            x,
            y,
            width,
            height,
            None,
            None,
            Some(module()),
            None,
        ) else {
            return;
        };
        SetTimer(Some(hwnd), 1, CONFIRM_MS, None);
        show_topmost(hwnd);
    }
    pump();
}

/// Top-left for a `w` x `h` window just below-right of the cursor, kept inside the work
/// area of the monitor the cursor is on.
fn near_cursor(w: i32, h: i32) -> (i32, i32) {
    let mut p = POINT::default();
    // SAFETY: plain out-parameter calls.
    let work = unsafe {
        let _ = GetCursorPos(&mut p);
        let mon = MonitorFromPoint(p, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(mon, &mut info).as_bool() {
            info.rcWork
        } else {
            RECT {
                left: 0,
                top: 0,
                right: GetSystemMetrics(SM_CXSCREEN),
                bottom: GetSystemMetrics(SM_CYSCREEN),
            }
        }
    };
    place_near(
        p.x,
        p.y,
        w,
        h,
        (work.left, work.top, work.right, work.bottom),
    )
}

/// Pure placement rule behind [`near_cursor`]: offset from the cursor, clamped into
/// `work` = (left, top, right, bottom).
fn place_near(cx: i32, cy: i32, w: i32, h: i32, work: (i32, i32, i32, i32)) -> (i32, i32) {
    let (left, top, right, bottom) = work;
    let x = (cx + 12).min(right - w).max(left);
    let y = (cy + 16).min(bottom - h).max(top);
    (x, y)
}

// ---------------------------------------------------------------- progress window

/// Shared progress of one batch: the workers count files, the window shows them and sets
/// `cancel`.
pub struct Progress {
    total: usize,
    done: AtomicUsize,
    cancel: AtomicBool,
    finished: AtomicBool,
}

impl Progress {
    pub fn new(total: usize) -> Arc<Self> {
        Arc::new(Self {
            total,
            done: AtomicUsize::new(0),
            cancel: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        })
    }

    /// One more file is done (successfully or not).
    pub fn step(&self) {
        self.done.fetch_add(1, Ordering::Relaxed);
    }

    /// Asks the workers to stop before their next file (Cancel button).
    pub fn cancel_now(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// True once Cancel was pressed.
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Every worker returned; the window closes.
    pub fn finish(&self) {
        self.finished.store(true, Ordering::Relaxed);
    }

    fn done(&self) -> usize {
        self.done.load(Ordering::Relaxed)
    }

    fn finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }
}

/// Label of the progress window: `Converting 12 files... 4 of 12`.
fn progress_text(label: &str, total: usize, done: usize) -> String {
    if total <= 1 {
        format!("{label} 1 file...")
    } else {
        format!("{label} {total} files... {done} of {total}")
    }
}

struct ProgressUi {
    progress: Arc<Progress>,
    verb: String,
    label_text: String,
    label: HWND,
    button: HWND,
    bar: RECT,
    start: Instant,
}

thread_local! {
    static PROGRESS: RefCell<Option<ProgressUi>> = const { RefCell::new(None) };
}

const PROGRESS_CLASS: PCWSTR = w!("ThumbwrightProgress");

fn progress_cancel(ui: &mut ProgressUi) {
    if ui.progress.total > 1 && !ui.progress.cancelled() {
        ui.progress.cancel_now();
        // SAFETY: the window's own child controls.
        unsafe {
            let _ = windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow(ui.button, false);
            let _ = SetWindowTextW(
                ui.label,
                &HSTRING::from("Stopping after the current file..."),
            );
        }
    }
}

unsafe extern "system" fn progress_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            // SAFETY: paint the window's own client area during WM_PAINT.
            unsafe {
                let hdc = BeginPaint(hwnd, &mut ps);
                PROGRESS.with(|p| {
                    if let Some(ui) = p.borrow().as_ref() {
                        let trough = CreateSolidBrush(rgb(0xe6, 0xe6, 0xe6));
                        FillRect(hdc, &ui.bar, trough);
                        let _ = DeleteObject(HGDIOBJ(trough.0));
                        let width = ui.bar.right - ui.bar.left;
                        let (from, to) = if ui.progress.total > 1 {
                            let n = ui.progress.done().min(ui.progress.total);
                            (0, width * n as i32 / ui.progress.total as i32)
                        } else {
                            // One long file: a block sliding back and forth.
                            let t = (ui.start.elapsed().as_millis() % 1600) as i32;
                            let pos = if t < 800 { t } else { 1600 - t };
                            let block = width / 4;
                            let x = (width - block) * pos / 800;
                            (x, x + block)
                        };
                        let fill = CreateSolidBrush(rgb(0x00, 0x78, 0xd4));
                        let r = RECT {
                            left: ui.bar.left + from,
                            right: ui.bar.left + to,
                            ..ui.bar
                        };
                        FillRect(hdc, &r, fill);
                        let _ = DeleteObject(HGDIOBJ(fill.0));
                    }
                });
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC => {
            // SAFETY: the static control's DC passed by the system; stock brush.
            unsafe {
                SetBkMode(
                    windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut _),
                    TRANSPARENT,
                );
                LRESULT(GetSysColorBrush(COLOR_WINDOW).0 as isize)
            }
        }
        WM_TIMER => {
            let mut close = false;
            PROGRESS.with(|p| {
                if let Some(ui) = p.borrow_mut().as_mut() {
                    if ui.progress.finished() {
                        close = true;
                        return;
                    }
                    // SAFETY: repaint the bar; update the label when its text changed.
                    unsafe {
                        if !ui.progress.cancelled() {
                            let want = ui.next_label();
                            if want != ui.label_text {
                                let _ = SetWindowTextW(ui.label, &HSTRING::from(want.as_str()));
                                ui.label_text = want;
                            }
                        }
                        let _ = InvalidateRect(Some(hwnd), Some(&ui.bar), false);
                    }
                }
            });
            if close {
                // SAFETY: the window's own handle.
                let _ = unsafe { DestroyWindow(hwnd) };
            }
            LRESULT(0)
        }
        WM_COMMAND if (wparam.0 & 0xFFFF) as i32 == IDCANCEL.0 => {
            PROGRESS.with(|p| {
                if let Some(ui) = p.borrow_mut().as_mut() {
                    progress_cancel(ui);
                }
            });
            LRESULT(0)
        }
        WM_CLOSE => {
            PROGRESS.with(|p| {
                if let Some(ui) = p.borrow_mut().as_mut() {
                    progress_cancel(ui);
                }
            });
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: ends this thread's message loop.
            unsafe {
                let _ = KillTimer(Some(hwnd), 1);
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        // SAFETY: default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

impl ProgressUi {
    fn next_label(&self) -> String {
        progress_text(&self.verb, self.progress.total, self.progress.done())
    }
}

/// Runs on its own thread next to the workers: waits until the job is slow, then shows a
/// small topmost window ("Converting 12 files... 4 of 12") with a Cancel button, until the
/// workers finish. Returns without a window when the job ends first.
pub fn progress_window(verb: &str, progress: &Arc<Progress>) {
    let delay = if progress.total > BIG_JOB {
        PROGRESS_DELAY_BIG
    } else {
        PROGRESS_DELAY
    };
    let start = Instant::now();
    while start.elapsed() < delay {
        if progress.finished() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if progress.finished() {
        return;
    }
    ui::dpi_aware();
    register(PROGRESS_CLASS, progress_proc);
    let font = font();
    let (cw, ch) = (scale(380), scale(if progress.total > 1 { 100 } else { 66 }));
    // SAFETY: a captioned popup window with a label and (for several files) a button.
    unsafe {
        let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST,
            PROGRESS_CLASS,
            w!("Thumbwright"),
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
            (sw - cw) / 2,
            (sh - ch) / 3,
            cw,
            ch + scale(32),
            None,
            None,
            Some(module()),
            None,
        ) else {
            return;
        };
        let text = progress_text(verb, progress.total, progress.done());
        let label = control(
            hwnd,
            w!("STATIC"),
            &text,
            0,
            (scale(16), scale(12), cw - scale(32), scale(20)),
            100,
            font,
        );
        let bar = RECT {
            left: scale(16),
            top: scale(40),
            right: cw - scale(16),
            bottom: scale(40) + scale(10),
        };
        let button = if progress.total > 1 {
            control(
                hwnd,
                w!("BUTTON"),
                "Cancel",
                BS_PUSHBUTTON as u32 | WS_TABSTOP.0,
                (cw - scale(16) - scale(84), scale(62), scale(84), scale(26)),
                IDCANCEL.0,
                font,
            )
        } else {
            HWND::default()
        };
        PROGRESS.with(|p| {
            *p.borrow_mut() = Some(ProgressUi {
                progress: Arc::clone(progress),
                verb: verb.to_owned(),
                label_text: text,
                label,
                button,
                bar,
                start: Instant::now(),
            });
        });
        SetTimer(Some(hwnd), 1, 80, None);
        show_topmost(hwnd);
    }
    pump();
    PROGRESS.with(|p| *p.borrow_mut() = None);
}

fn control(
    parent: HWND,
    class: PCWSTR,
    text: &str,
    style: u32,
    rect: (i32, i32, i32, i32),
    id: i32,
    font: HFONT,
) -> HWND {
    // SAFETY: a child control of a live window; the id is passed as the menu handle.
    unsafe {
        let h = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            &HSTRING::from(text),
            WINDOW_STYLE(style) | WS_CHILD | WS_VISIBLE,
            rect.0,
            rect.1,
            rect.2,
            rect.3,
            Some(parent),
            Some(HMENU(id as isize as *mut _)),
            Some(module()),
            None,
        )
        .unwrap_or_default();
        SendMessageW(
            h,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_text_matches_the_spec_wording() {
        assert_eq!(
            progress_text("Converting", 12, 4),
            "Converting 12 files... 4 of 12"
        );
        assert_eq!(progress_text("Resizing", 1, 0), "Resizing 1 file...");
    }

    #[test]
    fn placement_stays_inside_the_work_area() {
        let work = (0, 0, 1920, 1040);
        assert_eq!(place_near(100, 100, 200, 44, work), (112, 116));
        assert_eq!(place_near(1900, 1030, 200, 44, work), (1720, 996));
        assert_eq!(place_near(-50, -50, 200, 44, (0, 0, 800, 600)), (0, 0));
    }

    #[test]
    fn progress_counts_and_cancels() {
        let p = Progress::new(3);
        assert!(!p.cancelled() && !p.finished());
        p.step();
        p.step();
        assert_eq!(p.done(), 2);
        p.cancel_now();
        assert!(p.cancelled());
        p.finish();
        assert!(p.finished());
    }
}
