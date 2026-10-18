//! Thumbwright shell extension: the in-process COM server loaded by the OS thumbnail
//! surrogate (`IThumbnailProvider` + `IInitializeWithStream`), by Explorer
//! (`IShellExtInit` + `IContextMenu{,2,3}`), and, for the installer-registered classes, by
//! the cloud-folder surrogate (`cloud`), `prevhost.exe` (`preview`), the property system
//! (`property`) and the Windows 11 modern menu (`explorer_command`, declared by the sparse
//! package under `packaging/msix`).
#![cfg(windows)]

mod bitmap;
mod cloud;
mod explorer_command;
mod factory;
mod menu;
mod preview;
mod property;
mod thumbnail;

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, AtomicPtr, Ordering};

use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, E_POINTER, HINSTANCE, HMODULE, S_FALSE, S_OK, TRUE,
};
use windows::Win32::System::LibraryLoader::{DisableThreadLibraryCalls, GetModuleFileNameW};
use windows::Win32::System::Ole::SELFREG_E_CLASS;
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows::core::{BOOL, GUID, HRESULT, IUnknown, Interface};

use tw_core::registration::{self, InstallerPlan, Layout, Plan, Scope};

static MODULE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
/// Live COM objects plus `IClassFactory::LockServer` locks (DllCanUnloadNow).
static REFS: AtomicIsize = AtomicIsize::new(0);

/// Embedded in every COM object so the DLL stays loaded while any object is alive.
pub(crate) struct ModuleRef;

impl ModuleRef {
    pub(crate) fn new() -> Self {
        REFS.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for ModuleRef {
    fn drop(&mut self) {
        REFS.fetch_sub(1, Ordering::SeqCst);
    }
}

pub(crate) fn lock_server(lock: bool) {
    REFS.fetch_add(if lock { 1 } else { -1 }, Ordering::SeqCst);
}

/// Full path of this DLL.
pub(crate) fn module_path() -> PathBuf {
    let mut buf = vec![0u16; 32768];
    let module = HMODULE(MODULE.load(Ordering::Relaxed));
    // SAFETY: `buf` is a valid writable buffer; a null module means "the exe", which only
    // happens when this crate is linked as an rlib (tests), never in the shipped DLL.
    let n = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    PathBuf::from(String::from_utf16_lossy(&buf[..n]))
}

/// Directory holding the DLL, `tw-worker.exe`, `twctl.exe` and the portable INI.
pub(crate) fn install_dir() -> PathBuf {
    let mut p = module_path();
    p.pop();
    p
}

#[unsafe(no_mangle)]
extern "system" fn DllMain(hinst: HINSTANCE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        MODULE.store(hinst.0, Ordering::Relaxed);
        // SAFETY: valid module handle from the loader.
        let _ = unsafe { DisableThreadLibraryCalls(HMODULE(hinst.0)) };
    }
    TRUE
}

/// # Safety
/// Called by COM with valid pointers (`ppv` may be null, which is rejected).
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() || rclsid.is_null() || riid.is_null() {
        return E_POINTER;
    }
    // SAFETY: checked non-null above.
    unsafe { *ppv = std::ptr::null_mut() };
    let clsid = unsafe { *rclsid };
    let Some(kind) = factory::Kind::from_clsid(&clsid) else {
        return CLASS_E_CLASSNOTAVAILABLE;
    };
    let unknown: IUnknown = factory::ClassFactory::new(kind).into();
    // SAFETY: riid/ppv valid per the COM contract.
    unsafe { unknown.query(riid, ppv) }
}

/// The instance handle of this DLL, for window classes.
pub(crate) fn module_handle() -> HINSTANCE {
    HINSTANCE(MODULE.load(Ordering::Relaxed))
}

#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    if REFS.load(Ordering::SeqCst) == 0 {
        S_OK
    } else {
        S_FALSE
    }
}

fn machine_plan_extensions() -> Vec<&'static str> {
    tw_core::formats::all()
        .iter()
        .map(|f| f.ext)
        .filter(|e| !tw_core::decode::route::chain_for_ext(e).is_empty())
        .collect()
}

/// Extensions of plain image files: the preview handler shows the decoded image for these.
/// Documents, archives, audio and RAW containers keep their own previewers.
fn preview_extensions() -> Vec<&'static str> {
    use tw_core::decode::route::{Decoder, chain_for_ext};
    tw_core::formats::all()
        .iter()
        .map(|f| f.ext)
        .filter(|e| {
            matches!(
                chain_for_ext(e).first(),
                Some(Decoder::Raster | Decoder::Jxl | Decoder::Pcx | Decoder::Sgi | Decoder::Svg)
            )
        })
        .collect()
}

/// Extensions whose header the property handler can read (`decode::probe`).
fn property_extensions() -> Vec<&'static str> {
    tw_core::formats::all()
        .iter()
        .map(|f| f.ext)
        .filter(|e| tw_core::decode::raster::format_for_ext(e).is_some())
        .collect()
}

/// Machine-wide registration for the elevated installer (`regsvr32`), REQ-REG-8.
#[unsafe(no_mangle)]
pub extern "system" fn DllRegisterServer() -> HRESULT {
    let dll = module_path();
    let plan = Plan {
        dll_path: &dll,
        extensions: machine_plan_extensions(),
        context_menu: true,
        cloud: true,
        installer: Some(InstallerPlan {
            preview_extensions: preview_extensions(),
            property_extensions: property_extensions(),
        }),
    };
    match registration::register(&Layout::for_scope(Scope::Machine), &plan) {
        Ok(_) => S_OK,
        Err(e) => {
            tw_core::log::error(&format!("DllRegisterServer: {e}"));
            SELFREG_E_CLASS
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn DllUnregisterServer() -> HRESULT {
    match registration::unregister(&Layout::for_scope(Scope::Machine)) {
        Ok(_) => S_OK,
        Err(e) => {
            tw_core::log::error(&format!("DllUnregisterServer: {e}"));
            SELFREG_E_CLASS
        }
    }
}
