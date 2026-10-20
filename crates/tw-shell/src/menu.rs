//! Classic context menu (`IShellExtInit` + `IContextMenu{,2,3}`, REQ-COM-3), and the launcher
//! for the Windows 11 entry's popup.
//!
//! Runs inside Explorer, so it never decodes images (REQ-ISO-4): every verb launches
//! `twctl.exe` with the selection, and the preview row asks the shell's own thumbnail cache
//! (which extracts through the isolated surrogate) on a background thread. The menu tree is
//! `tw_core::menu`, shared with the popup `twctl menu` shows, so the two never drift apart.

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG, E_NOTIMPL, LPARAM, LRESULT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{DeleteObject, HBITMAP, HGDIOBJ};
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize, DVASPECT_CONTENT, FORMATETC,
    IDataObject, TYMED_HGLOBAL,
};
use windows::Win32::System::Ole::{CF_HDROP, ReleaseStgMedium};
use windows::Win32::System::Registry::HKEY;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    CMF_DEFAULTONLY, CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX, DragQueryFileW, GCS_HELPTEXTA,
    GCS_HELPTEXTW, GCS_VALIDATEA, GCS_VALIDATEW, GCS_VERBA, GCS_VERBW, GPFIDL_DEFAULT, HDROP,
    IContextMenu, IContextMenu_Impl, IContextMenu2, IContextMenu2_Impl, IContextMenu3,
    IContextMenu3_Impl, IShellExtInit, IShellExtInit_Impl, IShellItemImageFactory,
    SHCreateItemFromParsingName, SHGetPathFromIDListEx, SIIGBF_THUMBNAILONLY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    ASFW_ANY, AllowSetForegroundWindow, CreatePopupMenu, DestroyMenu, HMENU, InsertMenuItemW,
    MENUITEMINFOW, MFT_SEPARATOR, MFT_STRING, MIIM_FTYPE, MIIM_ID, MIIM_STRING, MIIM_SUBMENU,
    SetMenuItemInfoW, WM_INITMENUPOPUP,
};
use windows::core::{HRESULT, PCWSTR, PSTR, PWSTR, Ref, Result, implement};

use tw_core::config::Settings;
use tw_core::ids;
use tw_core::log;
use tw_core::menu::{self as tree, FOLDER_VERB, Selection, Shown, Verb};
use tw_core::menu_header::{self, HeaderBitmap};

use crate::ModuleRef;

/// `CMINVOKECOMMANDINFOEX` carries valid wide fields when this flag is set.
const CMIC_MASK_UNICODE: u32 = 0x0000_4000;
/// How long opening the submenu may wait for the shell's thumbnail.
const PREVIEW_WAIT: Duration = Duration::from_millis(300);

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
    /// Verbs in command-offset order; `None` for non-command slots (the root).
    commands: RefCell<Vec<Option<&'static Verb>>>,
    /// The submenu whose first row is the preview header, until the bitmap is attached.
    preview_menu: Cell<Option<HMENU>>,
    preview: RefCell<Option<Preview>>,
    /// The header's text, for the row that replaces it once the thumbnail is there.
    header_text: RefCell<String>,
    /// The composed header row, once the thumbnail is on it. The menu does not own it.
    header_bitmap: RefCell<Option<HeaderBitmap>>,
    _module: ModuleRef,
}

impl ContextMenu {
    pub(crate) fn new() -> Self {
        Self {
            files: RefCell::new(Vec::new()),
            commands: RefCell::new(Vec::new()),
            preview_menu: Cell::new(None),
            preview: RefCell::new(None),
            header_text: RefCell::new(String::new()),
            header_bitmap: RefCell::new(None),
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
        // A bitmap fetched but never shown is ours to free; one fetched later is freed by
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
pub(crate) fn folder_path(pidl: *const ITEMIDLIST) -> Result<PathBuf> {
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
/// (the shell passes the folder itself): the menu then offers only "Build thumbnails".
pub(crate) fn folder_selection(files: &[PathBuf]) -> bool {
    !files.is_empty() && files.iter().all(|f| f.is_dir())
}

fn make_hresult_success(code: u32) -> HRESULT {
    HRESULT(code as i32)
}

struct Builder<'a> {
    id_first: u32,
    id_last: u32,
    commands: &'a mut Vec<Option<&'static Verb>>,
}

impl Builder<'_> {
    /// Allocates the next command id, or `None` when the shell's id range is exhausted.
    fn next_id(&mut self, verb: Option<&'static Verb>) -> Option<u32> {
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
    id: Option<u32>,
    sub: Option<HMENU>,
) -> Result<()> {
    let mut text = wide(&tree::menu_text(label));
    let mut info = item(MFT_STRING);
    info.fMask |= MIIM_STRING;
    info.dwTypeData = PWSTR(text.as_mut_ptr());
    if let Some(id) = id {
        info.fMask |= MIIM_ID;
        info.wID = id;
    }
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

/// The first row of the submenu: the file's name and size, with no command (a click does
/// nothing). A plain themed item, so the menu keeps its dark theme. It is deliberately not
/// disabled: a themed menu draws a disabled row's bitmap greyed out, and the preview should
/// keep its colours. When the thumbnail arrives the row is replaced by a bitmap row that shows
/// the thumbnail with this text next to it (`tw_core::menu_header`): an item bitmap on this
/// row would widen the bitmap column of every row of the menu.
unsafe fn add_header(menu: HMENU, pos: u32, text: &str) -> Result<()> {
    let mut text = wide(&tree::menu_text(text));
    let mut info = item(MFT_STRING);
    info.fMask |= MIIM_STRING;
    info.dwTypeData = PWSTR(text.as_mut_ptr());
    // SAFETY: `text` outlives the call (the menu copies the string).
    unsafe { insert(menu, pos, &info) }
}

/// Inserts `entries` into `menu` from position `*pos`, taking a command id per verb. Returns
/// `false` once the shell's id range is used up: what fit stays, so a short range gives a
/// shorter menu, never an empty one. A separator is only written once an item follows it.
///
/// # Safety
/// `menu` must be a valid popup menu this thread may modify.
unsafe fn fill(
    b: &mut Builder,
    menu: HMENU,
    entries: &[Shown],
    pos: &mut u32,
    mut separator_due: bool,
) -> Result<bool> {
    for entry in entries {
        match entry {
            Shown::Separator => separator_due = separator_due || *pos > 0,
            Shown::Verb(verb) => {
                let Some(cmd) = b.next_id(Some(*verb)) else {
                    return Ok(false);
                };
                // SAFETY: `menu` is valid; the label is copied by the menu.
                unsafe {
                    if separator_due {
                        add_separator(menu, *pos)?;
                        *pos += 1;
                        separator_due = false;
                    }
                    add_string(menu, *pos, verb.label, Some(cmd), None)?;
                }
                *pos += 1;
            }
            Shown::Menu { label, items } => {
                // SAFETY: a menu created here is owned by `menu` once inserted, or destroyed.
                unsafe {
                    let child = CreatePopupMenu()?;
                    let mut child_pos = 0;
                    let complete = fill(b, child, items, &mut child_pos, false)?;
                    if child_pos == 0 {
                        let _ = DestroyMenu(child);
                    } else {
                        if separator_due {
                            add_separator(menu, *pos)?;
                            *pos += 1;
                            separator_due = false;
                        }
                        add_string(menu, *pos, label, None, Some(child))?;
                        *pos += 1;
                    }
                    if !complete {
                        return Ok(false);
                    }
                }
            }
        }
    }
    Ok(true)
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
        // Windows 11 already lists the packaged "Thumbwright" entry (and copies it into this
        // classic menu): a second root here would be its twin.
        if tree::classic_root_suppressed_now() {
            return Ok(0);
        }
        let mut commands = self.commands.borrow_mut();
        commands.clear();
        self.preview_menu.set(None);
        *self.header_bitmap.borrow_mut() = None;
        let header_text = tree::header_text(&files);
        let mut b = Builder {
            id_first,
            id_last,
            commands: &mut commands,
        };
        // The root takes the first id, so a short id range still gets a menu: whatever else
        // does not fit is left out instead of the whole menu.
        let Some(root_id) = b.next_id(None) else {
            return Ok(0);
        };
        let entries = tree::shown(&settings, &Selection::of(&files));
        let fetch_preview = files.len() == 1 && settings.menu_preview;

        // SAFETY: menu handles created here are owned by the parent menu once inserted.
        unsafe {
            let sub = CreatePopupMenu()?;
            let mut pos = 0u32;
            // File name and size, always; the thumbnail joins it when the shell has one.
            add_header(sub, pos, &header_text)?;
            pos += 1;
            if fetch_preview {
                self.preview_menu.set(Some(sub));
                self.start_preview_fetch(&files[0]);
            }
            fill(&mut b, sub, &entries, &mut pos, true)?;
            add_string(hmenu, index, ids::PRODUCT_NAME, Some(root_id), Some(sub))?;
        }
        *self.header_text.borrow_mut() = header_text;
        Ok(commands.len() as u32)
    }

    /// The folder menu: one entry that prebuilds the Explorer thumbnail cache. Where the
    /// packaged `FolderCommand` is shown (Windows 11 with the package registered) it is the
    /// entry, and Windows copies it into this classic menu ("Show more options"), so a
    /// classic entry here would be its twin: it appears only where the package does not
    /// (Windows 10, no package).
    fn build_folder(
        &self,
        hmenu: HMENU,
        index: u32,
        id_first: u32,
        id_last: u32,
        settings: &Settings,
        _folders: &[PathBuf],
    ) -> Result<u32> {
        if !settings.menu_enabled
            || !settings.menu_build_here
            || tree::classic_root_suppressed_now()
        {
            return Ok(0);
        }
        let mut commands = self.commands.borrow_mut();
        commands.clear();
        let mut b = Builder {
            id_first,
            id_last,
            commands: &mut commands,
        };
        let Some(id) = b.next_id(Some(&FOLDER_VERB)) else {
            return Ok(0);
        };
        // SAFETY: `hmenu` is the shell's menu; the label is copied by the menu.
        unsafe { add_string(hmenu, index, FOLDER_VERB.label, Some(id), None)? };
        Ok(commands.len() as u32)
    }

    fn start_preview_fetch(&self, file: &Path) {
        let shared: Shared = Arc::default();
        let theirs = Arc::clone(&shared);
        let path = wide(&file.to_string_lossy());
        let edge = menu_header::thumbnail_edge(menu_header::system_dpi());
        let handle = std::thread::Builder::new()
            .name("tw-menu-preview".into())
            .spawn(move || {
                // SAFETY: STA for shell item APIs on this thread; balanced uninit.
                unsafe {
                    let inited = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
                    // No BIGGERSIZEOK: the bitmap is composed as is, so it must fit the edge.
                    let bmp = SHCreateItemFromParsingName::<_, _, IShellItemImageFactory>(
                        PCWSTR(path.as_ptr()),
                        None,
                    )
                    .and_then(|f| f.GetImage(SIZE { cx: edge, cy: edge }, SIIGBF_THUMBNAILONLY));
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

    /// The fetched thumbnail, waiting up to `PREVIEW_WAIT` for the fetch thread.
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

    /// The shell is about to show `menu`: when it is the submenu, replace its header row by
    /// one that shows the thumbnail with the file's name and size beside it. The thumbnail is
    /// premultiplied 32-bpp (what the shell's thumbnail cache hands out); the row is composed
    /// from it (`tw_core::menu_header`), and the menu does not own the result: the menu object
    /// frees it, like the thumbnail.
    fn handle_msg(&self, msg: u32, wparam: WPARAM) -> Result<bool> {
        if msg != WM_INITMENUPOPUP {
            return Ok(false);
        }
        let Some(sub) = self.preview_menu.get() else {
            return Ok(false);
        };
        if wparam.0 != sub.0 as usize {
            return Ok(false);
        }
        self.preview_menu.set(None);
        if let Some(thumbnail) = self.preview_bitmap()
            && let Some(row) = menu_header::compose(
                &self.header_text.borrow(),
                thumbnail,
                menu_header::system_dpi(),
            )
        {
            // SAFETY: `sub` is the live submenu the shell is initialising; row 0 is the header.
            if unsafe { SetMenuItemInfoW(sub, 0, true, &row.item_info()) }.is_ok() {
                *self.header_bitmap.borrow_mut() = Some(row);
            }
        }
        Ok(false)
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
        let verb = match unsafe { resolve_verb(pici) } {
            VerbRef::Ordinal(offset) => self.commands.borrow().get(offset).copied().flatten(),
            VerbRef::Name(name) => tree::all_verbs()
                .into_iter()
                .find(|v| v.name.eq_ignore_ascii_case(&name)),
        };
        let verb = verb.ok_or(E_INVALIDARG)?;
        launch(verb, &self.files.borrow())
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
    fn HandleMenuMsg(&self, msg: u32, wparam: WPARAM, _lparam: LPARAM) -> Result<()> {
        self.handle_msg(msg, wparam).map(|_| ())
    }
}

impl IContextMenu3_Impl for ContextMenu_Impl {
    fn HandleMenuMsg2(
        &self,
        msg: u32,
        wparam: WPARAM,
        _lparam: LPARAM,
        result: *mut LRESULT,
    ) -> Result<()> {
        let handled = self.handle_msg(msg, wparam)?;
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

/// Starts `twctl` with `args`: NUL for its three standard handles and no other inherited
/// handle (audit U-4).
fn spawn_twctl(args: &[OsString]) -> Result<tw_core::spawn::Child> {
    let exe = crate::install_dir().join(ids::CTL_EXE_NAME);
    tw_core::spawn::spawn(
        &exe,
        args,
        tw_core::spawn::Stdio::Null,
        tw_core::spawn::Stdio::Null,
    )
    .map_err(|_| E_FAIL.into())
}

fn trace_launch(verb: &str, files: &[PathBuf], child: &tw_core::spawn::Child, started: Instant) {
    log::verbose(Settings::load(&crate::install_dir()).verbose_log, || {
        let ext = files
            .first()
            .and_then(|f| f.extension())
            .map(|e| e.to_string_lossy().to_ascii_lowercase());
        format!(
            "InvokeCommand: ok class=menu verb={verb} files={} ext={} twctl_pid={} ms={}",
            files.len(),
            ext.as_deref().unwrap_or("?"),
            child.id(),
            started.elapsed().as_millis()
        )
    });
}

/// Runs one verb on `files`: `twctl <args...> --notify <files...>`.
pub(crate) fn launch(verb: &Verb, files: &[PathBuf]) -> Result<()> {
    let started = Instant::now();
    let files = if verb.files { files } else { &[] };
    let total: usize = files.iter().map(|f| f.as_os_str().len() + 3).sum();
    let args = if total > CMDLINE_BUDGET {
        let list = tree::write_list(files).map_err(|_| E_FAIL)?;
        let mut args = verb.argv(&[]);
        args.push("--files-from".into());
        args.push(list.into_os_string());
        args
    } else {
        verb.argv(files)
    };
    let child = spawn_twctl(&args)?;
    trace_launch(verb.name, files, &child, started);
    Ok(())
}

/// Opens Thumbwright's own popup menu for `files` at screen position `at` (the Windows 11
/// entry's action): `twctl menu --at x,y --notify --files-from <list>`. The selection goes
/// through a list file whatever its size. Explorer owns the foreground when this runs, so
/// the new process is allowed to take it, or its popup would open behind the window.
pub(crate) fn launch_popup(files: &[PathBuf], at: (i32, i32)) -> Result<()> {
    let started = Instant::now();
    let list = tree::write_list(files).map_err(|_| E_FAIL)?;
    let args: Vec<OsString> = [
        "menu".into(),
        "--at".into(),
        format!("{},{}", at.0, at.1).into(),
        "--notify".into(),
        "--files-from".into(),
        list.clone().into_os_string(),
    ]
    .into();
    // SAFETY: no pointers; failure only means the popup may open without focus.
    let _ = unsafe { AllowSetForegroundWindow(ASFW_ANY) };
    let child = match spawn_twctl(&args) {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&list);
            return Err(e);
        }
    };
    trace_launch("menu", files, &child, started);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Item ids and the number of menu rows `fill` produced for a PNG, for an id range of
    /// `range` commands.
    fn filled(range: u32) -> (Vec<Option<&'static Verb>>, u32) {
        let entries = tree::shown(
            &Settings::default(),
            &Selection::of(&[PathBuf::from("a.png")]),
        );
        let mut commands = Vec::new();
        let mut b = Builder {
            id_first: 1,
            id_last: range,
            commands: &mut commands,
        };
        // SAFETY: a fresh menu owned by this test.
        let rows = unsafe {
            let menu = CreatePopupMenu().unwrap();
            let mut pos = 0;
            fill(&mut b, menu, &entries, &mut pos, false).unwrap();
            let _ = DestroyMenu(menu);
            pos
        };
        (commands, rows)
    }

    #[test]
    fn fill_stops_at_the_end_of_the_id_range_and_keeps_what_fit() {
        let (all, all_rows) = filled(0x7FFF);
        assert!(all.len() > 30 && all_rows > 10, "{} commands", all.len());
        for range in [1, 2, 5, 12] {
            let (commands, rows) = filled(range);
            assert_eq!(commands.len() as u32, range, "uses exactly the range");
            assert!(rows > 0, "range {range} still shows something");
        }
        let (none, rows) = filled(0);
        assert!(none.is_empty() && rows == 0);
    }
}
