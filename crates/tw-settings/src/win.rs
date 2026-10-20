//! Thin helpers over raw Win32 controls: wide strings, messages, text, check state, list
//! views, message boxes. Every function takes handles of windows this process owns.

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Controls::{
    LIST_VIEW_ITEM_FLAGS, LIST_VIEW_ITEM_STATE_FLAGS, LVCF_TEXT, LVCF_WIDTH, LVCOLUMNW, LVIF_STATE,
    LVIF_TEXT, LVIS_STATEIMAGEMASK, LVITEMW, LVM_DELETEALLITEMS, LVM_ENSUREVISIBLE, LVM_GETCOLUMNW,
    LVM_GETITEMCOUNT, LVM_GETITEMTEXTW, LVM_GETNEXTITEM, LVM_INSERTCOLUMNW, LVM_INSERTITEMW,
    LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMSTATE, LVNI_SELECTED,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BM_GETCHECK, BM_SETCHECK, GetWindowTextW, IDYES, MB_DEFBUTTON2, MB_ICONERROR,
    MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_YESNO, SendMessageW, SetWindowTextW, WM_SETFONT,
};
use windows::core::PCWSTR;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn send(h: HWND, msg: u32, w: usize, l: isize) -> isize {
    // SAFETY: `h` is a window of this process; callers that pass a pointer in `l` keep the
    // pointee alive and correctly typed for `msg` for the duration of this synchronous call.
    unsafe { SendMessageW(h, msg, Some(WPARAM(w)), Some(LPARAM(l))).0 }
}

pub fn set_font(h: HWND, font: isize) {
    send(h, WM_SETFONT, font as usize, 1);
}

pub fn set_text(h: HWND, text: &str) {
    // SAFETY: valid window and NUL-terminated string.
    let _ = unsafe { SetWindowTextW(h, PCWSTR(wide(text).as_ptr())) };
}

pub fn get_text(h: HWND) -> String {
    let mut buf = [0u16; 512];
    // SAFETY: the buffer is writable for its whole length.
    let n = unsafe { GetWindowTextW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

pub fn set_checked(h: HWND, on: bool) {
    send(h, BM_SETCHECK, usize::from(on), 0);
}

pub fn is_checked(h: HWND) -> bool {
    send(h, BM_GETCHECK, 0, 0) == 1
}

/// A message box owned by `owner`, captioned with the window's name and showing the product
/// icon in its title bar.
fn message_box(
    owner: HWND,
    text: &str,
    style: windows::Win32::UI::WindowsAndMessaging::MESSAGEBOX_STYLE,
) -> bool {
    tw_core::message_box::show(Some(owner), text, "Thumbwright Workbench", style) == IDYES
}

pub fn info(owner: HWND, text: &str) {
    message_box(owner, text, MB_OK | MB_ICONINFORMATION);
}

pub fn error(owner: HWND, text: &str) {
    message_box(owner, text, MB_OK | MB_ICONERROR);
}

/// Yes/No question; No is the default button.
pub fn confirm(owner: HWND, text: &str) -> bool {
    message_box(owner, text, MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2)
}

// ---- list views (report mode, optional check boxes) ----

pub fn lv_set_extended(h: HWND, style: u32) {
    send(
        h,
        LVM_SETEXTENDEDLISTVIEWSTYLE,
        style as usize,
        style as isize,
    );
}

pub fn lv_add_column(h: HWND, index: i32, title: &str, width: i32) {
    let mut text = wide(title);
    let col = LVCOLUMNW {
        mask: LVCF_TEXT | LVCF_WIDTH,
        cx: width,
        pszText: windows::core::PWSTR(text.as_mut_ptr()),
        iSubItem: index,
        ..Default::default()
    };
    send(
        h,
        LVM_INSERTCOLUMNW,
        index as usize,
        std::ptr::from_ref(&col) as isize,
    );
}

pub fn lv_clear(h: HWND) {
    send(h, LVM_DELETEALLITEMS, 0, 0);
}

pub fn lv_count(h: HWND) -> usize {
    send(h, LVM_GETITEMCOUNT, 0, 0) as usize
}

/// Appends a row; `cells[0]` is the item text and the rest are sub-item texts.
pub fn lv_add_row(h: HWND, cells: &[&str]) {
    let index = lv_count(h);
    let mut first = wide(cells[0]);
    let item = LVITEMW {
        mask: LVIF_TEXT,
        iItem: index as i32,
        pszText: windows::core::PWSTR(first.as_mut_ptr()),
        ..Default::default()
    };
    send(h, LVM_INSERTITEMW, 0, std::ptr::from_ref(&item) as isize);
    for (sub, cell) in cells.iter().enumerate().skip(1) {
        let mut text = wide(cell);
        let item = LVITEMW {
            iItem: index as i32,
            iSubItem: sub as i32,
            pszText: windows::core::PWSTR(text.as_mut_ptr()),
            ..Default::default()
        };
        // LVM_SETITEMTEXTW
        send(h, 0x1000 + 116, index, std::ptr::from_ref(&item) as isize);
    }
}

/// The header text of column `index`, `None` when the list has no such column.
pub fn lv_column_title(h: HWND, index: i32) -> Option<String> {
    let mut buf = [0u16; 128];
    let mut col = LVCOLUMNW {
        mask: LVCF_TEXT,
        pszText: windows::core::PWSTR(buf.as_mut_ptr()),
        cchTextMax: buf.len() as i32,
        ..Default::default()
    };
    let ok = send(
        h,
        LVM_GETCOLUMNW,
        index as usize,
        std::ptr::from_mut(&mut col) as isize,
    );
    (ok != 0).then(|| {
        let len = buf.iter().position(|&u| u == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
    })
}

/// The text of cell (`row`, `column`).
pub fn lv_cell_text(h: HWND, row: usize, column: i32) -> String {
    let mut buf = [0u16; 256];
    let mut item = LVITEMW {
        iSubItem: column,
        pszText: windows::core::PWSTR(buf.as_mut_ptr()),
        cchTextMax: buf.len() as i32,
        ..Default::default()
    };
    let n = send(
        h,
        LVM_GETITEMTEXTW,
        row,
        std::ptr::from_mut(&mut item) as isize,
    );
    String::from_utf16_lossy(&buf[..usize::try_from(n).unwrap_or(0).min(buf.len())])
}

/// State image index 1 is "unchecked", 2 is "checked" (list views with check boxes).
pub fn lv_set_checked(h: HWND, index: usize, on: bool) {
    let item = LVITEMW {
        mask: LIST_VIEW_ITEM_FLAGS(LVIF_STATE.0),
        stateMask: LVIS_STATEIMAGEMASK,
        state: LIST_VIEW_ITEM_STATE_FLAGS(((if on { 2 } else { 1 }) as u32) << 12),
        ..Default::default()
    };
    send(
        h,
        LVM_SETITEMSTATE,
        index,
        std::ptr::from_ref(&item) as isize,
    );
}

pub fn lv_selected(h: HWND) -> Option<usize> {
    let i = send(h, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize);
    usize::try_from(i).ok()
}

pub fn lv_select(h: HWND, index: usize) {
    let item = LVITEMW {
        stateMask: LIST_VIEW_ITEM_STATE_FLAGS(0x3), // LVIS_FOCUSED | LVIS_SELECTED
        state: LIST_VIEW_ITEM_STATE_FLAGS(0x3),
        ..Default::default()
    };
    send(
        h,
        LVM_SETITEMSTATE,
        index,
        std::ptr::from_ref(&item) as isize,
    );
    send(h, LVM_ENSUREVISIBLE, index, 0);
}
