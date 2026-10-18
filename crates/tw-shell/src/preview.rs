//! Preview-pane handler: `IPreviewHandler` + `IInitializeWithStream` + `IObjectWithSite` +
//! `IOleWindow`, hosted by `prevhost.exe` (installer registration, spec 4.2).
//!
//! `DoPreview` returns at once: it reads the stream (size-gated from metadata, like the
//! thumbnail class), creates a child window in the pane and decodes on a background thread
//! through the shared tier pipeline, so a slow file never blocks the host's UI thread (or
//! Explorer, which waits on these calls). The window paints the decoded bitmap scaled down to
//! fit, or "No preview available" when decoding failed.

use std::cell::RefCell;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Instant;

use windows::Win32::Foundation::{
    E_FAIL, E_UNEXPECTED, ERROR_ALREADY_INITIALIZED, HWND, LPARAM, LRESULT, RECT, S_FALSE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, AlphaBlend, BITMAP, BLENDFUNCTION, BeginPaint, COLOR_GRAYTEXT,
    COLOR_WINDOW, CreateCompatibleDC, DEFAULT_GUI_FONT, DT_CENTER, DT_SINGLELINE, DT_VCENTER,
    DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, GetObjectW, GetStockObject, GetSysColor,
    GetSysColorBrush, HBITMAP, HDC, HGDIOBJ, InvalidateRect, PAINTSTRUCT, SelectObject, SetBkMode,
    SetTextColor, TRANSPARENT,
};
use windows::Win32::System::Com::IStream;
use windows::Win32::System::Ole::{
    IObjectWithSite, IObjectWithSite_Impl, IOleWindow, IOleWindow_Impl,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus};
use windows::Win32::UI::Shell::PropertiesSystem::{
    IInitializeWithStream, IInitializeWithStream_Impl,
};
use windows::Win32::UI::Shell::{IPreviewHandler, IPreviewHandler_Impl, IPreviewHandlerFrame};
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetClientRect,
    GetWindowLongPtrW, IDC_ARROW, LoadCursorW, MSG, MoveWindow, RegisterClassExW, SetParent,
    SetWindowLongPtrW, UnregisterClassW, WM_ERASEBKGND, WM_NCCREATE, WM_NCDESTROY, WM_PAINT,
    WM_PRINTCLIENT, WNDCLASSEXW, WS_CHILD, WS_CLIPSIBLINGS, WS_EX_NOPARENTNOTIFY, WS_VISIBLE,
};
use windows::core::{GUID, HRESULT, HSTRING, IUnknown, Interface, PCWSTR, Ref, Result, implement};

use tw_core::config::Settings;
use tw_core::pipeline::Engine;
use tw_core::{ids, limits, log};

use crate::ModuleRef;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Loading,
    Ready,
    Failed,
}

struct Inner {
    phase: Phase,
    /// An `HBITMAP` owned by this struct (stored as an integer so the struct is `Send`).
    bitmap: Option<isize>,
    /// Set (under the lock) before the window is destroyed; a decode thread that finishes
    /// later discards its result instead of touching the window.
    abandoned: bool,
}

/// State shared between the window procedure, the owner and the decode thread.
struct Shared(Mutex<Inner>);

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The preview child window and everything that must die with it.
struct View {
    hwnd: HWND,
    class: HSTRING,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for View {
    fn drop(&mut self) {
        let bitmap = {
            let mut s = self.shared.lock();
            s.abandoned = true;
            s.bitmap.take()
        };
        // SAFETY: the window and class are ours; the bitmap is no longer reachable by the
        // window proc once the window is gone (same thread), nor by the decode thread
        // (it saw `abandoned`).
        unsafe {
            let _ = DestroyWindow(self.hwnd);
            let _ = UnregisterClassW(&self.class, Some(crate::module_handle()));
            if let Some(b) = bitmap {
                let _ = DeleteObject(HGDIOBJ(b as *mut _));
            }
        }
        if let Some(t) = self.thread.take() {
            if t.is_finished() {
                let _ = t.join();
            } else {
                // The decode thread still runs this DLL's code: never unload under it.
                tw_core::pipeline::pin_this_module();
            }
        }
    }
}

#[derive(Default)]
struct State {
    stream: Option<IStream>,
    parent: HWND,
    rect: RECT,
    site: Option<IUnknown>,
    view: Option<View>,
}

#[implement(IPreviewHandler, IInitializeWithStream, IObjectWithSite, IOleWindow)]
pub(crate) struct PreviewHandler {
    state: RefCell<State>,
    _module: ModuleRef,
}

impl PreviewHandler {
    pub(crate) fn new() -> Self {
        log::init("dll");
        Self {
            state: RefCell::new(State::default()),
            _module: ModuleRef::new(),
        }
    }
}

impl IInitializeWithStream_Impl for PreviewHandler_Impl {
    fn Initialize(&self, stream: Ref<IStream>, _mode: u32) -> Result<()> {
        let stream = stream.ok()?.clone();
        let mut st = self.state.borrow_mut();
        if st.stream.is_some() {
            return Err(HRESULT::from_win32(ERROR_ALREADY_INITIALIZED.0).into());
        }
        st.stream = Some(stream);
        Ok(())
    }
}

impl IObjectWithSite_Impl for PreviewHandler_Impl {
    fn SetSite(&self, site: Ref<IUnknown>) -> Result<()> {
        self.state.borrow_mut().site = site.as_ref().cloned();
        Ok(())
    }

    fn GetSite(&self, riid: *const GUID, ppv: *mut *mut std::ffi::c_void) -> Result<()> {
        if riid.is_null() || ppv.is_null() {
            return Err(windows::Win32::Foundation::E_POINTER.into());
        }
        // SAFETY: checked non-null; COM out-parameter contract.
        unsafe { *ppv = std::ptr::null_mut() };
        let site = self.state.borrow().site.clone().ok_or(E_FAIL)?;
        // SAFETY: riid/ppv valid per the COM contract.
        unsafe { site.query(riid, ppv).ok() }
    }
}

impl IOleWindow_Impl for PreviewHandler_Impl {
    fn GetWindow(&self) -> Result<HWND> {
        self.state
            .borrow()
            .view
            .as_ref()
            .map(|v| v.hwnd)
            .ok_or_else(|| E_UNEXPECTED.into())
    }

    fn ContextSensitiveHelp(&self, _enter: windows::core::BOOL) -> Result<()> {
        Err(windows::Win32::Foundation::E_NOTIMPL.into())
    }
}

fn width(r: &RECT) -> i32 {
    r.right - r.left
}

fn height(r: &RECT) -> i32 {
    r.bottom - r.top
}

impl IPreviewHandler_Impl for PreviewHandler_Impl {
    fn SetWindow(&self, hwnd: HWND, prc: *const RECT) -> Result<()> {
        if prc.is_null() || hwnd.is_invalid() {
            return Err(windows::Win32::Foundation::E_INVALIDARG.into());
        }
        let mut st = self.state.borrow_mut();
        st.parent = hwnd;
        // SAFETY: checked non-null.
        st.rect = unsafe { *prc };
        if let Some(v) = &st.view {
            // SAFETY: re-parenting and moving our own child window.
            unsafe {
                SetParent(v.hwnd, Some(hwnd))?;
                MoveWindow(
                    v.hwnd,
                    st.rect.left,
                    st.rect.top,
                    width(&st.rect),
                    height(&st.rect),
                    true,
                )?;
            }
        }
        Ok(())
    }

    fn SetRect(&self, prc: *const RECT) -> Result<()> {
        if prc.is_null() {
            return Err(windows::Win32::Foundation::E_INVALIDARG.into());
        }
        let mut st = self.state.borrow_mut();
        // SAFETY: checked non-null.
        st.rect = unsafe { *prc };
        if let Some(v) = &st.view {
            // SAFETY: moving our own child window.
            unsafe {
                MoveWindow(
                    v.hwnd,
                    st.rect.left,
                    st.rect.top,
                    width(&st.rect),
                    height(&st.rect),
                    true,
                )?;
            }
        }
        Ok(())
    }

    fn DoPreview(&self) -> Result<()> {
        let mut st = self.state.borrow_mut();
        let stream = st.stream.clone().ok_or(E_UNEXPECTED)?;
        if st.parent.is_invalid() {
            return Err(E_UNEXPECTED.into());
        }
        st.view = None;
        let loaded = load(&stream).map_err(|cause| {
            log::error(&format!("DoPreview: {cause}"));
            windows::core::Error::from(E_FAIL)
        })?;
        let shared = Arc::new(Shared(Mutex::new(Inner {
            phase: Phase::Loading,
            bitmap: None,
            abandoned: false,
        })));
        let (class, hwnd) = create_window(st.parent, &st.rect, &shared)?;
        let mut view = View {
            hwnd,
            class,
            shared: Arc::clone(&shared),
            thread: None,
        };
        view.thread = spawn_decode(loaded, hwnd, shared);
        if view.thread.is_none() {
            view.shared.lock().phase = Phase::Failed;
        }
        st.view = Some(view);
        Ok(())
    }

    fn Unload(&self) -> Result<()> {
        let mut st = self.state.borrow_mut();
        st.view = None;
        st.stream = None;
        Ok(())
    }

    fn SetFocus(&self) -> Result<()> {
        // Copy the handle out: focus changes send messages that may re-enter the handler.
        let hwnd = self.state.borrow().view.as_ref().map(|v| v.hwnd);
        if let Some(hwnd) = hwnd {
            // SAFETY: focusing our own child window.
            unsafe { SetFocus(Some(hwnd))? };
        }
        Ok(())
    }

    fn QueryFocus(&self) -> Result<HWND> {
        // SAFETY: plain query.
        let h = unsafe { GetFocus() };
        if h.is_invalid() {
            Err(E_FAIL.into())
        } else {
            Ok(h)
        }
    }

    fn TranslateAccelerator(&self, msg: *const MSG) -> Result<()> {
        let frame = self
            .state
            .borrow()
            .site
            .as_ref()
            .and_then(|s| s.cast::<IPreviewHandlerFrame>().ok());
        match frame {
            // SAFETY: forwarding the host's message pointer unchanged.
            Some(f) => unsafe { f.TranslateAccelerator(msg) },
            None => Err(S_FALSE.into()),
        }
    }
}

/// The stream's bytes plus what the decode needs.
struct Loaded {
    data: Vec<u8>,
    ext: Option<String>,
    edge: u32,
    /// `VerboseLog`: whether the decode logs a success line, and when the request began.
    verbose: bool,
    started: Instant,
}

/// Applies the same metadata gates as the thumbnail class (type enabled, size limits) and
/// reads the stream.
fn load(stream: &IStream) -> std::result::Result<Loaded, String> {
    let started = Instant::now();
    let settings = Settings::load(&crate::install_dir());
    let info = crate::thumbnail::stat(stream).map_err(|e| format!("stat: {e}"))?;
    let ext = Path::new(&info.name)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase());
    if ext.as_deref().is_some_and(|e| !settings.type_enabled(e)) {
        return Err("type disabled".into());
    }
    if info.size > settings.max_file_bytes() || info.size > limits::MAX_INPUT_BYTES {
        return Err("file exceeds max file size".into());
    }
    let data = crate::thumbnail::read_all(stream, info.size).map_err(|e| format!("read: {e}"))?;
    Ok(Loaded {
        data,
        ext,
        edge: settings.max_thumb_px,
        verbose: settings.verbose_log,
        started,
    })
}

static CLASS_SEQ: AtomicU32 = AtomicU32::new(0);

/// Registers a window class private to this preview and creates the child window. One
/// class per preview keeps the DLL free of global window state: the class goes away with
/// the window, long before the DLL can unload.
fn create_window(parent: HWND, rect: &RECT, shared: &Arc<Shared>) -> Result<(HSTRING, HWND)> {
    let class = HSTRING::from(format!(
        "{}.PreviewWnd.{}.{}",
        ids::PRODUCT_NAME,
        std::process::id(),
        CLASS_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let hinst = crate::module_handle();
    // SAFETY: standard class registration and window creation; `wnd_proc` lives in this
    // module, which `ModuleRef` keeps loaded while the window exists.
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            return Err(windows::core::Error::from_thread());
        }
        let hwnd = CreateWindowExW(
            WS_EX_NOPARENTNOTIFY,
            &class,
            PCWSTR::null(),
            WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
            rect.left,
            rect.top,
            width(rect),
            height(rect),
            Some(parent),
            None,
            Some(hinst),
            Some(Arc::as_ptr(shared).cast()),
        );
        match hwnd {
            Ok(h) => Ok((class, h)),
            Err(e) => {
                let _ = UnregisterClassW(&class, Some(hinst));
                Err(e)
            }
        }
    }
}

fn spawn_decode(loaded: Loaded, hwnd: HWND, shared: Arc<Shared>) -> Option<JoinHandle<()>> {
    // The raw handle value crosses threads as an integer; every use is guarded by
    // `abandoned`, set before the window is destroyed.
    let hwnd_raw = hwnd.0 as isize;
    std::thread::Builder::new()
        .name("tw-preview".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let engine = Engine::new(crate::install_dir().join(ids::WORKER_EXE_NAME));
                let edge = loaded.edge.max(16);
                let size = loaded.data.len();
                engine
                    .thumbnail_traced(Arc::new(loaded.data), loaded.ext.as_deref(), edge)
                    .map_err(|e| e.to_string())
                    .and_then(|(t, trace)| {
                        crate::bitmap::create(&t)
                            .inspect(|_| {
                                log::verbose(loaded.verbose, || {
                                    format!(
                                        "DoPreview: ok class=preview ext={} size={size} req={edge} {trace} ms={} out={}x{}",
                                        loaded.ext.as_deref().unwrap_or("?"),
                                        loaded.started.elapsed().as_millis(),
                                        t.width,
                                        t.height
                                    )
                                });
                            })
                            .map_err(|e| e.to_string())
                    })
            }))
            .unwrap_or_else(|_| Err("panic".into()));
            let mut inner = shared.lock();
            match result {
                Ok(bmp) if !inner.abandoned => {
                    inner.bitmap = Some(bmp.0 as isize);
                    inner.phase = Phase::Ready;
                }
                Ok(bmp) => {
                    // SAFETY: a bitmap we just created and nobody else references.
                    let _ = unsafe { DeleteObject(HGDIOBJ(bmp.0)) };
                }
                Err(cause) => {
                    log::error(&format!("preview decode failed: {cause}"));
                    inner.phase = Phase::Failed;
                }
            }
            if !inner.abandoned {
                // SAFETY: the window exists while `abandoned` is unset (checked under the
                // lock that its destruction also takes); InvalidateRect is thread-safe.
                unsafe {
                    let _ = InvalidateRect(Some(HWND(hwnd_raw as *mut _)), None, false);
                }
            }
        })
        .ok()
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: GWLP_USERDATA holds the `Shared` pointer passed at creation; the owning `View`
    // keeps the Arc alive until after the window is destroyed (WM_NCDESTROY clears it).
    unsafe {
        match msg {
            WM_NCCREATE => {
                let cs = &*(lparam.0 as *const CREATESTRUCTW);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_NCDESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                paint(hwnd, hdc);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_PRINTCLIENT => {
                paint(hwnd, HDC(wparam.0 as *mut _));
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// Fits a `bw` x `bh` image into `avail_w` x `avail_h`, shrinking only, centred in the
/// `avail` rectangle at `(left, top)`.
fn fit(bw: i32, bh: i32, left: i32, top: i32, avail_w: i32, avail_h: i32) -> (i32, i32, i32, i32) {
    let scale = (f64::from(avail_w) / f64::from(bw))
        .min(f64::from(avail_h) / f64::from(bh))
        .min(1.0);
    let dw = ((f64::from(bw) * scale) as i32).max(1);
    let dh = ((f64::from(bh) * scale) as i32).max(1);
    (left + (avail_w - dw) / 2, top + (avail_h - dh) / 2, dw, dh)
}

/// Paints the preview into `hdc` (the window's DC for WM_PAINT, the caller's for
/// WM_PRINTCLIENT).
///
/// # Safety
/// `hwnd` must be a live preview window; `hdc` a valid device context.
unsafe fn paint(hwnd: HWND, hdc: HDC) {
    // SAFETY: GDI calls on a valid DC; selected objects are restored and freed.
    unsafe {
        let mut client = RECT::default();
        if GetClientRect(hwnd, &mut client).is_err() {
            return;
        }
        FillRect(hdc, &client, GetSysColorBrush(COLOR_WINDOW));
        let shared = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Shared;
        let Some(shared) = shared.as_ref() else {
            return;
        };
        let inner = shared.lock();
        match inner.phase {
            Phase::Loading => {}
            Phase::Failed => {
                let old = SelectObject(hdc, GetStockObject(DEFAULT_GUI_FONT));
                SetBkMode(hdc, TRANSPARENT);
                SetTextColor(
                    hdc,
                    windows::Win32::Foundation::COLORREF(GetSysColor(COLOR_GRAYTEXT)),
                );
                let mut text: Vec<u16> = "No preview available".encode_utf16().collect();
                DrawTextW(
                    hdc,
                    &mut text,
                    &mut client,
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                );
                SelectObject(hdc, old);
            }
            Phase::Ready => {
                let Some(raw) = inner.bitmap else { return };
                let bmp = HBITMAP(raw as *mut _);
                let mut bm = BITMAP::default();
                if GetObjectW(
                    HGDIOBJ(bmp.0),
                    size_of::<BITMAP>() as i32,
                    Some((&raw mut bm).cast()),
                ) == 0
                {
                    return;
                }
                let (bw, bh) = (bm.bmWidth, bm.bmHeight.abs());
                if bw <= 0 || bh <= 0 {
                    return;
                }
                let (x, y, dw, dh) = fit(
                    bw,
                    bh,
                    client.left,
                    client.top,
                    width(&client).max(1),
                    height(&client).max(1),
                );
                let mem = CreateCompatibleDC(Some(hdc));
                let old = SelectObject(mem, HGDIOBJ(bmp.0));
                let blend = BLENDFUNCTION {
                    BlendOp: AC_SRC_OVER as u8,
                    BlendFlags: 0,
                    SourceConstantAlpha: 255,
                    AlphaFormat: AC_SRC_ALPHA as u8,
                };
                let _ = AlphaBlend(hdc, x, y, dw, dh, mem, 0, 0, bw, bh, blend);
                SelectObject(mem, old);
                let _ = DeleteDC(mem);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_shrinks_to_the_window_and_centres() {
        // 400x200 into 100x100: scale 0.25, centred vertically.
        assert_eq!(fit(400, 200, 0, 0, 100, 100), (0, 25, 100, 50));
    }

    #[test]
    fn fit_never_enlarges() {
        assert_eq!(fit(10, 10, 5, 5, 100, 100), (5 + 45, 5 + 45, 10, 10));
    }

    #[test]
    fn fit_keeps_a_visible_pixel_for_extreme_aspect_ratios() {
        let (_, _, w, h) = fit(10_000, 1, 0, 0, 50, 50);
        assert_eq!((w, h), (50, 1));
    }
}
