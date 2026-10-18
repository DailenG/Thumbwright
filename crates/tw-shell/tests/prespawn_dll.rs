//! The shipped DLL turns worker pre-spawning on (an SVG request leaves an idle
//! `tw-worker.exe` child behind) and ends those workers when COM asks whether it can be
//! unloaded, so none outlives the module that owns their pipes.
#![cfg(windows)]

use std::ffi::c_void;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HMODULE, S_OK};
use windows::Win32::Graphics::Gdi::{DeleteObject, HBITMAP, HGDIOBJ};
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, CoInitializeEx, IClassFactory, IStream, STGM_READ,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Shell::PropertiesSystem::IInitializeWithStream;
use windows::Win32::UI::Shell::{IThumbnailProvider, SHCreateMemStream, WTS_ALPHATYPE};
use windows::core::{GUID, HRESULT, HSTRING, Interface};

const CLSID_THUMBNAIL: GUID = GUID::from_u128(tw_core::ids::CLSID_THUMBNAIL.guid);

type GetClassObject =
    unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;
type CanUnloadNow = unsafe extern "system" fn() -> HRESULT;

/// Stages the shipped layout (DLL + worker side by side) from Cargo's build outputs.
fn stage() -> PathBuf {
    let worker = PathBuf::from(env!("CARGO_BIN_EXE_tw-worker"));
    let target = worker.parent().unwrap();
    for entry in std::fs::read_dir(target).unwrap().flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with("tw-test-prespawn-")
        {
            // Directories still loaded by a live test process fail to delete; leave those.
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    let dir = target.join(format!("tw-test-prespawn-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(
        target.join("deps").join(tw_core::ids::SHELL_DLL_NAME),
        dir.join(tw_core::ids::SHELL_DLL_NAME),
    )
    .unwrap();
    std::fs::copy(&worker, dir.join(tw_core::ids::WORKER_EXE_NAME)).unwrap();
    // A freshly copied executable is scanned by antivirus on its first launch.
    let ok = std::process::Command::new(dir.join(tw_core::ids::WORKER_EXE_NAME))
        .arg("--version")
        .output();
    assert!(ok.is_ok_and(|o| o.status.success()));
    dir
}

/// Live `tw-worker.exe` processes whose parent is this process.
fn worker_children() -> usize {
    let me = std::process::id();
    let exe = tw_core::ids::WORKER_EXE_NAME;
    // SAFETY: a toolhelp snapshot walked with correctly sized entries, then closed.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).unwrap();
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut count = 0;
        let mut more = Process32FirstW(snap, &mut entry).is_ok();
        while more {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            if entry.th32ParentProcessID == me
                && String::from_utf16_lossy(&entry.szExeFile[..len]).eq_ignore_ascii_case(exe)
            {
                count += 1;
            }
            more = Process32NextW(snap, &mut entry).is_ok();
        }
        let _ = CloseHandle(snap);
        count
    }
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "timed out: {what}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn svg_thumbnail(get_class_object: GetClassObject) {
    let svg: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10" fill="#00ff00"/></svg>"##;
    // SAFETY: standard COM calls; every object is released before returning.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let mut raw = std::ptr::null_mut();
        get_class_object(&CLSID_THUMBNAIL, &IClassFactory::IID, &mut raw)
            .ok()
            .unwrap();
        let factory = IClassFactory::from_raw(raw);
        let provider: IThumbnailProvider = factory.CreateInstance(None).unwrap();
        let stream: IStream = SHCreateMemStream(Some(svg)).unwrap();
        provider
            .cast::<IInitializeWithStream>()
            .unwrap()
            .Initialize(&stream, STGM_READ.0)
            .unwrap();
        let mut hbmp = HBITMAP::default();
        let mut alpha = WTS_ALPHATYPE::default();
        provider.GetThumbnail(32, &mut hbmp, &mut alpha).unwrap();
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
    }
}

#[test]
fn dll_prespawns_a_worker_and_drains_it_before_unload() {
    let dir = stage();
    let dll = dir.join(tw_core::ids::SHELL_DLL_NAME);
    assert_eq!(worker_children(), 0);
    // SAFETY: loading our own DLL by absolute path and calling its documented exports.
    let (get_class_object, can_unload): (GetClassObject, CanUnloadNow) = unsafe {
        let module: HMODULE = LoadLibraryW(&HSTRING::from(dll.as_os_str())).unwrap();
        let get = GetProcAddress(module, windows::core::s!("DllGetClassObject")).unwrap();
        let can = GetProcAddress(module, windows::core::s!("DllCanUnloadNow")).unwrap();
        (
            std::mem::transmute::<unsafe extern "system" fn() -> isize, GetClassObject>(get),
            std::mem::transmute::<unsafe extern "system" fn() -> isize, CanUnloadNow>(can),
        )
    };

    svg_thumbnail(get_class_object);
    // The request's own worker is gone; its successor waits for the next request.
    wait_for("one idle worker", || worker_children() == 1);

    // SAFETY: the export takes no arguments.
    let hr = unsafe { can_unload() };
    assert_eq!(hr, S_OK, "no object is alive, so the DLL may unload");
    assert_eq!(
        worker_children(),
        0,
        "idle worker outlived the unload check"
    );
}
