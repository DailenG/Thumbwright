//! Windows 11 modern context menu: two `IExplorerCommand`s, declared by the sparse package in
//! `packaging/msix`. Windows 11 lists only packaged verbs at the top level of the menu (the
//! classic `IContextMenu` handlers show under "Show more options"), so each entry the menu
//! offers needs a packaged twin.
//!
//! [`ExplorerCommand`] (file types, `Type="*"`) is a leaf titled "Thumbwright" with the
//! product icon, with no subcommands: Windows drops the second level of a packaged command
//! tree and copies the verb into the classic menu cut off after 16 commands. Invoking it
//! opens Thumbwright's own popup menu (`twctl menu`, built from the same `tw_core::menu`
//! tree as the classic menu), at the cursor. `GetState` shows it exactly when the classic
//! menu would: the menu is enabled and the selection is of supported types (or "all file
//! types" is on).
//!
//! [`FolderCommand`] (`Directory` and `Directory\Background`) is the folder entry "Build
//! thumbnails for this folder": a leaf that runs the same verb as the classic folder entry.
//! Windows may pass no items for the folder background, so it also takes the `IObjectWithSite`
//! site and reads the folder the window shows from it.
//!
//! Like the classic menu, neither ever decodes images (REQ-ISO-4).

use std::cell::RefCell;
use std::path::PathBuf;

use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG, E_NOTIMPL, E_POINTER, POINT};
use windows::Win32::System::Com::{CoTaskMemFree, IBindCtx, IServiceProvider};
use windows::Win32::System::Ole::{IObjectWithSite, IObjectWithSite_Impl};
use windows::Win32::UI::Shell::{
    ECF_DEFAULT, ECS_ENABLED, ECS_HIDDEN, IEnumExplorerCommand, IExplorerCommand,
    IExplorerCommand_Impl, IFolderView, IPersistFolder2, IShellBrowser, IShellItemArray, SHStrDupW,
    SID_STopLevelBrowser, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
use windows::core::{BOOL, GUID, HSTRING, IUnknown, Interface, PWSTR, Ref, Result, implement};

use tw_core::config::Settings;
use tw_core::ids;
use tw_core::menu::FOLDER_VERB;

use crate::ModuleRef;
use crate::menu::{folder_path, folder_selection, launch, launch_popup, menu_applies};

fn dup(s: &str) -> Result<PWSTR> {
    // SAFETY: SHStrDupW returns a CoTaskMem copy; ownership passes to the caller (the shell).
    unsafe { SHStrDupW(&HSTRING::from(s)) }
}

/// File-system paths of `items`; `None` when any item has no file-system path (a virtual
/// item cannot be handed to `twctl`).
fn paths(items: Option<&IShellItemArray>) -> Option<Vec<PathBuf>> {
    let items = items?;
    // SAFETY: plain shell item queries; the display-name buffer is freed after copying.
    unsafe {
        let n = items.GetCount().ok()?;
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            let name = items
                .GetItemAt(i)
                .ok()?
                .GetDisplayName(SIGDN_FILESYSPATH)
                .ok()?;
            let path = name.to_string().ok();
            CoTaskMemFree(Some(name.0 as *const _));
            out.push(PathBuf::from(path?));
        }
        Some(out)
    }
}

#[implement(IExplorerCommand)]
pub(crate) struct ExplorerCommand {
    _module: ModuleRef,
}

impl ExplorerCommand {
    /// The command, what the class factory creates.
    pub(crate) fn new() -> Self {
        Self {
            _module: ModuleRef::new(),
        }
    }
}

impl IExplorerCommand_Impl for ExplorerCommand_Impl {
    fn GetTitle(&self, _items: Ref<IShellItemArray>) -> Result<PWSTR> {
        dup(ids::PRODUCT_NAME)
    }

    /// The product icon, see [`icon`].
    fn GetIcon(&self, _items: Ref<IShellItemArray>) -> Result<PWSTR> {
        icon()
    }

    fn GetToolTip(&self, _items: Ref<IShellItemArray>) -> Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetCanonicalName(&self) -> Result<GUID> {
        Ok(GUID::from_u128(ids::CLSID_EXPLORER_COMMAND.guid))
    }

    fn GetState(&self, items: Ref<IShellItemArray>, _ok_to_be_slow: BOOL) -> Result<u32> {
        let settings = Settings::load(&crate::install_dir());
        let visible = paths(items.as_ref()).is_some_and(|files| menu_applies(&files, &settings));
        Ok((if visible { ECS_ENABLED } else { ECS_HIDDEN }).0 as u32)
    }

    fn Invoke(&self, items: Ref<IShellItemArray>, _bind: Ref<IBindCtx>) -> Result<()> {
        let files = paths(items.as_ref())
            .filter(|f| !f.is_empty())
            .ok_or(E_INVALIDARG)?;
        // Where the user clicked: the popup opens at the cursor, like the menu it replaces.
        let mut at = POINT::default();
        // SAFETY: `at` is a valid out-pointer; on failure the popup opens at the origin.
        let _ = unsafe { GetCursorPos(&mut at) };
        launch_popup(&files, (at.x, at.y))
    }

    fn GetFlags(&self) -> Result<u32> {
        Ok(ECF_DEFAULT.0 as u32)
    }

    /// A leaf: there is no submenu for Windows to mishandle.
    fn EnumSubCommands(&self) -> Result<IEnumExplorerCommand> {
        Err(E_NOTIMPL.into())
    }
}

/// The product icon: group icon id 1 in this DLL's resources (`build.rs`), as an icon
/// resource location `"<dll path>,-1"`.
fn icon() -> Result<PWSTR> {
    dup(&format!("{},-1", crate::module_path().display()))
}

/// The folder the window behind `site` shows, as a file-system path: the documented pattern
/// of Windows Terminal's "Open in Terminal". The site is a service provider; its top-level
/// browser has the active shell view, whose folder view names the folder being viewed.
/// `None` when any step is missing or the folder is virtual (This PC, Recycle Bin...).
fn site_folder(site: &IUnknown) -> Option<PathBuf> {
    // SAFETY: plain COM queries on the shell's own objects; the PIDL returned by
    // `GetCurFolder` is CoTaskMem and freed after the path is copied out of it.
    unsafe {
        let browser: IShellBrowser = site
            .cast::<IServiceProvider>()
            .ok()?
            .QueryService(&SID_STopLevelBrowser)
            .ok()?;
        let view = browser.QueryActiveShellView().ok()?;
        let folder: IPersistFolder2 = view.cast::<IFolderView>().ok()?.GetFolder().ok()?;
        let pidl = folder.GetCurFolder().ok()?;
        if pidl.is_null() {
            return None;
        }
        let path = folder_path(pidl);
        CoTaskMemFree(Some(pidl as *const _));
        path.ok()
    }
}

/// The folders an invocation acts on: the listed items (`Directory`: the selected folders),
/// or, when Windows lists none (`Directory\Background`), the folder behind `site`. `None`
/// unless every target is an existing file-system folder.
fn folder_targets(
    items: Option<&IShellItemArray>,
    site: Option<&IUnknown>,
) -> Option<Vec<PathBuf>> {
    // SAFETY: plain shell item query.
    let listed = items.filter(|i| unsafe { i.GetCount() }.is_ok_and(|n| n > 0));
    let folders = match listed {
        Some(items) => paths(Some(items))?,
        None => vec![site_folder(site?)?],
    };
    folder_selection(&folders).then_some(folders)
}

/// The folder entry: "Build thumbnails for this folder" on folders and on the folder
/// background, the packaged twin of the classic `ContextMenu` folder entry.
#[implement(IExplorerCommand, IObjectWithSite)]
pub(crate) struct FolderCommand {
    /// What Explorer handed to `SetSite`: the source of the folder on the background.
    site: RefCell<Option<IUnknown>>,
    _module: ModuleRef,
}

impl FolderCommand {
    /// The command, what the class factory creates.
    pub(crate) fn new() -> Self {
        Self {
            site: RefCell::new(None),
            _module: ModuleRef::new(),
        }
    }
}

impl IObjectWithSite_Impl for FolderCommand_Impl {
    fn SetSite(&self, site: Ref<IUnknown>) -> Result<()> {
        *self.site.borrow_mut() = site.as_ref().cloned();
        Ok(())
    }

    fn GetSite(&self, riid: *const GUID, ppv: *mut *mut std::ffi::c_void) -> Result<()> {
        if riid.is_null() || ppv.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: checked non-null; COM out-parameter contract.
        unsafe { *ppv = std::ptr::null_mut() };
        let site = self.site.borrow().clone().ok_or(E_FAIL)?;
        // SAFETY: riid/ppv valid per the COM contract.
        unsafe { site.query(riid, ppv).ok() }
    }
}

impl IExplorerCommand_Impl for FolderCommand_Impl {
    fn GetTitle(&self, _items: Ref<IShellItemArray>) -> Result<PWSTR> {
        dup(FOLDER_VERB.label)
    }

    fn GetIcon(&self, _items: Ref<IShellItemArray>) -> Result<PWSTR> {
        icon()
    }

    fn GetToolTip(&self, _items: Ref<IShellItemArray>) -> Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetCanonicalName(&self) -> Result<GUID> {
        Ok(GUID::from_u128(ids::CLSID_FOLDER_COMMAND.guid))
    }

    /// Shown exactly when the classic folder entry would be: the menu and "Build thumbnails
    /// for this folder" are on and the target is a file-system folder.
    fn GetState(&self, items: Ref<IShellItemArray>, _ok_to_be_slow: BOOL) -> Result<u32> {
        let settings = Settings::load(&crate::install_dir());
        let visible = settings.menu_enabled
            && settings.menu_build_here
            && folder_targets(items.as_ref(), self.site.borrow().as_ref()).is_some();
        Ok((if visible { ECS_ENABLED } else { ECS_HIDDEN }).0 as u32)
    }

    /// What the classic folder entry runs: `twctl prebuild --notify <folders>`.
    fn Invoke(&self, items: Ref<IShellItemArray>, _bind: Ref<IBindCtx>) -> Result<()> {
        let folders =
            folder_targets(items.as_ref(), self.site.borrow().as_ref()).ok_or(E_INVALIDARG)?;
        launch(&FOLDER_VERB, &folders)
    }

    fn GetFlags(&self) -> Result<u32> {
        Ok(ECF_DEFAULT.0 as u32)
    }

    /// A leaf: there is no submenu for Windows to mishandle.
    fn EnumSubCommands(&self) -> Result<IEnumExplorerCommand> {
        Err(E_NOTIMPL.into())
    }
}
