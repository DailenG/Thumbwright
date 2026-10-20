//! Opens the classic Thumbwright menu on one file the way Explorer hosts it, for screenshots
//! (AGENTS.md, visual acceptance): `IShellExtInit` on a data object of the file,
//! `QueryContextMenu` into a popup menu, `TrackPopupMenuEx` on a window that forwards the menu
//! messages to `IContextMenu3::HandleMenuMsg2`. The submenu is opened with the keyboard
//! (Down, Right) by whoever captures it. Prints nothing; the command picked is not run.
//!
//! `cargo run -p tw-shell --example classic_popup -- [--at X,Y] <file>`
#![cfg(windows)]

use std::cell::RefCell;
use std::ffi::c_void;

use thumbwright_shell::DllGetClassObject;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, CoInitializeEx, IClassFactory, IDataObject,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Shell::{
    BHID_DataObject, CMF_NORMAL, IContextMenu, IContextMenu3, IShellExtInit, IShellItem,
    SHCreateItemFromParsingName,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow, RegisterClassW,
    SetForegroundWindow, SetProcessDPIAware, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_DRAWITEM, WM_INITMENUPOPUP, WM_MEASUREITEM, WM_MENUCHAR,
    WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{GUID, HSTRING, Interface, PCSTR, PCWSTR, w};

thread_local! {
    /// The handler the window forwards menu messages to, as Explorer does.
    static HANDLER: RefCell<Option<IContextMenu3>> = const { RefCell::new(None) };
}

unsafe extern "system" fn host_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if matches!(
        message,
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR
    ) {
        let forwarded = HANDLER.with(|h| {
            let handler = h.borrow();
            let handler = handler.as_ref()?;
            let mut result = LRESULT(0);
            // SAFETY: a live handler and a valid out-pointer.
            unsafe { handler.HandleMenuMsg2(message, wparam, lparam, Some(&mut result)) }
                .ok()
                .map(|()| result)
        });
        if let Some(result) = forwarded {
            return result;
        }
    }
    // SAFETY: the arguments are the ones this procedure was called with.
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

/// Opts the process's menus into the system's dark theme, as Explorer is (uxtheme ordinals 135
/// and 136, undocumented, skipped when missing).
fn enable_dark_menus() {
    // SAFETY: ordinal lookups in a system DLL, with the signatures their callers use.
    unsafe {
        let Ok(uxtheme) = LoadLibraryW(w!("uxtheme.dll")) else {
            return;
        };
        if let Some(f) = GetProcAddress(uxtheme, PCSTR(135 as *const u8)) {
            let f: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(f);
            f(1);
        }
        if let Some(f) = GetProcAddress(uxtheme, PCSTR(136 as *const u8)) {
            let f: unsafe extern "system" fn() = std::mem::transmute(f);
            f();
        }
    }
}

fn main() {
    let mut at = (600, 300);
    let mut file = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--at" {
            let value = args.next().expect("--at needs X,Y");
            let (x, y) = value.split_once(',').expect("--at needs X,Y");
            at = (x.parse().expect("X"), y.parse().expect("Y"));
        } else {
            file = Some(arg);
        }
    }
    let file = file.expect("usage: classic_popup [--at X,Y] <file>");

    // SAFETY: plain Win32 and COM calls on this thread, in the order Explorer makes them; every
    // object created is released or destroyed before returning.
    unsafe {
        let _ = SetProcessDPIAware();
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        enable_dark_menus();

        let clsid = GUID::from_u128(tw_core::ids::CLSID_CONTEXT_MENU.guid);
        let mut raw: *mut c_void = std::ptr::null_mut();
        DllGetClassObject(&clsid, &IClassFactory::IID, &mut raw)
            .ok()
            .expect("DllGetClassObject");
        let factory = IClassFactory::from_raw(raw);
        let menu: IContextMenu = factory.CreateInstance(None).expect("CreateInstance");
        let item: IShellItem =
            SHCreateItemFromParsingName(&HSTRING::from(file.as_str()), None).expect("shell item");
        let data: IDataObject = item
            .BindToHandler(None, &BHID_DataObject)
            .expect("data object");
        menu.cast::<IShellExtInit>()
            .expect("IShellExtInit")
            .Initialize(None, &data, None)
            .expect("Initialize");

        let hmenu = CreatePopupMenu().expect("popup menu");
        let used = menu.QueryContextMenu(hmenu, 0, 1, 0x7FFF, CMF_NORMAL).0;
        assert!(used > 0, "QueryContextMenu added nothing");
        HANDLER.with(|h| *h.borrow_mut() = menu.cast::<IContextMenu3>().ok());

        let module = GetModuleHandleW(PCWSTR::null()).expect("module");
        let class = w!("ThumbwrightClassicPopupHost");
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(host_proc),
            hInstance: HINSTANCE(module.0),
            lpszClassName: class,
            ..Default::default()
        });
        let host = CreateWindowExW(
            WINDOW_EX_STYLE(WS_EX_TOOLWINDOW.0),
            class,
            w!("Thumbwright classic popup host"),
            WINDOW_STYLE(WS_POPUP.0),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(HINSTANCE(module.0)),
            None,
        )
        .expect("host window");
        let _ = SetForegroundWindow(host);
        let _ = TrackPopupMenuEx(
            hmenu,
            (TPM_RETURNCMD | TPM_RIGHTBUTTON).0,
            at.0,
            at.1,
            host,
            None,
        );
        let _ = DestroyWindow(host);
        HANDLER.with(|h| *h.borrow_mut() = None);
        // The menu (and its submenus) is destroyed before the handler is dropped, like Explorer
        // does it; the handler frees the bitmap it composed when it is dropped.
        let _ = DestroyMenu(hmenu);
        drop(menu);
    }
}
