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
use std::time::Duration;

use tw_core::config::Settings;
use tw_core::menu::{self as tree, Selection, Shown, Verb};
use windows::Win32::Foundation::{HWND, SIZE};
use windows::Win32::Graphics::Gdi::{DeleteObject, HBITMAP, HGDIOBJ};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_THUMBNAILONLY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DestroyMenu, DestroyWindow, GetCursorPos, HMENU,
    MENUITEMINFOW, MF_POPUP, MF_SEPARATOR, MF_STRING, MIIM_BITMAP, PostMessageW,
    SetForegroundWindow, SetMenuItemInfoW, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_NULL, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{PCSTR, PCWSTR, w};

use crate::ops::Result;

/// How long the popup waits for the shell's thumbnail before it opens without one.
const PREVIEW_WAIT: Duration = Duration::from_millis(150);
const PREVIEW_EDGE: i32 = 128;

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
    bitmap: Option<HBITMAP>,
}

impl Drop for Popup {
    fn drop(&mut self) {
        // SAFETY: the menu (and its submenus) and the bitmap are owned by this popup.
        unsafe {
            let _ = DestroyMenu(self.menu);
            if let Some(b) = self.bitmap.take() {
                let _ = DeleteObject(HGDIOBJ(b.0));
            }
        }
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
/// row's bitmap greyed out, and the preview should keep its colours.
pub fn build(entries: &[Shown], header: Option<&str>) -> Result<Popup> {
    // SAFETY: a fresh popup menu, owned by the returned `Popup`.
    let menu = unsafe { CreatePopupMenu() }.map_err(winerr)?;
    let mut popup = Popup {
        menu,
        verbs: Vec::new(),
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
    /// Puts `bitmap` (premultiplied 32-bpp, owned by the popup from now on) on the header row.
    fn set_preview(&mut self, bitmap: HBITMAP) {
        let info = MENUITEMINFOW {
            cbSize: size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_BITMAP,
            hbmpItem: bitmap,
            ..Default::default()
        };
        // SAFETY: valid menu; row 0 is the header.
        if unsafe { SetMenuItemInfoW(self.menu, 0, true, &info) }.is_ok() {
            self.bitmap = Some(bitmap);
        } else {
            // SAFETY: not attached, so still ours to free.
            let _ = unsafe { DeleteObject(HGDIOBJ(bitmap.0)) };
        }
    }
}

/// Asks the shell's thumbnail cache for `file` on a worker thread; the bitmap arrives as a raw
/// handle, or never. A late arrival is freed by the worker.
fn fetch_preview(file: &std::path::Path) -> mpsc::Receiver<isize> {
    let (tx, rx) = mpsc::channel();
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
                let bmp = SHCreateItemFromParsingName::<_, _, IShellItemImageFactory>(
                    PCWSTR(path.as_ptr()),
                    None,
                )
                .and_then(|f| {
                    f.GetImage(
                        SIZE {
                            cx: PREVIEW_EDGE,
                            cy: PREVIEW_EDGE,
                        },
                        SIIGBF_THUMBNAILONLY,
                    )
                });
                if let Ok(b) = bmp
                    && tx.send(b.0 as isize).is_err()
                {
                    let _ = DeleteObject(HGDIOBJ(b.0));
                }
                if inited {
                    CoUninitialize();
                }
            }
        });
    rx
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

/// The hidden window that owns the popup: the menu needs a foreground owner to dismiss when
/// the user clicks away.
fn create_owner() -> Result<HWND> {
    // SAFETY: a never-shown popup of the predefined STATIC class.
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(WS_EX_TOOLWINDOW.0),
            w!("STATIC"),
            w!("Thumbwright"),
            WINDOW_STYLE(WS_POPUP.0),
            0,
            0,
            0,
            0,
            None,
            None,
            None,
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
/// they dismissed it (Esc, or a click anywhere else).
pub fn choose(
    settings: &Settings,
    files: &[PathBuf],
    at: (i32, i32),
) -> Result<Option<&'static Verb>> {
    crate::ui::dpi_aware();
    enable_dark_menus();
    let preview = (files.len() == 1 && settings.menu_preview).then(|| fetch_preview(&files[0]));
    let entries = tree::shown(settings, &Selection::of(files));
    let mut popup = build(&entries, Some(&tree::header_text(files)))?;
    if let Some(rx) = preview
        && let Ok(raw) = rx.recv_timeout(PREVIEW_WAIT)
    {
        popup.set_preview(HBITMAP(raw as *mut _));
    }
    let owner = create_owner()?;
    // SAFETY: `owner` and the menu are ours; the menu is modal until a choice or dismissal.
    let id = unsafe {
        let _ = SetForegroundWindow(owner);
        let id = TrackPopupMenuEx(
            popup.menu,
            (TPM_RETURNCMD | TPM_RIGHTBUTTON).0,
            at.0,
            at.1,
            owner,
            None,
        );
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
    use windows::Win32::UI::WindowsAndMessaging::{GetMenuItemCount, GetMenuItemID, GetSubMenu};

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
