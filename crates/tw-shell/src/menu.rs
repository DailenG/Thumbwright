//! Classic context menu (`IShellExtInit` + `IContextMenu{,2,3}`, REQ-COM-3).
//!
//! Runs inside Explorer, so it never decodes images (REQ-ISO-4): every verb launches
//! `twctl.exe` with the selection, and the owner-drawn preview item asks the shell's own
//! thumbnail cache (which extracts through the isolated surrogate) on a background thread.

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    E_FAIL, E_INVALIDARG, E_NOTIMPL, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, AlphaBlend, BITMAP, BLENDFUNCTION, COLOR_MENU, CreateCompatibleDC,
    DeleteDC, DeleteObject, FillRect, GetObjectW, GetSysColorBrush, HBITMAP, HGDIOBJ, SelectObject,
};
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize, DVASPECT_CONTENT, FORMATETC,
    IDataObject, TYMED_HGLOBAL,
};
use windows::Win32::System::Ole::{CF_HDROP, ReleaseStgMedium};
use windows::Win32::System::Registry::HKEY;
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, MEASUREITEMSTRUCT};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    CMF_DEFAULTONLY, CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX, DragQueryFileW, GCS_HELPTEXTA,
    GCS_HELPTEXTW, GCS_VALIDATEA, GCS_VALIDATEW, GCS_VERBA, GCS_VERBW, GPFIDL_DEFAULT, HDROP,
    IContextMenu, IContextMenu_Impl, IContextMenu2, IContextMenu2_Impl, IContextMenu3,
    IContextMenu3_Impl, IShellExtInit, IShellExtInit_Impl, IShellItemImageFactory,
    SHCreateItemFromParsingName, SHGetPathFromIDListEx, SIIGBF_BIGGERSIZEOK, SIIGBF_THUMBNAILONLY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, HMENU, InsertMenuItemW, MENUITEMINFOW, MFS_DISABLED, MFT_OWNERDRAW,
    MFT_SEPARATOR, MFT_STRING, MIIM_FTYPE, MIIM_ID, MIIM_STATE, MIIM_STRING, MIIM_SUBMENU,
    WM_DRAWITEM, WM_INITMENUPOPUP, WM_MEASUREITEM,
};
use windows::core::{HRESULT, PCWSTR, PSTR, PWSTR, Ref, Result, implement};

use tw_core::config::{MenuPreview, Settings};
use tw_core::ids;
use tw_core::log;

use crate::ModuleRef;

/// `CMINVOKECOMMANDINFOEX` carries valid wide fields when this flag is set.
const CMIC_MASK_UNICODE: u32 = 0x0000_4000;
const PREVIEW_EDGE: i32 = 128;
const PREVIEW_PAD: i32 = 6;
/// How long drawing the preview may wait for the shell's thumbnail.
const PREVIEW_WAIT: Duration = Duration::from_millis(1500);

/// One invokable command: `twctl <args...> --notify <files...>`.
#[derive(Clone, Copy)]
pub(crate) struct Verb {
    /// Canonical verb name (GetCommandString / string invocation).
    pub(crate) name: &'static str,
    pub(crate) label: &'static str,
    pub(crate) args: &'static [&'static str],
}

const fn v(name: &'static str, label: &'static str, args: &'static [&'static str]) -> Verb {
    Verb { name, label, args }
}

pub(crate) enum Node {
    Separator,
    /// `id` is the visibility key (settings `HiddenMenuItems`), `single` = one file only.
    Leaf {
        id: &'static str,
        single: bool,
        verb: Verb,
    },
    Sub {
        id: &'static str,
        label: &'static str,
        single: bool,
        items: &'static [Verb],
    },
}

/// Submenu in the default order of FUNCTIONAL_SPEC.md 6.2, limited to the verbs this
/// product implements (`twctl` back ends).
pub(crate) const TREE: &[Node] = &[
    Node::Sub {
        id: "convert_into",
        label: "Convert into",
        single: false,
        items: &[
            v("tw.convert.png", "PNG", &["convert-to", "png"]),
            v("tw.convert.jpg", "JPG", &["convert-to", "jpg"]),
            v(
                "tw.convert.webp",
                "WebP (lossless)",
                &["convert-to", "webp"],
            ),
            v("tw.convert.avif", "AVIF", &["convert-to", "avif"]),
            v(
                "tw.convert.jxl",
                "JPEG XL (lossless)",
                &["convert-to", "jxl"],
            ),
            v("tw.convert.bmp", "BMP", &["convert-to", "bmp"]),
            v("tw.convert.gif", "GIF", &["convert-to", "gif"]),
            v("tw.convert.tiff", "TIFF", &["convert-to", "tiff"]),
            v("tw.convert.ico", "Icon (.ico)", &["convert-to", "ico"]),
        ],
    },
    Node::Sub {
        id: "resize",
        label: "Resize",
        single: false,
        items: &[
            v(
                "tw.resize.1080",
                "Fit 1920 x 1080",
                &["resize", "--to", "1920x1080"],
            ),
            v(
                "tw.resize.720",
                "Fit 1280 x 720",
                &["resize", "--to", "1280x720"],
            ),
            v(
                "tw.resize.600",
                "Fit 800 x 600",
                &["resize", "--to", "800x600"],
            ),
            v("tw.resize.50", "Scale to 50%", &["resize", "--to", "50%"]),
            v("tw.resize.25", "Scale to 25%", &["resize", "--to", "25%"]),
        ],
    },
    Node::Sub {
        id: "rotate",
        label: "Rotate / flip",
        single: false,
        items: &[
            v(
                "tw.rotate.right",
                "Rotate right 90\u{b0}",
                &["rotate", "--by", "right"],
            ),
            v(
                "tw.rotate.left",
                "Rotate left 90\u{b0}",
                &["rotate", "--by", "left"],
            ),
            v(
                "tw.rotate.180",
                "Rotate 180\u{b0}",
                &["rotate", "--by", "180"],
            ),
            v("tw.flip.h", "Flip horizontal", &["rotate", "--by", "fliph"]),
            v("tw.flip.v", "Flip vertical", &["rotate", "--by", "flipv"]),
        ],
    },
    Node::Separator,
    Node::Sub {
        id: "email",
        label: "Shrink for email",
        single: false,
        items: &[
            v(
                "tw.email.small",
                "Small (640 px)",
                &["email", "--edge", "640"],
            ),
            v(
                "tw.email.medium",
                "Medium (1024 px)",
                &["email", "--edge", "1024"],
            ),
            v(
                "tw.email.large",
                "Large (1600 px)",
                &["email", "--edge", "1600"],
            ),
        ],
    },
    Node::Sub {
        id: "compress",
        label: "Compress to size",
        single: false,
        items: &[
            v(
                "tw.compress.1",
                "Under 1 MB",
                &["compress", "--max-size", "1MB"],
            ),
            v(
                "tw.compress.5",
                "Under 5 MB",
                &["compress", "--max-size", "5MB"],
            ),
            v(
                "tw.compress.10",
                "Under 10 MB",
                &["compress", "--max-size", "10MB"],
            ),
        ],
    },
    Node::Leaf {
        id: "strip_meta",
        single: false,
        verb: v("tw.strip", "Strip metadata (EXIF/GPS)", &["strip"]),
    },
    Node::Separator,
    Node::Leaf {
        id: "copy",
        single: true,
        verb: v("tw.clip", "Copy to clipboard", &["clip"]),
    },
    Node::Leaf {
        id: "copy_text",
        single: true,
        verb: v("tw.ocr", "Copy text (OCR)", &["ocr", "--copy", "--window"]),
    },
    Node::Leaf {
        id: "copy_data_uri",
        single: true,
        verb: v("tw.datauri", "Copy as data URI", &["data-uri"]),
    },
    // Network verb: twctl asks for confirmation (Cancel is the default button, REQ-SEC-3)
    // before anything leaves the machine; the DLL itself never touches the network.
    Node::Leaf {
        id: "upload",
        single: false,
        verb: v(
            "tw.upload",
            "Upload (copy link)",
            &["upload", "--copy", "--confirm"],
        ),
    },
    Node::Separator,
    Node::Leaf {
        id: "combine_pdf",
        single: false,
        verb: v("tw.pdf", "Combine into PDF", &["pdf", "--auto"]),
    },
    Node::Leaf {
        id: "combine_pdf_searchable",
        single: false,
        verb: v(
            "tw.pdf.ocr",
            "Combine into searchable PDF (OCR)",
            &["pdf", "--auto", "--searchable"],
        ),
    },
    Node::Leaf {
        id: "combine_cbz",
        single: false,
        verb: v("tw.cbz", "Combine into CBZ (comic)", &["cbz", "--auto"]),
    },
    Node::Separator,
    Node::Sub {
        id: "rename",
        label: "Rename",
        single: false,
        items: &[
            v(
                "tw.rename.date",
                "By date taken",
                &["rename", "--by", "date"],
            ),
            v(
                "tw.rename.camera",
                "By camera + date",
                &["rename", "--by", "camera-date"],
            ),
            v(
                "tw.rename.artist",
                "By artist - title (audio)",
                &["rename", "--by", "artist-title"],
            ),
            v(
                "tw.rename.track",
                "By track - title (audio)",
                &["rename", "--by", "track-title"],
            ),
            v(
                "tw.rename.pattern",
                "Rename with pattern\u{2026}",
                &["rename", "--by", "pattern", "--ask"],
            ),
        ],
    },
    Node::Leaf {
        id: "files_to_folder",
        single: false,
        verb: v("tw.tofolder", "Files to folder", &["to-folder"]),
    },
    Node::Sub {
        id: "sort",
        label: "Sort into folders",
        single: false,
        items: &[
            v("tw.sort.size", "By image size", &["sort", "--by", "size"]),
            v("tw.sort.date", "By date taken", &["sort", "--by", "date"]),
            v(
                "tw.sort.tag",
                "By audio tag\u{2026}",
                &["sort", "--by", "tag", "--ask"],
            ),
        ],
    },
    Node::Separator,
    Node::Leaf {
        id: "image_info",
        single: true,
        verb: v("tw.info", "Image info", &["info", "--gui"]),
    },
    Node::Leaf {
        id: "pick_color",
        single: true,
        verb: v(
            "tw.pickcolor",
            "Pick color (eyedropper)\u{2026}",
            &["pick-color"],
        ),
    },
    Node::Separator,
    Node::Sub {
        id: "wallpaper",
        label: "Set as wallpaper",
        single: true,
        items: &[
            v(
                "tw.wall.stretch",
                "Stretched",
                &["wallpaper", "--style", "stretch"],
            ),
            v("tw.wall.tile", "Tiled", &["wallpaper", "--style", "tile"]),
            v(
                "tw.wall.center",
                "Centered",
                &["wallpaper", "--style", "center"],
            ),
            v("tw.wall.fill", "Filled", &["wallpaper", "--style", "fill"]),
            v("tw.wall.fit", "Fitted", &["wallpaper", "--style", "fit"]),
            v("tw.wall.span", "Spanned", &["wallpaper", "--style", "span"]),
        ],
    },
    Node::Leaf {
        id: "lock_screen",
        single: true,
        verb: v("tw.lockscreen", "Set as lock screen", &["lock-screen"]),
    },
    Node::Leaf {
        id: "folder_icon",
        single: true,
        verb: v("tw.foldericon", "Set as folder icon", &["folder-icon"]),
    },
    Node::Separator,
    // Always present (spec 6.3): `build` never hides it.
    Node::Leaf {
        id: SETTINGS_ID,
        single: false,
        verb: v("tw.settings", "Settings", &["settings"]),
    },
];

static SEPARATOR: Node = Node::Separator;

impl Node {
    /// The settings id of an item; separators have none.
    fn item_id(&self) -> Option<&'static str> {
        match self {
            Node::Separator => None,
            Node::Leaf { id, .. } | Node::Sub { id, .. } => Some(id),
        }
    }
}

/// The tree nodes in display order for the saved `MenuOrder`. With no saved order that is
/// `TREE` itself. Otherwise each item keeps the separator-delimited group it has in `TREE`
/// and a separator goes wherever consecutive items belong to different groups, so the
/// default order renders exactly as the default tree.
pub(crate) fn display_nodes(order: &[String]) -> Vec<&'static Node> {
    if order.is_empty() {
        return TREE.iter().collect();
    }
    let mut group = 0;
    let mut items: Vec<(usize, &'static Node)> = Vec::new();
    for node in TREE {
        match node {
            Node::Separator => group += 1,
            _ => items.push((group, node)),
        }
    }
    let mut out = Vec::with_capacity(items.len() * 2);
    let mut prev = None;
    for id in tw_core::config::ordered_menu_ids(order) {
        if let Some(&(group, node)) = items.iter().find(|(_, n)| n.item_id() == Some(id)) {
            if prev.is_some_and(|p| p != group) {
                out.push(&SEPARATOR);
            }
            out.push(node);
            prev = Some(group);
        }
    }
    out
}

/// The folder-menu entry: prebuilds the Explorer thumbnail cache for the folder.
const FOLDER_VERB: Verb = v("tw.prebuild", "Build thumbnails here", &["prebuild"]);

/// Item id that is always shown regardless of the hidden-items setting.
pub(crate) const SETTINGS_ID: &str = "settings";

/// Every verb in the tree, in a stable order (for string-verb lookup and tests).
fn all_verbs() -> impl Iterator<Item = Verb> {
    TREE.iter()
        .flat_map(|n| match n {
            Node::Separator => Vec::new(),
            Node::Leaf { verb, .. } => vec![*verb],
            Node::Sub { items, .. } => items.to_vec(),
        })
        .chain(std::iter::once(FOLDER_VERB))
}

/// Hand-off between the preview fetch thread and the menu object. Both sides touch it
/// only under the lock, and whichever side runs second frees the bitmap, so no
/// interleaving of "fetch finished" and "menu released" leaks a GDI handle.
#[derive(Default)]
struct Slot {
    bitmap: Option<isize>,
    done: bool,
    abandoned: bool,
}

type Shared = Arc<(Mutex<Slot>, Condvar)>;

fn lock(shared: &Shared) -> MutexGuard<'_, Slot> {
    shared.0.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Preview {
    shared: Shared,
    handle: Option<JoinHandle<()>>,
    bitmap: Option<HBITMAP>,
}

#[implement(IShellExtInit, IContextMenu, IContextMenu2, IContextMenu3)]
pub(crate) struct ContextMenu {
    files: RefCell<Vec<PathBuf>>,
    /// Verbs in command-offset order; `None` for non-command slots (preview).
    commands: RefCell<Vec<Option<Verb>>>,
    preview_id: Cell<Option<u32>>,
    preview: RefCell<Option<Preview>>,
    _module: ModuleRef,
}

impl ContextMenu {
    pub(crate) fn new() -> Self {
        Self {
            files: RefCell::new(Vec::new()),
            commands: RefCell::new(Vec::new()),
            preview_id: Cell::new(None),
            preview: RefCell::new(None),
            _module: ModuleRef::new(),
        }
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        if let Some(b) = self.bitmap.take() {
            // SAFETY: bitmap received from the fetch thread and owned by this preview.
            let _ = unsafe { DeleteObject(HGDIOBJ(b.0)) };
        }
        // A bitmap fetched but never drawn is ours to free; one fetched later is freed by
        // the fetch thread, which sees `abandoned`.
        let queued = {
            let mut slot = lock(&self.shared);
            slot.abandoned = true;
            slot.bitmap.take()
        };
        if let Some(raw) = queued {
            // SAFETY: an HBITMAP the fetch thread handed over and nobody else references.
            let _ = unsafe { DeleteObject(HGDIOBJ(raw as *mut _)) };
        }
        if let Some(h) = self.handle.take() {
            if h.is_finished() {
                let _ = h.join();
            } else {
                // The fetch thread still runs this DLL's code: never unload under it.
                tw_core::pipeline::pin_this_module();
            }
        }
    }
}

/// What `InvokeCommand` was asked to run.
#[derive(Debug, PartialEq, Eq)]
enum VerbRef {
    /// Command offset from `QueryContextMenu`'s `id_first`.
    Ordinal(usize),
    /// Canonical verb name.
    Name(String),
}

/// `IS_INTRESOURCE`: the pointer value carries an ordinal in its low word.
fn is_ordinal(p: usize) -> bool {
    p >> 16 == 0
}

/// Resolves the verb in the documented order: a Unicode string from an EX struct with
/// `CMIC_MASK_UNICODE`, else an ANSI string in `lpVerb`, else `LOWORD(lpVerb)`.
///
/// # Safety
/// `pici` must point to a valid `CMINVOKECOMMANDINFO`, or to the EX variant when
/// `cbSize` says so; non-ordinal verb pointers must be NUL-terminated strings.
unsafe fn resolve_verb(pici: *const CMINVOKECOMMANDINFO) -> VerbRef {
    // SAFETY: caller guarantees a valid struct.
    let info = unsafe { &*pici };
    if info.cbSize as usize >= size_of::<CMINVOKECOMMANDINFOEX>()
        && info.fMask & CMIC_MASK_UNICODE != 0
    {
        // SAFETY: EX layout confirmed by cbSize.
        let ex = unsafe { &*(pici as *const CMINVOKECOMMANDINFOEX) };
        if !is_ordinal(ex.lpVerbW.0 as usize) {
            // SAFETY: non-ordinal lpVerbW is a NUL-terminated UTF-16 string.
            return VerbRef::Name(unsafe { ex.lpVerbW.to_string() }.unwrap_or_default());
        }
    }
    let p = info.lpVerb.0 as usize;
    if is_ordinal(p) {
        VerbRef::Ordinal(p & 0xFFFF)
    } else {
        // SAFETY: non-ordinal lpVerb is a NUL-terminated ANSI string.
        VerbRef::Name(unsafe { info.lpVerb.to_string() }.unwrap_or_default())
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Files from a CF_HDROP data object.
fn dropped_files(data: &IDataObject) -> Result<Vec<PathBuf>> {
    let fmt = FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    // SAFETY: standard CF_HDROP retrieval; the medium is released below.
    unsafe {
        let mut medium = data.GetData(&fmt)?;
        let hdrop = HDROP(medium.u.hGlobal.0);
        let count = DragQueryFileW(hdrop, u32::MAX, None);
        let mut files = Vec::with_capacity(count as usize);
        for i in 0..count {
            let len = DragQueryFileW(hdrop, i, None) as usize;
            let mut buf = vec![0u16; len + 1];
            let n = DragQueryFileW(hdrop, i, Some(&mut buf)) as usize;
            files.push(PathBuf::from(OsString::from_wide(&buf[..n])));
        }
        ReleaseStgMedium(&mut medium);
        Ok(files)
    }
}

/// File-system path of a folder item-id list (long paths included).
fn folder_path(pidl: *const ITEMIDLIST) -> Result<PathBuf> {
    let mut buf = vec![0u16; 32_768];
    // SAFETY: `pidl` is the shell's folder PIDL (non-null, checked by the caller) and `buf`
    // holds the character count passed.
    let ok = unsafe { SHGetPathFromIDListEx(pidl, &mut buf, GPFIDL_DEFAULT) };
    if !ok.as_bool() {
        return Err(E_FAIL.into());
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Ok(PathBuf::from(OsString::from_wide(&buf[..n])))
}

fn ext_of(p: &Path) -> String {
    p.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// The menu shows only for supported types unless "Show menu on all file types" is on.
pub(crate) fn menu_applies(files: &[PathBuf], settings: &Settings) -> bool {
    if files.is_empty() || !settings.menu_enabled {
        return false;
    }
    settings.menu_all_file_types
        || files.iter().all(|f| {
            let e = ext_of(f);
            !tw_core::decode::route::chain_for_ext(&e).is_empty() && settings.type_enabled(&e)
        })
}

/// A selection made only of folders (the `Directory` handler), or the folder background
/// (the shell passes the folder itself): the menu then offers only "Build thumbnails here".
fn folder_selection(files: &[PathBuf]) -> bool {
    !files.is_empty() && files.iter().all(|f| f.is_dir())
}

fn make_hresult_success(code: u32) -> HRESULT {
    HRESULT(code as i32)
}

struct Builder<'a> {
    id_first: u32,
    id_last: u32,
    commands: &'a mut Vec<Option<Verb>>,
}

impl Builder<'_> {
    /// Allocates the next command id, or `None` when the shell's id range is exhausted.
    fn next_id(&mut self, verb: Option<Verb>) -> Option<u32> {
        let id = self.id_first.checked_add(self.commands.len() as u32)?;
        (id <= self.id_last).then(|| {
            self.commands.push(verb);
            id
        })
    }
}

unsafe fn insert(menu: HMENU, pos: u32, info: &MENUITEMINFOW) -> Result<()> {
    // SAFETY: caller passes a valid menu and a fully initialised MENUITEMINFOW.
    unsafe { InsertMenuItemW(menu, pos, true, info) }
}

fn item(mask_type: windows::Win32::UI::WindowsAndMessaging::MENU_ITEM_TYPE) -> MENUITEMINFOW {
    MENUITEMINFOW {
        cbSize: size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_FTYPE,
        fType: mask_type,
        ..Default::default()
    }
}

unsafe fn add_string(
    menu: HMENU,
    pos: u32,
    label: &str,
    id: u32,
    sub: Option<HMENU>,
) -> Result<()> {
    let mut text = wide(label);
    let mut info = item(MFT_STRING);
    info.fMask |= MIIM_STRING | MIIM_ID;
    info.wID = id;
    info.dwTypeData = PWSTR(text.as_mut_ptr());
    if let Some(s) = sub {
        info.fMask |= MIIM_SUBMENU;
        info.hSubMenu = s;
    }
    // SAFETY: `text` outlives the call (the menu copies the string).
    unsafe { insert(menu, pos, &info) }
}

unsafe fn add_separator(menu: HMENU, pos: u32) -> Result<()> {
    // SAFETY: valid menu.
    unsafe { insert(menu, pos, &item(MFT_SEPARATOR)) }
}

unsafe fn add_preview(menu: HMENU, pos: u32, id: u32) -> Result<()> {
    let mut info = item(MFT_OWNERDRAW);
    info.fMask |= MIIM_ID | MIIM_STATE;
    info.wID = id;
    info.fState = MFS_DISABLED;
    // SAFETY: valid menu.
    unsafe { insert(menu, pos, &info) }
}

impl ContextMenu_Impl {
    fn build(&self, hmenu: HMENU, index: u32, id_first: u32, id_last: u32) -> Result<u32> {
        let files = self.files.borrow();
        let settings = Settings::load(&crate::install_dir());
        if folder_selection(&files) {
            return self.build_folder(hmenu, index, id_first, id_last, &settings, &files);
        }
        if !menu_applies(&files, &settings) {
            return Ok(0);
        }
        let mut commands = self.commands.borrow_mut();
        commands.clear();
        let mut b = Builder {
            id_first,
            id_last,
            commands: &mut commands,
        };
        let single = files.len() == 1;
        let hidden =
            |id: &str| id != SETTINGS_ID && settings.hidden_menu_items.iter().any(|h| h == id);
        let want_preview = single && settings.menu_preview != MenuPreview::Off;

        // SAFETY: menu handles created here are owned by the parent menu once inserted.
        unsafe {
            let sub = CreatePopupMenu()?;
            let mut pos = 0u32;
            let mut top_pos = index;
            if want_preview && let Some(id) = b.next_id(None) {
                self.preview_id.set(Some(id));
                if settings.menu_preview == MenuPreview::InContextMenu {
                    add_preview(hmenu, top_pos, id)?;
                    top_pos += 1;
                } else {
                    add_preview(sub, pos, id)?;
                    pos += 1;
                    add_separator(sub, pos)?;
                    pos += 1;
                }
                self.start_preview_fetch(&files[0]);
            }
            let mut last_was_sep = pos == 0 || want_preview;
            for node in display_nodes(&settings.menu_order) {
                match node {
                    Node::Separator => {
                        if !last_was_sep {
                            add_separator(sub, pos)?;
                            pos += 1;
                            last_was_sep = true;
                        }
                    }
                    Node::Leaf {
                        id,
                        single: needs_one,
                        verb,
                    } => {
                        if hidden(id) || (*needs_one && !single) {
                            continue;
                        }
                        let Some(cmd) = b.next_id(Some(*verb)) else {
                            break;
                        };
                        add_string(sub, pos, verb.label, cmd, None)?;
                        pos += 1;
                        last_was_sep = false;
                    }
                    Node::Sub {
                        id,
                        label,
                        single: needs_one,
                        items,
                    } => {
                        if hidden(id) || (*needs_one && !single) {
                            continue;
                        }
                        let child = CreatePopupMenu()?;
                        for (i, verb) in items.iter().enumerate() {
                            let Some(cmd) = b.next_id(Some(*verb)) else {
                                break;
                            };
                            add_string(child, i as u32, verb.label, cmd, None)?;
                        }
                        let Some(cmd) = b.next_id(None) else { break };
                        add_string(sub, pos, label, cmd, Some(child))?;
                        pos += 1;
                        last_was_sep = false;
                    }
                }
            }
            let Some(root_id) = b.next_id(None) else {
                return Ok(b.commands.len() as u32);
            };
            add_string(hmenu, top_pos, ids::PRODUCT_NAME, root_id, Some(sub))?;
        }
        Ok(commands.len() as u32)
    }

    /// The folder menu: one entry that prebuilds the Explorer thumbnail cache.
    fn build_folder(
        &self,
        hmenu: HMENU,
        index: u32,
        id_first: u32,
        id_last: u32,
        settings: &Settings,
        _folders: &[PathBuf],
    ) -> Result<u32> {
        if !settings.menu_enabled || !settings.menu_build_here {
            return Ok(0);
        }
        let mut commands = self.commands.borrow_mut();
        commands.clear();
        let mut b = Builder {
            id_first,
            id_last,
            commands: &mut commands,
        };
        let Some(id) = b.next_id(Some(FOLDER_VERB)) else {
            return Ok(0);
        };
        // SAFETY: `hmenu` is the shell's menu; the label is copied by the menu.
        unsafe { add_string(hmenu, index, FOLDER_VERB.label, id, None)? };
        Ok(commands.len() as u32)
    }

    fn start_preview_fetch(&self, file: &Path) {
        let shared: Shared = Arc::default();
        let theirs = Arc::clone(&shared);
        let path = wide(&file.to_string_lossy());
        let handle = std::thread::Builder::new()
            .name("tw-menu-preview".into())
            .spawn(move || {
                // SAFETY: STA for shell item APIs on this thread; balanced uninit.
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
                            SIIGBF_THUMBNAILONLY | SIIGBF_BIGGERSIZEOK,
                        )
                    });
                    let mut slot = lock(&theirs);
                    if let Ok(b) = bmp {
                        if slot.abandoned {
                            let _ = DeleteObject(HGDIOBJ(b.0));
                        } else {
                            slot.bitmap = Some(b.0 as isize);
                        }
                    }
                    slot.done = true;
                    drop(slot);
                    theirs.1.notify_all();
                    if inited {
                        CoUninitialize();
                    }
                }
            })
            .ok();
        if handle.is_none() {
            lock(&shared).done = true;
        }
        *self.preview.borrow_mut() = Some(Preview {
            shared,
            handle,
            bitmap: None,
        });
    }

    fn preview_bitmap(&self) -> Option<HBITMAP> {
        let mut slot = self.preview.borrow_mut();
        let p = slot.as_mut()?;
        if p.bitmap.is_none() {
            let guard = lock(&p.shared);
            let (mut guard, _) = p
                .shared
                .1
                .wait_timeout_while(guard, PREVIEW_WAIT, |s| !s.done)
                .unwrap_or_else(PoisonError::into_inner);
            p.bitmap = guard.bitmap.take().map(|raw| HBITMAP(raw as *mut _));
        }
        p.bitmap
    }

    fn handle_msg(&self, msg: u32, lparam: LPARAM) -> Result<bool> {
        let Some(preview_id) = self.preview_id.get() else {
            return Ok(false);
        };
        match msg {
            WM_MEASUREITEM => {
                // SAFETY: for WM_MEASUREITEM, lparam points to a MEASUREITEMSTRUCT.
                let mi = unsafe { &mut *(lparam.0 as *mut MEASUREITEMSTRUCT) };
                if mi.itemID != preview_id {
                    return Ok(false);
                }
                mi.itemWidth = (PREVIEW_EDGE + 2 * PREVIEW_PAD) as u32;
                mi.itemHeight = (PREVIEW_EDGE + 2 * PREVIEW_PAD) as u32;
                Ok(true)
            }
            WM_DRAWITEM => {
                // SAFETY: for WM_DRAWITEM, lparam points to a DRAWITEMSTRUCT.
                let di = unsafe { &*(lparam.0 as *const DRAWITEMSTRUCT) };
                if di.itemID != preview_id {
                    return Ok(false);
                }
                // SAFETY: drawing into the DC the menu supplied, within its item rect.
                unsafe { draw_preview(di, self.preview_bitmap()) };
                Ok(true)
            }
            WM_INITMENUPOPUP => Ok(false),
            _ => Ok(false),
        }
    }
}

unsafe fn draw_preview(di: &DRAWITEMSTRUCT, bitmap: Option<HBITMAP>) {
    // SAFETY: GDI calls on the menu's DC; objects selected are restored and freed.
    unsafe {
        FillRect(di.hDC, &di.rcItem, GetSysColorBrush(COLOR_MENU));
        let Some(bmp) = bitmap else { return };
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
        let r: RECT = di.rcItem;
        let avail_w = (r.right - r.left - 2 * PREVIEW_PAD).max(1);
        let avail_h = (r.bottom - r.top - 2 * PREVIEW_PAD).max(1);
        let scale = (avail_w as f32 / bw as f32)
            .min(avail_h as f32 / bh as f32)
            .min(1.0);
        let (dw, dh) = ((bw as f32 * scale) as i32, (bh as f32 * scale) as i32);
        let x = r.left + (r.right - r.left - dw) / 2;
        let y = r.top + (r.bottom - r.top - dh) / 2;
        let mem = CreateCompatibleDC(Some(di.hDC));
        let old = SelectObject(mem, HGDIOBJ(bmp.0));
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let _ = AlphaBlend(di.hDC, x, y, dw, dh, mem, 0, 0, bw, bh, blend);
        SelectObject(mem, old);
        let _ = DeleteDC(mem);
    }
}

impl IShellExtInit_Impl for ContextMenu_Impl {
    fn Initialize(
        &self,
        folder: *const ITEMIDLIST,
        data: Ref<IDataObject>,
        _progid: HKEY,
    ) -> Result<()> {
        *self.files.borrow_mut() = match data.ok() {
            Ok(data) => dropped_files(data)?,
            // The folder background passes no data object, only the folder itself.
            Err(_) if !folder.is_null() => vec![folder_path(folder)?],
            Err(e) => return Err(e),
        };
        Ok(())
    }
}

impl IContextMenu_Impl for ContextMenu_Impl {
    fn QueryContextMenu(
        &self,
        hmenu: HMENU,
        index: u32,
        id_first: u32,
        id_last: u32,
        flags: u32,
    ) -> HRESULT {
        if flags & CMF_DEFAULTONLY != 0 {
            return make_hresult_success(0);
        }
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.build(hmenu, index, id_first, id_last)
        })) {
            Ok(Ok(n)) => make_hresult_success(n),
            Ok(Err(e)) => e.code(),
            Err(_) => E_FAIL,
        }
    }

    fn InvokeCommand(&self, pici: *const CMINVOKECOMMANDINFO) -> Result<()> {
        if pici.is_null() {
            return Err(E_INVALIDARG.into());
        }
        // SAFETY: the shell passes a valid CMINVOKECOMMANDINFO (or the EX variant when
        // cbSize says so).
        let info = unsafe { &*pici };
        // SAFETY: as above.
        let verb = match unsafe { resolve_verb(pici) } {
            VerbRef::Ordinal(offset) => self.commands.borrow().get(offset).copied().flatten(),
            VerbRef::Name(name) => all_verbs().find(|v| v.name.eq_ignore_ascii_case(&name)),
        };
        let verb = verb.ok_or(E_INVALIDARG)?;
        launch(&verb, &self.files.borrow(), info.hwnd)
    }

    fn GetCommandString(
        &self,
        idcmd: usize,
        utype: u32,
        _reserved: *const u32,
        name: PSTR,
        cchmax: u32,
    ) -> Result<()> {
        let verb = self.commands.borrow().get(idcmd).copied().flatten();
        let Some(verb) = verb else {
            return Err(E_INVALIDARG.into());
        };
        let text = match utype {
            GCS_VALIDATEA | GCS_VALIDATEW => return Ok(()),
            GCS_VERBA | GCS_VERBW => verb.name,
            GCS_HELPTEXTA | GCS_HELPTEXTW => verb.label,
            _ => return Err(E_NOTIMPL.into()),
        };
        if name.is_null() || cchmax == 0 {
            return Err(E_INVALIDARG.into());
        }
        let max = cchmax as usize - 1;
        // SAFETY: the caller's buffer holds `cchmax` characters of the requested width.
        unsafe {
            if utype == GCS_VERBW || utype == GCS_HELPTEXTW {
                let w: Vec<u16> = text.encode_utf16().take(max).collect();
                let dst = name.0.cast::<u16>();
                std::ptr::copy_nonoverlapping(w.as_ptr(), dst, w.len());
                *dst.add(w.len()) = 0;
            } else {
                let a: Vec<u8> = text.bytes().filter(u8::is_ascii).take(max).collect();
                std::ptr::copy_nonoverlapping(a.as_ptr(), name.0, a.len());
                *name.0.add(a.len()) = 0;
            }
        }
        Ok(())
    }
}

impl IContextMenu2_Impl for ContextMenu_Impl {
    fn HandleMenuMsg(&self, msg: u32, _wparam: WPARAM, lparam: LPARAM) -> Result<()> {
        self.handle_msg(msg, lparam).map(|_| ())
    }
}

impl IContextMenu3_Impl for ContextMenu_Impl {
    fn HandleMenuMsg2(
        &self,
        msg: u32,
        _wparam: WPARAM,
        lparam: LPARAM,
        result: *mut LRESULT,
    ) -> Result<()> {
        let handled = self.handle_msg(msg, lparam)?;
        if !result.is_null() {
            // SAFETY: optional out-pointer, checked non-null.
            unsafe { *result = LRESULT(handled as isize) };
        }
        Ok(())
    }
}

/// Command-line budget below the 32 767-character CreateProcess limit; longer
/// selections go through a temporary list file.
const CMDLINE_BUDGET: usize = 30_000;

pub(crate) fn launch(verb: &Verb, files: &[PathBuf], _hwnd: HWND) -> Result<()> {
    let started = Instant::now();
    let exe = crate::install_dir().join(ids::CTL_EXE_NAME);
    let mut args: Vec<OsString> = verb.args.iter().map(OsString::from).collect();
    args.push("--notify".into());
    let total: usize = files.iter().map(|f| f.as_os_str().len() + 3).sum();
    if total > CMDLINE_BUDGET {
        let list = std::env::temp_dir().join(format!(
            "tw-list-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let body: String = files.iter().map(|f| format!("{}\n", f.display())).collect();
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&list)
            .map_err(|_| E_FAIL)?;
        std::io::Write::write_all(&mut f, body.as_bytes()).map_err(|_| E_FAIL)?;
        drop(f);
        args.push("--files-from".into());
        args.push(list.into_os_string());
    } else {
        args.extend(files.iter().map(|f| f.as_os_str().to_owned()));
    }
    // Audit U-4: twctl gets NUL for its three standard handles and no other inherited handle.
    let child = tw_core::spawn::spawn(
        &exe,
        &args,
        tw_core::spawn::Stdio::Null,
        tw_core::spawn::Stdio::Null,
    )
    .map_err(|_| E_FAIL)?;
    log::verbose(Settings::load(&crate::install_dir()).verbose_log, || {
        let ext = files
            .first()
            .and_then(|f| f.extension())
            .map(|e| e.to_string_lossy().to_ascii_lowercase());
        format!(
            "InvokeCommand: ok class=menu verb={} files={} ext={} twctl_pid={} ms={}",
            verb.name,
            files.len(),
            ext.as_deref().unwrap_or("?"),
            child.id(),
            started.elapsed().as_millis()
        )
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verb_names_are_unique() {
        let names: Vec<_> = all_verbs().map(|v| v.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len());
    }

    #[test]
    fn folder_selection_needs_only_existing_directories() {
        let dir = std::env::temp_dir();
        assert!(folder_selection(std::slice::from_ref(&dir)));
        assert!(!folder_selection(&[]));
        assert!(!folder_selection(&[
            dir,
            PathBuf::from(r"C:\no\such\file.png")
        ]));
    }

    #[test]
    fn settings_is_last_and_present_exactly_once() {
        let ids: Vec<&str> = TREE
            .iter()
            .filter_map(|n| match n {
                Node::Leaf { id, .. } | Node::Sub { id, .. } => Some(*id),
                Node::Separator => None,
            })
            .collect();
        assert_eq!(ids.last(), Some(&SETTINGS_ID));
        assert_eq!(ids.iter().filter(|i| **i == SETTINGS_ID).count(), 1);
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "item ids are unique");
    }

    #[test]
    fn menu_gating() {
        let s = Settings::default();
        let png = vec![PathBuf::from(r"C:\a.png")];
        let exe = vec![PathBuf::from(r"C:\a.exe")];
        assert!(menu_applies(&png, &s));
        assert!(!menu_applies(&exe, &s));
        assert!(!menu_applies(&[png[0].clone(), exe[0].clone()], &s));
        let all = Settings {
            menu_all_file_types: true,
            ..Settings::default()
        };
        assert!(menu_applies(&exe, &all));
        let off = Settings {
            menu_enabled: false,
            ..Settings::default()
        };
        assert!(!menu_applies(&png, &off));
    }

    /// Item ids with `None` for separators, the shape of a menu.
    fn shape(nodes: &[&Node]) -> Vec<Option<&'static str>> {
        nodes.iter().map(|n| n.item_id()).collect()
    }

    #[test]
    fn every_tree_item_is_known_to_the_settings_app() {
        for id in TREE.iter().filter_map(Node::item_id) {
            assert!(
                tw_core::config::MENU_ITEMS.iter().any(|i| i.id == id),
                "{id} is missing from config::MENU_ITEMS (settings app, MenuOrder)"
            );
        }
    }

    #[test]
    fn default_order_renders_the_default_tree() {
        let tree: Vec<&Node> = TREE.iter().collect();
        assert_eq!(shape(&display_nodes(&[])), shape(&tree));
        let default: Vec<String> = tw_core::config::MENU_ITEMS
            .iter()
            .map(|i| i.id.to_string())
            .collect();
        assert_eq!(shape(&display_nodes(&default)), shape(&tree));
    }

    #[test]
    fn custom_order_moves_items_and_regroups_separators() {
        let order = ["wallpaper".to_string(), "resize".to_string()];
        let shown = shape(&display_nodes(&order));
        // wallpaper and resize sit in different default groups; the rest follow in default order.
        assert_eq!(&shown[..3], [Some("wallpaper"), None, Some("resize")]);
        assert!(!shown[3..].contains(&Some("wallpaper")) && !shown[3..].contains(&Some("resize")));
        let ids = shown.iter().flatten().count();
        assert_eq!(
            ids,
            TREE.iter().filter_map(Node::item_id).count(),
            "no item lost"
        );
        assert!(
            shown.windows(2).all(|w| w[0].is_some() || w[1].is_some()),
            "no doubled separators"
        );
    }

    fn verb_of(ex: &CMINVOKECOMMANDINFOEX) -> VerbRef {
        // SAFETY: a fully initialised EX struct whose strings outlive the call.
        unsafe { resolve_verb(std::ptr::from_ref(ex).cast()) }
    }

    fn ex_info() -> CMINVOKECOMMANDINFOEX {
        CMINVOKECOMMANDINFOEX {
            cbSize: size_of::<CMINVOKECOMMANDINFOEX>() as u32,
            ..Default::default()
        }
    }

    #[test]
    fn unicode_verb_wins_over_null_lpverb() {
        let w = wide("rotate-cw");
        let mut ex = ex_info();
        ex.fMask = CMIC_MASK_UNICODE;
        ex.lpVerbW = PCWSTR(w.as_ptr());
        // Old code read NULL lpVerb as ordinal 0.
        assert_eq!(verb_of(&ex), VerbRef::Name("rotate-cw".into()));
    }

    #[test]
    fn ordinal_and_ansi_verbs() {
        let mut ex = ex_info();
        ex.fMask = CMIC_MASK_UNICODE;
        ex.lpVerb = windows::core::PCSTR(3 as *const u8);
        ex.lpVerbW = PCWSTR(3 as *const u16);
        assert_eq!(verb_of(&ex), VerbRef::Ordinal(3));

        let a = std::ffi::CString::new("rotate-cw").unwrap();
        let mut ex = ex_info();
        ex.lpVerb = windows::core::PCSTR(a.as_ptr().cast());
        assert_eq!(verb_of(&ex), VerbRef::Name("rotate-cw".into()));

        // UNICODE set but lpVerbW is not a string: fall back to the ANSI verb.
        ex.fMask = CMIC_MASK_UNICODE;
        assert_eq!(verb_of(&ex), VerbRef::Name("rotate-cw".into()));
    }
}
