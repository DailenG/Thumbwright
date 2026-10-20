//! `twctl menu`: Thumbwright's own popup menu, the action behind the single Windows 11
//! "Thumbwright" entry. A native popup (`CreatePopupMenu` + `TrackPopupMenuEx`) built from the
//! same `tw_core::menu` tree the classic menu uses, so nested submenus, keyboard, accessibility
//! and the system look come for free. Items that do not apply to the selection are absent, not
//! greyed. The choice runs in this process, through the ordinary command dispatch.
//!
//! Dark mode: the process opts in through the undocumented `uxtheme` ordinals 135
//! (`SetPreferredAppMode`) and 136 (`FlushMenuThemes`), both looked up at run time and skipped
//! silently when a Windows build lacks them; the menu then follows `AppsUseLightTheme`.

use std::path::PathBuf;
use std::sync::mpsc;

use tw_core::config::Settings;
use tw_core::menu::{self as tree, Selection, Shown, Verb};
use tw_core::menu_header::{self, HeaderBitmap, Placement};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{DeleteObject, HBITMAP, HGDIOBJ, InvalidateRect};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_ICONONLY, SIIGBF_THUMBNAILONLY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    EnumThreadWindows, GWLP_USERDATA, GetClassNameW, GetCursorPos, GetWindowLongPtrW, HMENU,
    MF_POPUP, MF_SEPARATOR, MF_STRING, MSG, PM_REMOVE, PeekMessageW, PostMessageW, RegisterClassW,
    SetForegroundWindow, SetWindowLongPtrW, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_NULL, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{BOOL, PCSTR, PCWSTR, w};

use crate::ops::Result;

/// Posted to the owner window by the preview worker: `lparam` is the bitmap for the header's
/// box (an `HBITMAP`, to be freed by the receiver) and `wparam` is 1 when it is the file's icon
/// (the shell had no thumbnail), else 0. A menu's modal loop dispatches the messages of its
/// owner, so it arrives while the popup is open.
const WM_PREVIEW: u32 = WM_APP + 1;

/// `--at X,Y`: the screen position the popup opens at.
pub fn parse_at(s: &str) -> Result<(i32, i32)> {
    let bad = || format!("bad --at {s:?}: expected X,Y");
    let (x, y) = s.split_once(',').ok_or_else(bad)?;
    Ok((
        x.trim().parse().map_err(|_| bad())?,
        y.trim().parse().map_err(|_| bad())?,
    ))
}

/// A built popup: the menu, and the verb behind each command id (`id - 1` indexes `verbs`).
pub struct Popup {
    pub menu: HMENU,
    pub verbs: Vec<&'static Verb>,
    /// The header row's text, which the thumbnail row repeats as its own text.
    header: Option<String>,
    /// The composed header row, once it is on the menu. The menu does not own it.
    bitmap: Option<HeaderBitmap>,
}

impl Drop for Popup {
    fn drop(&mut self) {
        if let Some(bitmap) = &self.bitmap {
            bitmap.detach();
        }
        // SAFETY: the menu (and its submenus) is owned by this popup; the bitmap it showed
        // is freed after it, as a field.
        let _ = unsafe { DestroyMenu(self.menu) };
    }
}

fn winerr(e: windows::core::Error) -> String {
    e.to_string()
}

/// Appends `entries` to `menu`; `count` is the number of rows in it. A separator is only
/// written once an item follows it.
fn append(
    menu: HMENU,
    entries: &[Shown],
    verbs: &mut Vec<&'static Verb>,
    count: &mut u32,
    mut separator_due: bool,
) -> Result<()> {
    for entry in entries {
        match entry {
            Shown::Separator => separator_due = separator_due || *count > 0,
            Shown::Verb(verb) => {
                separator_before(menu, count, &mut separator_due)?;
                verbs.push(verb);
                let text = tree::menu_text(verb.label);
                // SAFETY: valid menu; the text is copied by the menu.
                unsafe {
                    AppendMenuW(
                        menu,
                        MF_STRING,
                        verbs.len(),
                        &windows::core::HSTRING::from(text),
                    )
                }
                .map_err(winerr)?;
                *count += 1;
            }
            Shown::Menu { label, items } => {
                // SAFETY: a fresh popup menu, owned by `menu` once appended or destroyed.
                let child = unsafe { CreatePopupMenu() }.map_err(winerr)?;
                let mut child_count = 0;
                append(child, items, verbs, &mut child_count, false)?;
                if child_count == 0 {
                    // SAFETY: `child` was created above and not attached.
                    let _ = unsafe { DestroyMenu(child) };
                    continue;
                }
                separator_before(menu, count, &mut separator_due)?;
                let text = tree::menu_text(label);
                // SAFETY: valid menus; MF_POPUP carries the submenu handle as the id.
                unsafe {
                    AppendMenuW(
                        menu,
                        MF_POPUP,
                        child.0 as usize,
                        &windows::core::HSTRING::from(text),
                    )
                }
                .map_err(winerr)?;
                *count += 1;
            }
        }
    }
    Ok(())
}

fn separator_before(menu: HMENU, count: &mut u32, due: &mut bool) -> Result<()> {
    if *due {
        // SAFETY: valid menu.
        unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null()) }.map_err(winerr)?;
        *count += 1;
        *due = false;
    }
    Ok(())
}

/// Builds the popup for `entries`, with `header` as its first row. The header has no command,
/// so a click on it just closes the popup. It is not disabled: a themed menu draws a disabled
/// row's bitmap greyed out, and the preview should keep its colours. It starts as a text row;
/// with a preview it becomes a bitmap item (see `Popup::reserve_preview` and
/// `tw_core::menu_header`).
pub fn build(entries: &[Shown], header: Option<&str>) -> Result<Popup> {
    // SAFETY: a fresh popup menu, owned by the returned `Popup`.
    let menu = unsafe { CreatePopupMenu() }.map_err(winerr)?;
    let mut popup = Popup {
        menu,
        verbs: Vec::new(),
        header: header.map(str::to_owned),
        bitmap: None,
    };
    let mut count = 0;
    if let Some(text) = header {
        // SAFETY: valid menu; the text is copied. A row with no command id.
        unsafe {
            AppendMenuW(
                menu,
                MF_STRING,
                0,
                &windows::core::HSTRING::from(tree::menu_text(text)),
            )
        }
        .map_err(winerr)?;
        count = 1;
    }
    append(
        menu,
        entries,
        &mut popup.verbs,
        &mut count,
        header.is_some(),
    )?;
    Ok(popup)
}

impl Popup {
    /// Turns the header row into a bitmap row with the header text and an empty box where the
    /// thumbnail goes, sized for the thumbnail's largest case. The menu is sized from this row
    /// when it opens, so [`Popup::set_preview`] can swap the thumbnail in later without the
    /// menu changing size. Does nothing when there is no header.
    fn reserve_preview(&mut self) {
        self.install(None);
    }

    /// Puts `bitmap` (premultiplied 32-bpp, freed here) into the header row's box, as
    /// `placement` says. Works before the menu is shown and while it is open (the caller
    /// repaints the open menu); the row keeps its size either way.
    fn set_preview(&mut self, bitmap: HBITMAP, placement: Placement) {
        self.install(Some((bitmap, placement)));
        // SAFETY: the bitmap is ours and was selected nowhere; composing copied it.
        let _ = unsafe { DeleteObject(HGDIOBJ(bitmap.0)) };
    }

    /// Composes the header row with `bitmap` (or an empty box) and makes row 0 show it,
    /// replacing the row composed before, if any. On failure the row stays as it was.
    fn install(&mut self, bitmap: Option<(HBITMAP, Placement)>) {
        let Some(row) = self
            .header
            .as_deref()
            .and_then(|text| menu_header::compose_boxed(text, bitmap, menu_header::system_dpi()))
        else {
            return;
        };
        // SAFETY: valid menu; row 0 is the header, and the bitmap is detached in `drop`,
        // before the menu is destroyed. A replaced bitmap is dropped only after the menu
        // shows the new one.
        let shown = unsafe {
            match &self.bitmap {
                Some(previous) => row.replace(self.menu, previous),
                None => row.attach(self.menu),
            }
        };
        if shown.is_ok() {
            self.bitmap = Some(row);
        }
    }
}

/// Asks the shell for the picture of `file` on a worker thread: its thumbnail, or when it has
/// none, its icon. The worker posts the result as [`WM_PREVIEW`] to the owner window, whose
/// handle the caller sends through the returned channel once the window exists (dropping the
/// sender means there will be none). A bitmap that cannot be delivered is freed by the worker;
/// when the shell has neither, nothing is posted and the header's box stays empty.
fn fetch_preview(file: &std::path::Path) -> mpsc::Sender<isize> {
    let (owner_tx, owner_rx) = mpsc::channel::<isize>();
    let edge = menu_header::thumbnail_edge(menu_header::system_dpi());
    let path: Vec<u16> = file
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let _ = std::thread::Builder::new()
        .name("tw-popup-preview".into())
        .spawn(move || {
            // SAFETY: STA for the shell item APIs on this thread; balanced uninit.
            unsafe {
                let inited = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
                let picture = SHCreateItemFromParsingName::<_, _, IShellItemImageFactory>(
                    PCWSTR(path.as_ptr()),
                    None,
                )
                .ok()
                .and_then(|f| {
                    let size = SIZE { cx: edge, cy: edge };
                    f.GetImage(size, SIIGBF_THUMBNAILONLY)
                        .map(|bitmap| (bitmap, false))
                        .or_else(|_| {
                            f.GetImage(size, SIIGBF_ICONONLY)
                                .map(|bitmap| (bitmap, true))
                        })
                        .ok()
                });
                if let Some((bitmap, icon)) = picture {
                    let delivered = owner_rx.recv().is_ok_and(|owner| {
                        PostMessageW(
                            Some(HWND(owner as *mut _)),
                            WM_PREVIEW,
                            WPARAM(usize::from(icon)),
                            LPARAM(bitmap.0 as isize),
                        )
                        .is_ok()
                    });
                    if !delivered {
                        let _ = DeleteObject(HGDIOBJ(bitmap.0));
                    }
                }
                if inited {
                    CoUninitialize();
                }
            }
        });
    owner_tx
}

/// The menu windows (`#32768`) of this thread, the open popup and its submenus.
fn open_menus() -> Vec<HWND> {
    unsafe extern "system" fn each(window: HWND, found: LPARAM) -> BOOL {
        let mut class = [0u16; 8];
        // SAFETY: a live window of this thread and a buffer of the size passed.
        let len = unsafe { GetClassNameW(window, &mut class) };
        if String::from_utf16_lossy(&class[..usize::try_from(len).unwrap_or(0)]) == "#32768" {
            // SAFETY: `found` is the vector `open_menus` passed, alive during the enumeration.
            unsafe { (*(found.0 as *mut Vec<HWND>)).push(window) };
        }
        true.into()
    }
    let mut found = Vec::new();
    // SAFETY: the callback only touches the window it is given and the vector.
    let _ = unsafe {
        EnumThreadWindows(
            GetCurrentThreadId(),
            Some(each),
            LPARAM((&raw mut found) as isize),
        )
    };
    found
}

/// Redraws every menu window of this thread, to show a header row that changed while open
/// (without erasing first, so nothing flickers).
fn repaint_open_menus() {
    for window in open_menus() {
        // SAFETY: a live window; the whole client area.
        let _ = unsafe { InvalidateRect(Some(window), None, false) };
    }
}

type DarkModeFn = unsafe extern "system" fn(i32) -> i32;
type FlushFn = unsafe extern "system" fn();

/// Opts this process's menus into the system's dark theme: `SetPreferredAppMode(AllowDark)`
/// (uxtheme ordinal 135) then `FlushMenuThemes` (ordinal 136). Undocumented, so each is
/// looked up and skipped quietly when missing.
fn enable_dark_menus() {
    const ALLOW_DARK: i32 = 1;
    // SAFETY: ordinal lookups in a system DLL; the signatures are those the callers of these
    // exports have used since Windows 10 1809 (an int argument, resp. none).
    unsafe {
        let Ok(uxtheme) = LoadLibraryW(w!("uxtheme.dll")) else {
            return;
        };
        if let Some(f) = GetProcAddress(uxtheme, PCSTR(135 as *const u8)) {
            let f: DarkModeFn = std::mem::transmute(f);
            f(ALLOW_DARK);
        }
        if let Some(f) = GetProcAddress(uxtheme, PCSTR(136 as *const u8)) {
            let f: FlushFn = std::mem::transmute(f);
            f();
        }
    }
}

/// The window procedure of the popup's owner: it receives the thumbnail while the menu is
/// open. The `Popup` it updates is behind `GWLP_USERDATA`, set by [`choose`] for as long as
/// the menu is open.
unsafe extern "system" fn owner_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_PREVIEW {
        let bitmap = HBITMAP(lparam.0 as *mut _);
        let placement = if wparam.0 != 0 {
            Placement::Icon
        } else {
            Placement::Thumbnail
        };
        // SAFETY: `choose` stores a pointer to its `Popup` for exactly the time the menu runs
        // on this thread, and touches the `Popup` only outside it.
        match unsafe { (GetWindowLongPtrW(window, GWLP_USERDATA) as *mut Popup).as_mut() } {
            Some(popup) => {
                popup.set_preview(bitmap, placement);
                repaint_open_menus();
            }
            None => {
                // SAFETY: ours, delivered to nobody.
                let _ = unsafe { DeleteObject(HGDIOBJ(bitmap.0)) };
            }
        }
        return LRESULT(0);
    }
    // SAFETY: the arguments are the ones this procedure was called with.
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

/// The hidden window that owns the popup: the menu needs a foreground owner to dismiss when
/// the user clicks away, and the owner receives the thumbnail ([`owner_proc`]).
fn create_owner() -> Result<HWND> {
    let class = w!("ThumbwrightPopupOwner");
    // SAFETY: a never-shown popup of a class registered here (registering twice fails
    // harmlessly and the first registration stays).
    unsafe {
        let module = GetModuleHandleW(PCWSTR::null()).map_err(winerr)?;
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(owner_proc),
            hInstance: HINSTANCE(module.0),
            lpszClassName: class,
            ..Default::default()
        });
        CreateWindowExW(
            WINDOW_EX_STYLE(WS_EX_TOOLWINDOW.0),
            class,
            w!("Thumbwright"),
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
    }
    .map_err(winerr)
}

/// The cursor position, the default place for the popup.
pub fn cursor() -> (i32, i32) {
    let mut p = Default::default();
    // SAFETY: valid out-pointer.
    let _ = unsafe { GetCursorPos(&mut p) };
    (p.x, p.y)
}

/// Shows the popup for `files` at `at` and returns the verb the user picked, or `None` when
/// they dismissed it (Esc, or a click anywhere else). The popup opens at once, with an empty
/// box in its header where the thumbnail goes; the thumbnail is swapped in when the shell
/// delivers it.
pub fn choose(
    settings: &Settings,
    files: &[PathBuf],
    at: (i32, i32),
) -> Result<Option<&'static Verb>> {
    // The DPI first, as the thumbnail is asked for at the size the screen shows it; then
    // before anything else, so the shell's work overlaps the menu build.
    crate::ui::dpi_aware();
    let preview = (files.len() == 1 && settings.menu_preview).then(|| fetch_preview(&files[0]));
    enable_dark_menus();
    let entries = tree::shown(settings, &Selection::of(files));
    let mut popup = build(&entries, Some(&tree::header_text(files)))?;
    if preview.is_some() {
        popup.reserve_preview();
    }
    let owner = create_owner()?;
    if let Some(owner_tx) = &preview {
        let _ = owner_tx.send(owner.0 as isize);
    }
    let menu = popup.menu;
    // SAFETY: `owner` and the menu are ours; the menu is modal until a choice or dismissal.
    // `popup` is not touched while the pointer is stored, and the pointer is cleared before
    // the menu is gone.
    let id = unsafe {
        SetWindowLongPtrW(owner, GWLP_USERDATA, (&raw mut popup) as isize);
        let _ = SetForegroundWindow(owner);
        let id = TrackPopupMenuEx(
            menu,
            (TPM_RETURNCMD | TPM_RIGHTBUTTON).0,
            at.0,
            at.1,
            owner,
            None,
        );
        SetWindowLongPtrW(owner, GWLP_USERDATA, 0);
        // A thumbnail that arrived too late is still queued: free it.
        let mut late = MSG::default();
        while PeekMessageW(&mut late, Some(owner), WM_PREVIEW, WM_PREVIEW, PM_REMOVE).as_bool() {
            let _ = DeleteObject(HGDIOBJ(late.lParam.0 as *mut _));
        }
        // The documented companion of a tray-style popup: lets the menu close cleanly.
        let _ = PostMessageW(Some(owner), WM_NULL, Default::default(), Default::default());
        let _ = DestroyWindow(owner);
        id.0
    };
    Ok(usize::try_from(id)
        .ok()
        .and_then(|id| id.checked_sub(1))
        .and_then(|i| popup.verbs.get(i).copied()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetMenuItemCount, GetMenuItemID, GetMenuItemInfoW, GetSubMenu, MENU_ITEM_MASK,
        MENUITEMINFOW, MFT_BITMAP, MFT_OWNERDRAW, MFT_STRING, MIIM_BITMAP, MIIM_FTYPE,
    };

    fn menu_for(files: &[&str], settings: &Settings) -> Popup {
        let files: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
        let entries = tree::shown(settings, &Selection::of(&files));
        build(&entries, Some("header")).unwrap()
    }

    fn rows(menu: HMENU) -> i32 {
        // SAFETY: a live menu owned by the test.
        unsafe { GetMenuItemCount(Some(menu)) }
    }

    #[test]
    fn at_parses_coordinates() {
        assert_eq!(parse_at("400,400"), Ok((400, 400)));
        assert_eq!(parse_at(" -1920 , 12 "), Ok((-1920, 12)));
        assert!(parse_at("400").is_err());
        assert!(parse_at("a,b").is_err());
    }

    #[test]
    fn the_popup_has_a_header_the_top_rows_and_a_command_per_verb() {
        let p = menu_for(&["a.png"], &Settings::default());
        // Header and separator, Convert/Resize/Rotate/Make smaller, separator, Copy text and
        // Copy, separator, Organise, separator, Details and Set as, separator, Workbench.
        assert_eq!(rows(p.menu), 16);
        // The header carries no command; submenus carry no command either.
        // SAFETY: reading ids of a live menu.
        unsafe {
            assert_eq!(GetMenuItemID(p.menu, 0), 0);
            assert_eq!(
                GetMenuItemID(p.menu, 2),
                u32::MAX,
                "a submenu row reports -1"
            );
            assert!(!GetSubMenu(p.menu, 2).is_invalid());
        }
        assert!(p.verbs.iter().any(|v| v.name == "tw.convert.jpg"));
        assert!(p.verbs.iter().any(|v| v.name == "tw.settings"));
        let mut names: Vec<_> = p.verbs.iter().map(|v| v.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), p.verbs.len(), "one id per verb");
    }

    #[test]
    fn items_that_do_not_apply_are_absent_from_the_built_popup() {
        let s = Settings::default();
        let one = menu_for(&["a.png"], &s);
        let two = menu_for(&["a.png", "b.png"], &s);
        let audio = menu_for(&["a.mp3"], &s);
        let has = |p: &Popup, name: &str| p.verbs.iter().any(|v| v.name == name);
        assert!(
            !has(&one, "tw.pdf") && has(&two, "tw.pdf"),
            "combine needs 2+"
        );
        assert!(has(&one, "tw.lockscreen") && !has(&two, "tw.lockscreen"));
        assert!(!has(&one, "tw.rename.artist") && has(&audio, "tw.rename.artist"));
        assert!(has(&two, "tw.settings"), "Workbench is always there");
    }

    #[test]
    fn every_verb_of_the_tree_is_reachable_from_the_popup() {
        let s = Settings::default();
        let mut reachable: Vec<&str> = Vec::new();
        for files in [
            vec!["a.png"],
            vec!["a.png", "b.png"],
            vec!["a.mp3"],
            vec!["a.mp3", "b.mp3"],
        ] {
            reachable.extend(menu_for(&files, &s).verbs.iter().map(|v| v.name));
        }
        for v in tree::all_verbs() {
            if v.name != tree::FOLDER_VERB.name {
                assert!(
                    reachable.contains(&v.name),
                    "{} is not in the popup",
                    v.name
                );
            }
        }
    }

    fn row_type(menu: HMENU, pos: u32, mask: MENU_ITEM_MASK) -> MENUITEMINFOW {
        let mut info = MENUITEMINFOW {
            cbSize: size_of::<MENUITEMINFOW>() as u32,
            fMask: mask,
            ..Default::default()
        };
        // SAFETY: reading one row of a live menu owned by the test.
        unsafe { GetMenuItemInfoW(menu, pos, true, &mut info) }.unwrap();
        info
    }

    /// A fresh 32-bpp DIB section, as the shell hands out thumbnails; the popup frees it.
    fn thumbnail(w: i32, h: i32) -> HBITMAP {
        use windows::Win32::Graphics::Gdi::{
            BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateDIBSection, DIB_RGB_COLORS,
        };
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
        // SAFETY: a fresh DIB section, handed to the caller.
        unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) }.unwrap()
    }

    #[test]
    fn a_thumbnail_turns_the_header_into_a_bitmap_row_and_leaves_the_others_alone() {
        let mut p = menu_for(&["a.png"], &Settings::default());
        assert_eq!(row_type(p.menu, 0, MIIM_FTYPE).fType, MFT_STRING);
        p.set_preview(thumbnail(64, 40), Placement::Thumbnail);
        assert!(
            p.bitmap.is_some(),
            "the composed row is kept for the menu's life"
        );
        assert_eq!(row_type(p.menu, 0, MIIM_FTYPE).fType, MFT_BITMAP);
        assert_eq!(rows(p.menu), 16, "same rows as before");
        // The other rows are plain text rows with no bitmap of their own, so none of them is
        // pushed right by a shared bitmap column.
        for i in 1..16 {
            let row = row_type(p.menu, i, MIIM_FTYPE | MIIM_BITMAP);
            assert!(row.hbmpItem.is_invalid(), "row {i} has no item bitmap");
            assert_eq!(
                row.fType.0 & MFT_OWNERDRAW.0,
                0,
                "row {i} is not owner-drawn"
            );
        }
    }

    #[test]
    fn the_box_is_reserved_before_the_thumbnail_and_the_swap_does_not_resize_the_row() {
        let mut p = menu_for(&["a.png"], &Settings::default());
        p.reserve_preview();
        assert_eq!(row_type(p.menu, 0, MIIM_FTYPE).fType, MFT_BITMAP);
        let reserved = p.bitmap.as_ref().expect("the empty row is kept").size();
        p.set_preview(thumbnail(64, 40), Placement::Thumbnail);
        assert_eq!(p.bitmap.as_ref().unwrap().size(), reserved);
        p.set_preview(thumbnail(40, 64), Placement::Thumbnail);
        assert_eq!(p.bitmap.as_ref().unwrap().size(), reserved, "also portrait");
        assert_eq!(row_type(p.menu, 0, MIIM_FTYPE).fType, MFT_BITMAP);
        assert_eq!(rows(p.menu), 16, "same rows as before");
    }

    #[test]
    fn an_icon_takes_the_thumbnails_place_without_resizing_the_row() {
        let mut p = menu_for(&["a.txt"], &Settings::default());
        p.reserve_preview();
        let reserved = p.bitmap.as_ref().expect("the empty row is kept").size();
        p.set_preview(thumbnail(128, 128), Placement::Icon);
        assert_eq!(p.bitmap.as_ref().unwrap().size(), reserved);
        assert_eq!(row_type(p.menu, 0, MIIM_FTYPE).fType, MFT_BITMAP);
        assert_eq!(rows(p.menu), 16, "no row added or removed");
        // SAFETY: reading ids of a live menu.
        assert_eq!(unsafe { GetMenuItemID(p.menu, 0) }, 0);
    }

    #[test]
    fn a_thumbnail_without_a_header_is_dropped() {
        let mut p = build(&[], None).unwrap();
        assert!(p.header.is_none());
        p.set_preview(
            // SAFETY: a 1x1 bitmap handed over to be freed.
            unsafe { windows::Win32::Graphics::Gdi::CreateBitmap(1, 1, 1, 32, None) },
            Placement::Thumbnail,
        );
        assert!(p.bitmap.is_none());
    }

    #[test]
    fn a_hidden_item_is_not_built() {
        let s = Settings {
            hidden_menu_items: vec!["rotate".into()],
            ..Settings::default()
        };
        let p = menu_for(&["a.png"], &s);
        assert!(!p.verbs.iter().any(|v| v.name.starts_with("tw.rotate")));
    }
}
