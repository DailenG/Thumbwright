//! `MessageBoxW` with the product icon in its title bar. A standard message box shows no
//! caption icon, so a thread-local CBT hook catches the box's window as it is created and
//! sets the calling module's icon resource 1 on it (the convention for branding message
//! boxes without writing a custom dialog). The hook lives only for the duration of the call.

use std::cell::Cell;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetClassNameW, GetSystemMetrics, HCBT_ACTIVATE, HHOOK, ICON_BIG, ICON_SMALL,
    IMAGE_ICON, LR_DEFAULTCOLOR, LR_SHARED, LoadImageW, MESSAGEBOX_RESULT, MESSAGEBOX_STYLE,
    MessageBoxW, SM_CXICON, SM_CXSMICON, SM_CYICON, SM_CYSMICON, SYSTEM_METRICS_INDEX,
    SendMessageW, SetWindowsHookExW, UnhookWindowsHookEx, WH_CBT, WM_SETICON,
};
use windows::core::{HSTRING, PCWSTR};

thread_local! {
    static HOOK: Cell<Option<HHOOK>> = const { Cell::new(None) };
}

/// Icon resource 1 of the running exe at the given system-metric size, or `None`.
fn product_icon(cx: SYSTEM_METRICS_INDEX, cy: SYSTEM_METRICS_INDEX) -> Option<isize> {
    // SAFETY: loads an icon resource of this module; shared, process-owned handle.
    unsafe {
        let module = GetModuleHandleW(PCWSTR::null()).ok()?;
        LoadImageW(
            Some(module.into()),
            PCWSTR(std::ptr::without_provenance(1)),
            IMAGE_ICON,
            GetSystemMetrics(cx),
            GetSystemMetrics(cy),
            LR_DEFAULTCOLOR | LR_SHARED,
        )
        .ok()
        .map(|h| h.0 as isize)
    }
}

unsafe extern "system" fn cbt(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HCBT_ACTIVATE as i32 {
        let hwnd = HWND(wparam.0 as *mut _);
        let mut class = [0u16; 16];
        // SAFETY: a live window handle passed by the system; bounded buffer.
        let n = unsafe { GetClassNameW(hwnd, &mut class) };
        // "#32770" is the dialog class a message box uses.
        if String::from_utf16_lossy(&class[..n.max(0) as usize]) == "#32770" {
            for (kind, (cx, cy)) in [
                (ICON_SMALL, (SM_CXSMICON, SM_CYSMICON)),
                (ICON_BIG, (SM_CXICON, SM_CYICON)),
            ] {
                if let Some(icon) = product_icon(cx, cy) {
                    // SAFETY: our own message box window; shared icon handle.
                    unsafe {
                        SendMessageW(
                            hwnd,
                            WM_SETICON,
                            Some(WPARAM(kind as usize)),
                            Some(LPARAM(icon)),
                        )
                    };
                }
            }
        }
    }
    let hook = HOOK.with(Cell::get);
    // SAFETY: forwards to the next hook in the chain with the original arguments.
    unsafe { CallNextHookEx(hook, code, wparam, lparam) }
}

/// `MessageBoxW(owner, text, caption, style)` with the product icon in the caption.
pub fn show(
    owner: Option<HWND>,
    text: &str,
    caption: &str,
    style: MESSAGEBOX_STYLE,
) -> MESSAGEBOX_RESULT {
    // SAFETY: a thread-local CBT hook on this thread only, removed right after the box
    // closes; the strings outlive the call.
    unsafe {
        let hook = SetWindowsHookExW(WH_CBT, Some(cbt), None, GetCurrentThreadId()).ok();
        HOOK.with(|h| h.set(hook));
        let result = MessageBoxW(owner, &HSTRING::from(text), &HSTRING::from(caption), style);
        if let Some(h) = hook {
            let _ = UnhookWindowsHookEx(h);
        }
        HOOK.with(|h| h.set(None));
        result
    }
}
