//! The settings window: a fixed-size Win32 window with a page list on the left and one set
//! of child controls per page. Controls hold the edits until Apply/OK; the File types and
//! Menu lists keep their state in `App` (the filter box hides rows, so the list view alone
//! cannot be the source of truth).

use std::cell::RefCell;
use std::ffi::c_void;
use std::path::PathBuf;

use tw_core::config::{self, MENU_ITEMS, MenuPreview, Settings};
use tw_core::registration::{Layout, Scope};
use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    COLOR_BTNFACE, CreateFontIndirectW, DeleteObject, HBRUSH, HGDIOBJ,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    ICC_LISTVIEW_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx, LVIF_STATE,
    LVIS_STATEIMAGEMASK, LVN_ITEMCHANGED, LVS_EX_CHECKBOXES, LVS_EX_FULLROWSELECT, NMHDR,
    NMLISTVIEW,
};
use windows::Win32::UI::HiDpi::{GetDpiForSystem, SystemParametersInfoForDpi};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, BM_CLICK, CREATESTRUCTW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
    DestroyWindow, DispatchMessageW, EN_CHANGE, FindWindowW, GetDlgItem, GetMessageW, HMENU,
    IDC_ARROW, IsDialogMessageW, LB_ADDSTRING, LB_GETCURSEL, LB_SETCURSEL, LBN_SELCHANGE,
    LoadCursorW, MSG, NONCLIENTMETRICSW, PostQuitMessage, RegisterClassExW, SHOW_WINDOW_CMD,
    SPI_GETNONCLIENTMETRICS, SW_HIDE, SW_SHOW, SW_SHOWNORMAL, ShowWindow, TranslateMessage,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_NCDESTROY, WM_NOTIFY,
    WNDCLASSEXW, WS_CAPTION, WS_CHILD, WS_EX_CLIENTEDGE, WS_EX_CONTROLPARENT, WS_GROUP,
    WS_MINIMIZEBOX, WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};
use windows::core::{PCWSTR, w};

use crate::win::{
    confirm, error, get_text, info, is_checked, lv_add_column, lv_add_row, lv_clear, lv_count,
    lv_select, lv_selected, lv_set_checked, lv_set_extended, send, set_checked, set_font, set_text,
    wide,
};
use crate::{cache, model};

const CLASS: PCWSTR = w!("ThumbwrightSettingsWindow");
const TITLE: PCWSTR = w!("Thumbwright Settings");

// Raw control style bits (documented values; the `windows` constants are typed unevenly).
const BS_PUSHBUTTON: u32 = 0;
const BS_AUTOCHECKBOX: u32 = 3;
const BS_AUTORADIOBUTTON: u32 = 9;
const BS_GROUPBOX: u32 = 7;
const ES_AUTOHSCROLL: u32 = 0x80;
const ES_NUMBER: u32 = 0x2000;
const LBS_NOTIFY: u32 = 1;
const LVS_REPORT: u32 = 1;
const LVS_SINGLESEL: u32 = 4;
const LVS_SHOWSELALWAYS: u32 = 8;
const LVS_NOCOLUMNHEADER: u32 = 0x4000;
const SS_LEFT: u32 = 0;
const WS_VSCROLL: u32 = 0x0020_0000;

// Window and control ids.
const ID_OK: i32 = 1;
const ID_CANCEL: i32 = 2;
const ID_APPLY: i32 = 3;
const ID_NAV: i32 = 100;
const ID_STATUS: i32 = 101;
const ID_THUMBS: i32 = 200;
const ID_EMBEDDED: i32 = 201;
const ID_VERBOSE: i32 = 202;
const ID_JPEG: i32 = 203;
const ID_PNG: i32 = 204;
const ID_KEEP_META: i32 = 205;
const ID_KEEP_DATE: i32 = 206;
const ID_PDF_MARGIN: i32 = 207;
const ID_FILTER: i32 = 300;
const ID_TYPES: i32 = 301;
const ID_TYPES_COUNT: i32 = 302;
const ID_TYPES_ALL: i32 = 303;
const ID_TYPES_NONE: i32 = 304;
const ID_TYPES_DEFAULTS: i32 = 305;
const ID_MENU_ON: i32 = 400;
const ID_MENU_ALL: i32 = 401;
const ID_MENU_HERE: i32 = 402;
const ID_MENU_LIST: i32 = 403;
const ID_MENU_UP: i32 = 404;
const ID_MENU_DOWN: i32 = 405;
const ID_MENU_RESET: i32 = 406;
const ID_MENU_SHOWALL: i32 = 407;
const ID_MAX_FILE: i32 = 500;
const ID_MAX_THUMB: i32 = 501;
const ID_PREVIEW_OFF: i32 = 600;
const ID_PREVIEW_SUB: i32 = 601;
const ID_PREVIEW_MENU: i32 = 602;
const ID_CLOUD: i32 = 603;
const ID_VIDEO_PCT: i32 = 604;
const ID_MODE: i32 = 700;
const ID_REPAIR: i32 = 701;
const ID_REBUILD: i32 = 702;
const ID_TURN_ON: i32 = 703;
const ID_TURN_OFF: i32 = 704;
const ID_OPEN_LOG: i32 = 705;

const PAGES: [&str; 6] = [
    "General",
    "File types",
    "Menu",
    "Limits",
    "Preview",
    "Maintenance",
];
const PAGE_GENERAL: usize = 0;
const PAGE_TYPES: usize = 1;
const PAGE_MENU: usize = 2;
const PAGE_LIMITS: usize = 3;
const PAGE_PREVIEW: usize = 4;
const PAGE_MAINT: usize = 5;
/// Controls on no page: always visible.
const ALL_PAGES: usize = usize::MAX;

const CLIENT_W: i32 = 700;
const CLIENT_H: i32 = 480;
const X0: i32 = 170;
const CONTENT_W: i32 = 520;

struct TypeRow {
    ext: &'static str,
    desc: &'static str,
    enabled: bool,
}

struct MenuRow {
    id: &'static str,
    label: &'static str,
    visible: bool,
    hideable: bool,
}

#[derive(Default)]
struct Controls {
    nav: HWND,
    status: HWND,
    thumbs: HWND,
    embedded: HWND,
    verbose: HWND,
    jpeg: HWND,
    png: HWND,
    keep_meta: HWND,
    keep_date: HWND,
    pdf_margin: HWND,
    filter: HWND,
    types: HWND,
    types_count: HWND,
    menu_on: HWND,
    menu_all: HWND,
    menu_here: HWND,
    menu_list: HWND,
    max_file: HWND,
    max_thumb: HWND,
    preview: [HWND; 3],
    cloud: HWND,
    video_pct: HWND,
    mode: HWND,
}

/// Everything the window handlers share. Kept in a thread-local `RefCell`; handlers borrow
/// it only for short, non-pumping stretches and show message boxes after releasing it.
struct App {
    hwnd: HWND,
    dir: PathBuf,
    /// The settings as last loaded or saved; fields the window has no control for survive.
    base: Settings,
    font: isize,
    dpi: i32,
    quit_on_destroy: bool,
    /// True while this code fills controls, so their change notifications are ignored.
    loading: bool,
    pages: Vec<Vec<HWND>>,
    c: Controls,
    types: Vec<TypeRow>,
    /// Indices into `types` of the rows the filter currently shows.
    shown: Vec<usize>,
    menu: Vec<MenuRow>,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| {
        cell.try_borrow_mut()
            .ok()
            .and_then(|mut a| a.as_mut().map(f))
    })
}

struct Init {
    dir: PathBuf,
    settings: Settings,
    quit_on_destroy: bool,
}

fn user_layout() -> Layout {
    Layout::for_scope(Scope::User)
}

impl App {
    fn s(&self, v: i32) -> i32 {
        v * self.dpi / 96
    }

    #[allow(clippy::too_many_arguments)]
    fn add(
        &mut self,
        page: usize,
        class: PCWSTR,
        text: &str,
        style: u32,
        ex: u32,
        (x, y, w, h): (i32, i32, i32, i32),
        id: i32,
    ) -> HWND {
        // SAFETY: creates a child of our own window with NUL-terminated strings that live for
        // the call; the returned handle is owned by the parent.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(ex),
                class,
                PCWSTR(wide(text).as_ptr()),
                WINDOW_STYLE(style | WS_CHILD.0 | WS_VISIBLE.0),
                self.s(x),
                self.s(y),
                self.s(w),
                self.s(h),
                Some(self.hwnd),
                Some(HMENU(id as usize as *mut c_void)),
                None,
                None,
            )
        }
        .unwrap_or_default();
        set_font(hwnd, self.font);
        if page != ALL_PAGES {
            self.pages[page].push(hwnd);
        }
        hwnd
    }

    fn label(&mut self, page: usize, text: &str, rect: (i32, i32, i32, i32), id: i32) -> HWND {
        self.add(page, w!("STATIC"), text, SS_LEFT, 0, rect, id)
    }

    fn check(&mut self, page: usize, text: &str, rect: (i32, i32, i32, i32), id: i32) -> HWND {
        self.add(
            page,
            w!("BUTTON"),
            text,
            BS_AUTOCHECKBOX | WS_TABSTOP.0,
            0,
            rect,
            id,
        )
    }

    fn button(&mut self, page: usize, text: &str, rect: (i32, i32, i32, i32), id: i32) -> HWND {
        self.add(
            page,
            w!("BUTTON"),
            text,
            BS_PUSHBUTTON | WS_TABSTOP.0,
            0,
            rect,
            id,
        )
    }

    fn number_edit(&mut self, page: usize, rect: (i32, i32, i32, i32), id: i32) -> HWND {
        self.add(
            page,
            w!("EDIT"),
            "",
            ES_AUTOHSCROLL | ES_NUMBER | WS_TABSTOP.0,
            WS_EX_CLIENTEDGE.0,
            rect,
            id,
        )
    }

    fn list(&mut self, page: usize, rect: (i32, i32, i32, i32), id: i32, header: bool) -> HWND {
        let mut style = LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS | WS_TABSTOP.0 | WS_VSCROLL;
        if !header {
            style |= LVS_NOCOLUMNHEADER;
        }
        let lv = self.add(
            page,
            w!("SysListView32"),
            "",
            style,
            WS_EX_CLIENTEDGE.0,
            rect,
            id,
        );
        lv_set_extended(lv, LVS_EX_CHECKBOXES | LVS_EX_FULLROWSELECT);
        lv
    }

    fn build(&mut self, settings: &Settings) {
        self.pages = vec![Vec::new(); PAGES.len()];
        let nav = self.add(
            ALL_PAGES,
            w!("LISTBOX"),
            "",
            LBS_NOTIFY | WS_TABSTOP.0,
            WS_EX_CLIENTEDGE.0,
            (10, 10, 150, 420),
            ID_NAV,
        );
        for page in PAGES {
            send(nav, LB_ADDSTRING, 0, wide(page).as_ptr() as isize);
        }
        self.c.nav = nav;
        self.c.status = self.label(ALL_PAGES, "", (10, 448, 400, 20), ID_STATUS);
        self.button(ALL_PAGES, "OK", (430, 442, 80, 28), ID_OK);
        self.button(ALL_PAGES, "Cancel", (518, 442, 80, 28), ID_CANCEL);
        self.button(ALL_PAGES, "Apply", (606, 442, 80, 28), ID_APPLY);

        self.build_general();
        self.build_types();
        self.build_menu();
        self.build_limits();
        self.build_preview();
        self.build_maintenance();
        self.load(settings);
        send(nav, LB_SETCURSEL, PAGE_GENERAL, 0);
        self.show_page(PAGE_GENERAL);
    }

    fn build_general(&mut self) {
        let p = PAGE_GENERAL;
        self.add(
            p,
            w!("BUTTON"),
            "Thumbnails",
            BS_GROUPBOX,
            0,
            (X0, 8, CONTENT_W, 104),
            0,
        );
        self.c.thumbs = self.check(
            p,
            "Enable Explorer thumbnails",
            (X0 + 14, 30, 480, 20),
            ID_THUMBS,
        );
        self.c.embedded = self.check(
            p,
            "Prefer a file's embedded (EXIF) thumbnail when it has one",
            (X0 + 14, 56, 480, 20),
            ID_EMBEDDED,
        );
        self.c.verbose = self.check(p, "Verbose logging", (X0 + 14, 82, 480, 20), ID_VERBOSE);

        self.add(
            p,
            w!("BUTTON"),
            "Conversion defaults",
            BS_GROUPBOX,
            0,
            (X0, 124, CONTENT_W, 174),
            0,
        );
        self.label(p, "JPEG quality (1-100):", (X0 + 14, 150, 190, 18), 0);
        self.c.jpeg = self.number_edit(p, (X0 + 210, 146, 60, 22), ID_JPEG);
        self.label(
            p,
            "PNG compression level (0-9):",
            (X0 + 14, 180, 190, 18),
            0,
        );
        self.c.png = self.number_edit(p, (X0 + 210, 176, 60, 22), ID_PNG);
        self.c.keep_meta = self.check(
            p,
            "Keep EXIF and other metadata when converting",
            (X0 + 14, 208, 480, 20),
            ID_KEEP_META,
        );
        self.c.keep_date = self.check(
            p,
            "Keep the original date on saved files",
            (X0 + 14, 234, 480, 20),
            ID_KEEP_DATE,
        );
        self.c.pdf_margin = self.check(
            p,
            "Add a margin around pages when combining images into a PDF",
            (X0 + 14, 260, 480, 20),
            ID_PDF_MARGIN,
        );
    }

    fn build_types(&mut self) {
        let p = PAGE_TYPES;
        self.label(p, "Filter:", (X0, 14, 44, 18), 0);
        self.c.filter = self.add(
            p,
            w!("EDIT"),
            "",
            ES_AUTOHSCROLL | WS_TABSTOP.0,
            WS_EX_CLIENTEDGE.0,
            (X0 + 48, 10, 200, 22),
            ID_FILTER,
        );
        self.c.types_count = self.label(p, "", (X0 + 270, 14, 250, 18), ID_TYPES_COUNT);
        let lv = self.list(p, (X0, 40, CONTENT_W, 330), ID_TYPES, true);
        lv_add_column(lv, 0, "Extension", self.s(110));
        lv_add_column(lv, 1, "Description", self.s(370));
        self.c.types = lv;
        self.button(p, "Select all", (X0, 380, 100, 26), ID_TYPES_ALL);
        self.button(p, "Clear all", (X0 + 108, 380, 100, 26), ID_TYPES_NONE);
        self.button(p, "Defaults", (X0 + 216, 380, 100, 26), ID_TYPES_DEFAULTS);
        self.label(
            p,
            "Checked types get Thumbwright's thumbnail handler. Select all and Clear all apply \
             to the rows the filter shows.",
            (X0, 412, CONTENT_W, 32),
            0,
        );
    }

    fn build_menu(&mut self) {
        let p = PAGE_MENU;
        self.c.menu_on = self.check(
            p,
            "Enable the right-click menu",
            (X0, 12, 480, 20),
            ID_MENU_ON,
        );
        self.c.menu_all = self.check(
            p,
            "Show the menu on all file types",
            (X0, 38, 480, 20),
            ID_MENU_ALL,
        );
        self.c.menu_here = self.check(
            p,
            "Add \"Build thumbnails here\" to the folder menu",
            (X0, 64, 480, 20),
            ID_MENU_HERE,
        );
        self.label(
            p,
            "Menu items. Checked items are shown; the list order is the menu order.",
            (X0, 96, CONTENT_W, 18),
            0,
        );
        let lv = self.list(p, (X0, 118, 390, 290), ID_MENU_LIST, false);
        lv_add_column(lv, 0, "Menu item", self.s(350));
        self.c.menu_list = lv;
        self.button(p, "Move up", (X0 + 404, 118, 116, 26), ID_MENU_UP);
        self.button(p, "Move down", (X0 + 404, 150, 116, 26), ID_MENU_DOWN);
        self.button(p, "Reset order", (X0 + 404, 190, 116, 26), ID_MENU_RESET);
        self.button(p, "Show all", (X0 + 404, 222, 116, 26), ID_MENU_SHOWALL);
        self.label(
            p,
            "Settings is always in the menu. Items the menu does not have yet are kept in the \
             list so your choices survive updates.",
            (X0, 414, CONTENT_W, 30),
            0,
        );
    }

    fn build_limits(&mut self) {
        let p = PAGE_LIMITS;
        self.label(p, "Maximum file size (MB):", (X0, 20, 200, 18), 0);
        self.c.max_file = self.number_edit(p, (X0 + 210, 16, 80, 22), ID_MAX_FILE);
        self.label(
            p,
            "Larger files get no thumbnail (1 - 4096).",
            (X0 + 300, 20, 220, 18),
            0,
        );
        self.label(p, "Maximum thumbnail size (px):", (X0, 56, 200, 18), 0);
        self.c.max_thumb = self.number_edit(p, (X0 + 210, 52, 80, 22), ID_MAX_THUMB);
        self.label(
            p,
            "Longest edge of a produced thumbnail (16 - 1024).",
            (X0 + 300, 56, 220, 32),
            0,
        );
    }

    fn build_preview(&mut self) {
        let p = PAGE_PREVIEW;
        self.add(
            p,
            w!("BUTTON"),
            "Preview in the right-click menu",
            BS_GROUPBOX,
            0,
            (X0, 8, CONTENT_W, 118),
            0,
        );
        let radio = BS_AUTORADIOBUTTON | WS_TABSTOP.0;
        self.c.preview[0] = self.add(
            p,
            w!("BUTTON"),
            "Off",
            radio | WS_GROUP.0,
            0,
            (X0 + 14, 30, 480, 20),
            ID_PREVIEW_OFF,
        );
        self.c.preview[1] = self.add(
            p,
            w!("BUTTON"),
            "In the submenu",
            radio,
            0,
            (X0 + 14, 56, 480, 20),
            ID_PREVIEW_SUB,
        );
        self.c.preview[2] = self.add(
            p,
            w!("BUTTON"),
            "In the right-click menu itself",
            radio,
            0,
            (X0 + 14, 82, 480, 20),
            ID_PREVIEW_MENU,
        );
        self.add(
            p,
            w!("BUTTON"),
            "Thumbnail content",
            BS_GROUPBOX,
            0,
            (X0, 138, CONTENT_W, 90),
            0,
        );
        self.c.cloud = self.check(
            p,
            "Thumbnails in cloud folders (never downloads online-only files)",
            (X0 + 14, 160, 490, 20),
            ID_CLOUD,
        );
        self.label(
            p,
            "Video thumbnail frame (% into the video):",
            (X0 + 14, 194, 270, 18),
            0,
        );
        self.c.video_pct = self.number_edit(p, (X0 + 290, 190, 60, 22), ID_VIDEO_PCT);
        self.label(p, "0 - 100", (X0 + 360, 194, 100, 18), 0);
    }

    fn build_maintenance(&mut self) {
        let p = PAGE_MAINT;
        self.c.mode = self.label(p, "", (X0, 12, CONTENT_W, 48), ID_MODE);
        let rows = [
            (
                ID_REPAIR,
                "Repair associations",
                "Re-registers every enabled file type, then clears the thumbnail cache.",
            ),
            (
                ID_REBUILD,
                "Rebuild thumbnail cache",
                "Deletes Windows' cached thumbnails; Explorer restarts.",
            ),
            (
                ID_TURN_ON,
                "Turn on thumbnails",
                "Registers Thumbwright with Explorer for this user (portable copies).",
            ),
            (
                ID_TURN_OFF,
                "Turn off thumbnails",
                "Removes every registry key Thumbwright created for this user.",
            ),
            (ID_OPEN_LOG, "Open log file", "Shows the diagnostics log."),
        ];
        for (i, (id, label, hint)) in rows.into_iter().enumerate() {
            let y = 70 + i as i32 * 44;
            self.button(p, label, (X0, y, 190, 28), id);
            self.label(p, hint, (X0 + 202, y, 318, 36), 0);
        }
    }

    fn show_page(&mut self, page: usize) {
        for (i, handles) in self.pages.iter().enumerate() {
            for &h in handles {
                // SAFETY: our own child windows.
                let _ = unsafe { ShowWindow(h, if i == page { SW_SHOW } else { SW_HIDE }) };
            }
        }
    }

    fn set_status(&self, text: &str) {
        set_text(self.c.status, text);
    }

    fn refresh_mode(&self) {
        let mode = if config::is_portable(&self.dir) {
            format!("Portable copy: settings are in {}.", config::PORTABLE_INI)
        } else {
            format!(
                "Installed copy: settings are in HKCU\\{}.",
                config::PRODUCT_KEY
            )
        };
        let reg = model::registration_summary(&user_layout());
        set_text(self.c.mode, &format!("{mode}\n{reg}"));
    }

    // ---- model <-> controls ----

    /// Puts `s` into every control and list.
    fn load(&mut self, s: &Settings) {
        self.loading = true;
        self.base = s.clone();
        set_checked(self.c.thumbs, s.thumbnails_enabled);
        set_checked(self.c.embedded, s.use_embedded_thumbs);
        set_checked(self.c.cloud, s.cloud_thumbnails);
        set_text(self.c.video_pct, &s.video_frame_percent.to_string());
        set_checked(self.c.verbose, s.verbose_log);
        set_text(self.c.jpeg, &s.jpeg_quality.to_string());
        set_text(self.c.png, &s.png_compression.to_string());
        set_checked(self.c.keep_meta, s.keep_metadata);
        set_checked(self.c.keep_date, s.preserve_file_date);
        set_checked(self.c.pdf_margin, s.pdf_margin);
        set_checked(self.c.menu_on, s.menu_enabled);
        set_checked(self.c.menu_all, s.menu_all_file_types);
        set_checked(self.c.menu_here, s.menu_build_here);
        set_text(self.c.max_file, &s.max_file_mb.to_string());
        set_text(self.c.max_thumb, &s.max_thumb_px.to_string());
        let which = match s.menu_preview {
            MenuPreview::Off => 0,
            MenuPreview::InSubmenu => 1,
            MenuPreview::InContextMenu => 2,
        };
        for (i, &h) in self.c.preview.iter().enumerate() {
            set_checked(h, i == which);
        }
        self.types = model::registrable()
            .into_iter()
            .map(|f| TypeRow {
                ext: f.ext,
                desc: f.description,
                enabled: s.type_enabled(f.ext),
            })
            .collect();
        self.menu = config::ordered_menu_ids(&s.menu_order)
            .into_iter()
            .filter_map(|id| MENU_ITEMS.iter().find(|i| i.id == id))
            .map(|i| MenuRow {
                id: i.id,
                label: i.label,
                visible: !i.hideable || s.menu_item_visible(i.id),
                hideable: i.hideable,
            })
            .collect();
        self.loading = false;
        self.fill_types();
        self.fill_menu(None);
        self.refresh_mode();
    }

    /// Rebuilds the File types list from `types` through the filter box.
    fn fill_types(&mut self) {
        let filter = get_text(self.c.filter).trim().to_ascii_lowercase();
        self.loading = true;
        let lv = self.c.types;
        lv_clear(lv);
        self.shown.clear();
        for (i, t) in self.types.iter().enumerate() {
            if filter.is_empty()
                || t.ext.contains(filter.as_str())
                || t.desc.to_ascii_lowercase().contains(&filter)
            {
                lv_add_row(lv, &[&format!(".{}", t.ext), t.desc]);
                lv_set_checked(lv, self.shown.len(), t.enabled);
                self.shown.push(i);
            }
        }
        self.loading = false;
        self.update_types_count();
    }

    fn update_types_count(&self) {
        let on = self.types.iter().filter(|t| t.enabled).count();
        set_text(
            self.c.types_count,
            &format!("{on} of {} file types enabled", self.types.len()),
        );
    }

    fn fill_menu(&mut self, select: Option<usize>) {
        self.loading = true;
        let lv = self.c.menu_list;
        lv_clear(lv);
        for (i, m) in self.menu.iter().enumerate() {
            lv_add_row(lv, &[m.label]);
            lv_set_checked(lv, i, m.visible);
        }
        self.loading = false;
        if let Some(i) = select {
            lv_select(lv, i);
        }
    }

    /// The settings the controls describe, or the page and message of the first bad value.
    fn collect(&self) -> Result<Settings, (usize, String)> {
        let number =
            |h: HWND, page, name: &str, lo: u64, hi: u64| -> Result<u64, (usize, String)> {
                get_text(h)
                    .trim()
                    .parse::<u64>()
                    .ok()
                    .filter(|n| (lo..=hi).contains(n))
                    .ok_or((
                        page,
                        format!("{name} must be a whole number from {lo} to {hi}."),
                    ))
            };
        let mut s = self.base.clone();
        s.thumbnails_enabled = is_checked(self.c.thumbs);
        s.use_embedded_thumbs = is_checked(self.c.embedded);
        s.cloud_thumbnails = is_checked(self.c.cloud);
        s.video_frame_percent = number(
            self.c.video_pct,
            PAGE_PREVIEW,
            "Video thumbnail frame",
            0,
            100,
        )? as u8;
        s.verbose_log = is_checked(self.c.verbose);
        s.jpeg_quality = number(self.c.jpeg, PAGE_GENERAL, "JPEG quality", 1, 100)? as u8;
        s.png_compression = number(self.c.png, PAGE_GENERAL, "PNG compression level", 0, 9)? as u8;
        s.keep_metadata = is_checked(self.c.keep_meta);
        s.preserve_file_date = is_checked(self.c.keep_date);
        s.pdf_margin = is_checked(self.c.pdf_margin);
        s.menu_enabled = is_checked(self.c.menu_on);
        s.menu_all_file_types = is_checked(self.c.menu_all);
        s.menu_build_here = is_checked(self.c.menu_here);
        s.max_file_mb = number(
            self.c.max_file,
            PAGE_LIMITS,
            "Maximum file size",
            1,
            tw_core::limits::DEFAULT_MAX_FILE_MB,
        )?;
        s.max_thumb_px = number(
            self.c.max_thumb,
            PAGE_LIMITS,
            "Maximum thumbnail size",
            16,
            u64::from(tw_core::limits::DEFAULT_MAX_THUMB_EDGE),
        )? as u32;
        s.menu_preview = if is_checked(self.c.preview[0]) {
            MenuPreview::Off
        } else if is_checked(self.c.preview[2]) {
            MenuPreview::InContextMenu
        } else {
            MenuPreview::InSubmenu
        };
        // Entries for extensions or menu items this window does not list are not ours to drop.
        let listed_type = |e: &str| self.types.iter().any(|t| t.ext == e);
        s.disabled_types = self
            .types
            .iter()
            .filter(|t| !t.enabled)
            .map(|t| t.ext.to_string())
            .chain(
                self.base
                    .disabled_types
                    .iter()
                    .filter(|e| !listed_type(e))
                    .cloned(),
            )
            .collect();
        let listed_item = |id: &str| self.menu.iter().any(|m| m.id == id);
        s.hidden_menu_items = self
            .menu
            .iter()
            .filter(|m| m.hideable && !m.visible)
            .map(|m| m.id.to_string())
            .chain(
                self.base
                    .hidden_menu_items
                    .iter()
                    .filter(|id| !listed_item(id))
                    .cloned(),
            )
            .collect();
        let order: Vec<&str> = self.menu.iter().map(|m| m.id).collect();
        let default: Vec<&str> = MENU_ITEMS.iter().map(|i| i.id).collect();
        s.menu_order = if order == default {
            Vec::new()
        } else {
            order.iter().map(|id| id.to_string()).collect()
        };
        Ok(s)
    }

    // ---- handlers that only touch window state ----

    fn set_shown_types(&mut self, enabled: Option<bool>) {
        match enabled {
            Some(on) => {
                for &i in &self.shown {
                    self.types[i].enabled = on;
                }
            }
            None => self.types.iter_mut().for_each(|t| t.enabled = true),
        }
        self.fill_types();
    }

    fn move_menu_item(&mut self, delta: isize) {
        let Some(i) = lv_selected(self.c.menu_list) else {
            return;
        };
        let Some(j) = i.checked_add_signed(delta).filter(|&j| j < self.menu.len()) else {
            return;
        };
        self.menu.swap(i, j);
        self.fill_menu(Some(j));
    }

    fn reset_menu_order(&mut self) {
        let mut selected = None;
        let current = lv_selected(self.c.menu_list).and_then(|i| self.menu.get(i).map(|m| m.id));
        let visible: Vec<(&str, bool)> = self.menu.iter().map(|m| (m.id, m.visible)).collect();
        self.menu = MENU_ITEMS
            .iter()
            .map(|i| MenuRow {
                id: i.id,
                label: i.label,
                visible: visible
                    .iter()
                    .find(|(id, _)| *id == i.id)
                    .is_none_or(|&(_, v)| v),
                hideable: i.hideable,
            })
            .collect();
        if let Some(id) = current {
            selected = self.menu.iter().position(|m| m.id == id);
        }
        self.fill_menu(selected);
    }

    fn show_all_menu_items(&mut self) {
        self.menu.iter_mut().for_each(|m| m.visible = true);
        let selected = lv_selected(self.c.menu_list);
        self.fill_menu(selected);
    }

    fn list_state_changed(&mut self, from: HWND, index: usize, checked: bool) {
        if self.loading {
            return;
        }
        if from == self.c.types {
            if let Some(&i) = self.shown.get(index) {
                self.types[i].enabled = checked;
                self.update_types_count();
            }
        } else if from == self.c.menu_list
            && let Some(row) = self.menu.get_mut(index)
        {
            if row.hideable {
                row.visible = checked;
            } else if !checked {
                lv_set_checked(from, index, true); // always present
            }
        }
    }
}

// ---- window procedure and actions that show dialogs ----

fn wm_command(hwnd: HWND, id: i32, code: u32) {
    match id {
        ID_OK => {
            if apply(hwnd).is_some() {
                // SAFETY: our own top-level window.
                let _ = unsafe { DestroyWindow(hwnd) };
            }
        }
        ID_CANCEL => {
            // SAFETY: our own top-level window.
            let _ = unsafe { DestroyWindow(hwnd) };
        }
        ID_APPLY => {
            apply(hwnd);
        }
        ID_NAV if code == LBN_SELCHANGE => {
            with_app(|a| {
                let sel = send(a.c.nav, LB_GETCURSEL, 0, 0);
                if let Ok(page) = usize::try_from(sel) {
                    a.show_page(page);
                }
            });
        }
        ID_FILTER if code == EN_CHANGE => {
            with_app(|a| {
                if !a.loading {
                    a.fill_types();
                }
            });
        }
        ID_TYPES_ALL => drop(with_app(|a| a.set_shown_types(Some(true)))),
        ID_TYPES_NONE => drop(with_app(|a| a.set_shown_types(Some(false)))),
        ID_TYPES_DEFAULTS => drop(with_app(|a| a.set_shown_types(None))),
        ID_MENU_UP => drop(with_app(|a| a.move_menu_item(-1))),
        ID_MENU_DOWN => drop(with_app(|a| a.move_menu_item(1))),
        ID_MENU_RESET => drop(with_app(App::reset_menu_order)),
        ID_MENU_SHOWALL => drop(with_app(App::show_all_menu_items)),
        ID_REPAIR => repair(hwnd),
        ID_REBUILD => rebuild_cache(hwnd),
        ID_TURN_ON => turn_on(hwnd),
        ID_TURN_OFF => turn_off(hwnd),
        ID_OPEN_LOG => open_log(hwnd),
        _ => {}
    }
}

/// Validates the controls and saves; on success the window's baseline becomes what was
/// saved. Shows a message box and returns `None` on failure.
fn apply(hwnd: HWND) -> Option<Settings> {
    let (dir, collected) = with_app(|a| (a.dir.clone(), a.collect()))?;
    let settings = match collected {
        Ok(s) => s,
        Err((page, msg)) => {
            with_app(|a| {
                send(a.c.nav, LB_SETCURSEL, page, 0);
                a.show_page(page);
            });
            error(hwnd, &msg);
            return None;
        }
    };
    match model::apply(&dir, &user_layout(), &settings) {
        Ok(applied) => {
            with_app(|a| {
                a.base = settings.clone();
                a.set_status("Settings saved.");
                a.refresh_mode();
            });
            if applied.menu_needs_repair {
                info(
                    hwnd,
                    "Settings saved. The right-click menu was off when Thumbwright was registered; \
                     use Repair associations on the Maintenance page to add it.",
                );
            }
            Some(settings)
        }
        Err(e) => {
            error(hwnd, &e);
            None
        }
    }
}

fn after_registration_change(hwnd: HWND, text: &str) {
    with_app(|a| a.refresh_mode());
    info(hwnd, text);
}

fn turn_on(hwnd: HWND) {
    let Some(settings) = apply(hwnd) else { return };
    let Some(dir) = with_app(|a| a.dir.clone()) else {
        return;
    };
    match model::register_user(&dir, &user_layout(), &settings) {
        Ok(n) => after_registration_change(
            hwnd,
            &format!("Explorer thumbnails turned on for this user ({n} file types)."),
        ),
        Err(e) => error(hwnd, &e),
    }
}

fn turn_off(hwnd: HWND) {
    match model::unregister_user(&user_layout()) {
        Ok(()) => after_registration_change(hwnd, "Explorer thumbnails turned off for this user."),
        Err(e) => error(hwnd, &e),
    }
}

fn repair(hwnd: HWND) {
    if !confirm(
        hwnd,
        "Repair re-registers every enabled file type and then clears Windows' thumbnail cache.\n\n\
         Clearing the cache closes all File Explorer windows and restarts the taskbar. \
         Save your work in Explorer first. Continue?",
    ) {
        return;
    }
    let Some(settings) = apply(hwnd) else { return };
    let Some(dir) = with_app(|a| a.dir.clone()) else {
        return;
    };
    let n = match model::register_user(&dir, &user_layout(), &settings) {
        Ok(n) => n,
        Err(e) => return error(hwnd, &e),
    };
    with_app(|a| a.refresh_mode());
    match cache::rebuild() {
        Ok(msg) => info(hwnd, &format!("Registered {n} file types.\n{msg}")),
        Err(e) => error(hwnd, &format!("Registered {n} file types.\n{e}")),
    }
}

fn rebuild_cache(hwnd: HWND) {
    if !confirm(
        hwnd,
        "Rebuilding the thumbnail cache deletes Windows' cached thumbnails (thumbcache_*.db). \
         Explorer must be closed to do that, so all File Explorer windows close and the taskbar \
         restarts.\n\nSave your work in Explorer first. Continue?",
    ) {
        return;
    }
    match cache::rebuild() {
        Ok(msg) => info(hwnd, &msg),
        Err(e) => error(hwnd, &e),
    }
}

fn open_log(hwnd: HWND) {
    let Some(log) = std::env::var_os("LOCALAPPDATA")
        .map(|d| std::path::Path::new(&d).join("Thumbwright.log"))
        .filter(|p| p.is_file())
    else {
        return info(hwnd, "There is no log file yet.");
    };
    // SAFETY: documented ShellExecute call with NUL-terminated strings.
    unsafe {
        ShellExecuteW(
            Some(hwnd),
            w!("open"),
            PCWSTR(wide(&log.to_string_lossy()).as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

fn wm_notify(lparam: LPARAM) {
    // SAFETY: for WM_NOTIFY, lparam points at an NMHDR (an NMLISTVIEW for list views) that
    // lives for the duration of this synchronous notification.
    let (hdr, lv) = unsafe {
        let hdr = &*(lparam.0 as *const NMHDR);
        (hdr, &*(lparam.0 as *const NMLISTVIEW))
    };
    if hdr.code != LVN_ITEMCHANGED || lv.uChanged.0 & LVIF_STATE.0 == 0 {
        return;
    }
    let image = |state: u32| (state & LVIS_STATEIMAGEMASK.0) >> 12;
    let (old, new) = (image(lv.uOldState), image(lv.uNewState));
    // Only real check-state changes: item creation reports old = 0, selection changes keep
    // the image bits equal.
    if old != 0 && new != 0 && old != new && lv.iItem >= 0 {
        with_app(|a| a.list_state_changed(hdr.hwndFrom, lv.iItem as usize, new == 2));
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            // SAFETY: lparam is the CREATESTRUCTW of this window; `lpCreateParams` is the
            // address of the `Init` on `create`'s stack, alive for this whole synchronous call.
            let init = unsafe {
                let cs = &*(lparam.0 as *const CREATESTRUCTW);
                &*(cs.lpCreateParams as *const Init)
            };
            let dpi = unsafe { GetDpiForSystem() } as i32;
            let mut app = App {
                hwnd,
                dir: init.dir.clone(),
                base: init.settings.clone(),
                font: message_font(dpi as u32),
                dpi,
                quit_on_destroy: init.quit_on_destroy,
                loading: false,
                pages: Vec::new(),
                c: Controls::default(),
                types: Vec::new(),
                shown: Vec::new(),
                menu: Vec::new(),
            };
            app.build(&init.settings);
            APP.with(|cell| *cell.borrow_mut() = Some(app));
            LRESULT(0)
        }
        WM_COMMAND => {
            wm_command(
                hwnd,
                (wparam.0 & 0xFFFF) as i32,
                ((wparam.0 >> 16) & 0xFFFF) as u32,
            );
            LRESULT(0)
        }
        WM_NOTIFY => {
            wm_notify(lparam);
            LRESULT(0)
        }
        WM_DESTROY => {
            if with_app(|a| a.quit_on_destroy) == Some(true) {
                // SAFETY: posts WM_QUIT to this thread's queue.
                unsafe { PostQuitMessage(0) };
            }
            LRESULT(0)
        }
        WM_NCDESTROY => {
            let app = APP.with(|cell| cell.try_borrow_mut().ok().and_then(|mut a| a.take()));
            if let Some(app) = app {
                // SAFETY: the font was created by `message_font` for this window and is
                // no longer selected into any control once the window is gone.
                let _ = unsafe { DeleteObject(HGDIOBJ(app.font as *mut c_void)) };
            }
            // SAFETY: default handling for the final message.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        // SAFETY: default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// The system message font (the font dialogs use) at `dpi`.
fn message_font(dpi: u32) -> isize {
    let mut metrics = NONCLIENTMETRICSW {
        cbSize: size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    // SAFETY: `metrics` is a correctly sized, writable NONCLIENTMETRICSW.
    unsafe {
        let _ = SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            size_of::<NONCLIENTMETRICSW>() as u32,
            Some(std::ptr::from_mut(&mut metrics).cast()),
            0,
            dpi,
        );
        CreateFontIndirectW(&metrics.lfMessageFont).0 as isize
    }
}

fn init_controls() {
    let icc = INITCOMMONCONTROLSEX {
        dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_LISTVIEW_CLASSES,
    };
    // SAFETY: `icc` is a valid, sized INITCOMMONCONTROLSEX.
    let _ = unsafe { InitCommonControlsEx(&icc) };
}

fn register_class() -> Result<(), String> {
    // SAFETY: registers a window class whose procedure and strings are 'static.
    unsafe {
        let hinstance = GetModuleHandleW(None).map_err(|e| e.to_string())?;
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).map_err(|e| e.to_string())?,
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as usize as *mut c_void),
            lpszClassName: CLASS,
            ..Default::default()
        };
        if RegisterClassExW(&class) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            return Err("cannot register the window class".into());
        }
    }
    Ok(())
}

/// Creates the settings window for the binaries in `dir`. It is shown unless `visible` is
/// false (the self-test).
fn create(
    dir: PathBuf,
    settings: Settings,
    quit_on_destroy: bool,
    visible: bool,
) -> Result<HWND, String> {
    init_controls();
    register_class()?;
    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
    let ex = WS_EX_CONTROLPARENT;
    // SAFETY: `init` outlives the synchronous CreateWindowExW call that hands its address to
    // WM_CREATE; everything else is plain Win32 calls with valid arguments.
    unsafe {
        let dpi = GetDpiForSystem() as i32;
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: CLIENT_W * dpi / 96,
            bottom: CLIENT_H * dpi / 96,
        };
        let _ = AdjustWindowRectEx(&mut rect, style, false, ex);
        let init = Init {
            dir,
            settings,
            quit_on_destroy,
        };
        let hwnd = CreateWindowExW(
            ex,
            CLASS,
            TITLE,
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rect.right - rect.left,
            rect.bottom - rect.top,
            None,
            None,
            None,
            Some(std::ptr::from_ref(&init).cast::<c_void>()),
        );
        match hwnd {
            Ok(h) => {
                if visible {
                    let _ = ShowWindow(h, SHOW_WINDOW_CMD(SW_SHOWNORMAL.0));
                }
                Ok(h)
            }
            Err(e) => Err(format!("cannot create the window: {e}")),
        }
    }
}

/// Runs the settings window until it closes.
pub fn run(dir: PathBuf, settings: Settings) -> Result<(), String> {
    let hwnd = create(dir, settings, true, true)?;
    let mut msg = MSG::default();
    // SAFETY: standard message loop on the thread that created the window.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
    Ok(())
}

/// Creates the (hidden) window with non-default settings, drives it the way a user would
/// (check boxes, buttons, the filter box), checks that it reads back exactly what was set
/// and changed, finds it by class name, and destroys it.
pub fn self_test(dir: PathBuf) -> Result<String, String> {
    let all = model::registrable();
    let settings = Settings {
        thumbnails_enabled: false,
        use_embedded_thumbs: false,
        cloud_thumbnails: true,
        max_file_mb: 250,
        max_thumb_px: 512,
        jpeg_quality: 72,
        png_compression: 4,
        keep_metadata: false,
        preserve_file_date: true,
        pdf_margin: true,
        menu_enabled: false,
        menu_all_file_types: true,
        menu_build_here: false,
        menu_preview: MenuPreview::InContextMenu,
        // A saved order lists every item (the window writes the full order).
        menu_order: config::ordered_menu_ids(&["wallpaper".into(), "resize".into()])
            .into_iter()
            .map(String::from)
            .collect(),
        disabled_types: vec!["png".into(), "svg".into(), "not-listed".into()],
        hidden_menu_items: vec!["email".into(), "future_item".into()],
        verbose_log: true,
        video_frame_percent: 60,
    };
    let hwnd = create(dir, settings.clone(), false, false)?;
    let check = |ok: bool, what: &str| if ok { Ok(()) } else { Err(what.to_string()) };

    // SAFETY: plain lookups and a destroy of the window created above.
    let found = unsafe { FindWindowW(CLASS, TITLE) };
    check(
        found == Ok(hwnd),
        "FindWindowW did not return the settings window",
    )?;

    let (types, menu, filter) = with_app(|a| (a.c.types, a.c.menu_list, a.c.filter))
        .ok_or("no window state after creation")?;
    check(lv_count(types) == all.len(), "File types list size")?;
    check(lv_count(menu) == MENU_ITEMS.len(), "Menu list size")?;
    let read = with_app(|a| a.collect()).ok_or("window state busy")?;
    let read = read.map_err(|(_, e)| e)?;
    // List order is not part of the contract for the two sets.
    let canon = |mut s: Settings| {
        s.disabled_types.sort();
        s.hidden_menu_items.sort();
        s
    };
    check(
        canon(read.clone()) == canon(settings.clone()),
        &format!("controls do not read back their settings:\n{read:?}\n{settings:?}"),
    )?;

    // A user ticking .png back on, and unticking the first menu row (wallpaper).
    let png = all
        .iter()
        .position(|f| f.ext == "png")
        .ok_or("png not routable")?;
    lv_set_checked(types, png, true);
    lv_set_checked(menu, 0, false);
    let after = with_app(|a| a.collect())
        .ok_or("window state busy")?
        .map_err(|(_, e)| e)?;
    check(
        after.type_enabled("png") && !after.type_enabled("svg"),
        "type toggle",
    )?;
    check(
        after.hidden_menu_items.contains(&"wallpaper".to_string()),
        "menu visibility toggle",
    )?;
    check(
        after.disabled_types.contains(&"not-listed".to_string())
            && after.hidden_menu_items.contains(&"future_item".to_string()),
        "unlisted entries must survive",
    )?;

    // The filter shows only matching rows, and Clear all then applies to those only.
    set_text(filter, "svg");
    let shown = lv_count(types);
    check(
        shown > 0 && shown < all.len(),
        "filter did not narrow the list",
    )?;
    let clear = unsafe { GetDlgItem(Some(hwnd), ID_TYPES_NONE) }.map_err(|e| e.to_string())?;
    with_app(|a| a.show_page(PAGE_TYPES));
    send(clear, BM_CLICK, 0, 0);
    set_text(filter, "");
    check(
        lv_count(types) == all.len(),
        "clearing the filter restores the list",
    )?;
    let cleared = with_app(|a| a.collect())
        .ok_or("window state busy")?
        .map_err(|(_, e)| e)?;
    check(
        cleared.type_enabled("png") && !cleared.type_enabled("svg"),
        "Clear all must only affect the filtered rows",
    )?;

    // Bad numbers are refused with the page that holds them.
    with_app(|a| set_text(a.c.jpeg, "0"));
    let bad = with_app(|a| a.collect()).ok_or("window state busy")?;
    check(
        matches!(bad, Err((PAGE_GENERAL, _))),
        "JPEG quality 0 must be rejected",
    )?;

    with_app(|a| set_text(a.c.video_pct, "101"));
    let bad = with_app(|a| a.collect()).ok_or("window state busy")?;
    check(
        matches!(bad, Err((PAGE_PREVIEW, _))),
        "video frame 101% must be rejected",
    )?;

    // SAFETY: destroys the window created above, on its own thread.
    unsafe { DestroyWindow(hwnd) }.map_err(|e| e.to_string())?;
    // SAFETY: plain lookup.
    check(
        unsafe { FindWindowW(CLASS, TITLE) }.is_err(),
        "window still exists after destroy",
    )?;
    check(
        with_app(|_| ()).is_none(),
        "window state outlived the window",
    )?;

    Ok(format!(
        "window created, driven and destroyed: {} file types, {} menu items",
        all.len(),
        MENU_ITEMS.len()
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn window_is_created_driven_and_destroyed() {
        let dir = std::env::temp_dir();
        let summary = super::self_test(dir).unwrap();
        assert!(summary.contains("destroyed"), "{summary}");
    }
}
