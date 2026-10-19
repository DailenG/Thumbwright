//! Windows 11 modern context menu: one `IExplorerCommand`, declared by the sparse package in
//! `packaging/msix`. It is a leaf titled "Thumbwright" with the product icon, with no
//! subcommands: Windows drops the second level of a packaged command tree and copies the
//! verb into the classic menu cut off after 16 commands. Invoking it opens Thumbwright's own
//! popup menu (`twctl menu`, built from the same `tw_core::menu` tree as the classic menu), at
//! the cursor. Like the classic menu it never decodes images (REQ-ISO-4).
//!
//! `GetState` shows the entry exactly when the classic menu would: the menu is enabled and
//! the selection is of supported types (or "all file types" is on).

use std::path::PathBuf;

use windows::Win32::Foundation::{E_INVALIDARG, E_NOTIMPL, POINT};
use windows::Win32::System::Com::{CoTaskMemFree, IBindCtx};
use windows::Win32::UI::Shell::{
    ECF_DEFAULT, ECS_ENABLED, ECS_HIDDEN, IEnumExplorerCommand, IExplorerCommand,
    IExplorerCommand_Impl, IShellItemArray, SHStrDupW, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
use windows::core::{BOOL, GUID, HSTRING, PWSTR, Ref, Result, implement};

use tw_core::config::Settings;
use tw_core::ids;

use crate::ModuleRef;
use crate::menu::{launch_popup, menu_applies};

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

    /// The product icon: group icon id 1 in this DLL's resources (`build.rs`), as an icon
    /// resource location `"<dll path>,-1"`.
    fn GetIcon(&self, _items: Ref<IShellItemArray>) -> Result<PWSTR> {
        dup(&format!("{},-1", crate::module_path().display()))
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
