//! The installer-registered and cloud classes, exercised like the OS does: LoadLibrary,
//! DllGetClassObject, IClassFactory, then each class's main method on a fixture. Kept apart
//! from `com_server.rs` because these tests write the staged install directory's settings
//! file and so need an install directory of their own.
#![cfg(windows)]

use std::ffi::c_void;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use image::{ImageFormat, Rgba, RgbaImage};
use windows::Win32::Foundation::{
    COLORREF, E_FAIL, E_NOINTERFACE, E_NOTIMPL, HMODULE, HWND, LPARAM, RECT, STG_E_ACCESSDENIED,
    WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetPixel, HGDIOBJ,
    ReleaseDC, SelectObject,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_OFFLINE, GetFileAttributesW, SetFileAttributesW,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, IClassFactory, IStream, STGM_READ,
    STGM_READWRITE,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Ole::{IObjectWithSite, IOleWindow};
use windows::Win32::UI::Shell::PropertiesSystem::{
    IInitializeWithFile, IInitializeWithStream, IPropertyStore,
};
use windows::Win32::UI::Shell::{
    ECF_HASSUBCOMMANDS, ECF_ISSEPARATOR, ECS_ENABLED, ECS_HIDDEN, IExplorerCommand,
    IInitializeWithItem, IPreviewHandler, IShellItem, IShellItemArray, IThumbnailProvider,
    SHCreateItemFromParsingName, SHCreateMemStream, SHCreateShellItemArrayFromShellItem,
    SHCreateStreamOnFileEx, WTS_ALPHATYPE, WTSAT_ARGB,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, IsWindow, MSG, PM_REMOVE, PeekMessageW,
    SendMessageW, WM_PRINTCLIENT, WS_POPUP,
};
use windows::core::{GUID, HRESULT, HSTRING, Interface, PCWSTR, PWSTR, w};

use tw_core::ids;

type GetClassObject =
    unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;

const PRF_CLIENT: usize = 0x4;

/// Stages the shipped layout from Cargo's outputs: the cdylib from `deps/`, the worker, and
/// a copy of the worker standing in for `twctl.exe` (so `Invoke` has something to spawn).
fn install_dir() -> PathBuf {
    let worker = Path::new(env!("CARGO_BIN_EXE_tw-worker"));
    let target = worker.parent().unwrap();
    for entry in std::fs::read_dir(target).unwrap().flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with("tw-classes-install-")
        {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    let dir = target.join(format!("tw-classes-install-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dll = target.join("deps").join(ids::SHELL_DLL_NAME);
    assert!(dll.is_file(), "{} missing", dll.display());
    std::fs::copy(&dll, dir.join(ids::SHELL_DLL_NAME)).unwrap();
    std::fs::copy(worker, dir.join(ids::WORKER_EXE_NAME)).unwrap();
    std::fs::copy(worker, dir.join(ids::CTL_EXE_NAME)).unwrap();
    dir
}

static INSTALL: LazyLock<PathBuf> = LazyLock::new(install_dir);

static DLL: LazyLock<usize> = LazyLock::new(|| {
    let path = INSTALL.join(ids::SHELL_DLL_NAME);
    // SAFETY: loading our own DLL by absolute path.
    let module: HMODULE = unsafe { LoadLibraryW(&HSTRING::from(path.as_os_str())).unwrap() };
    module.0 as usize
});

/// Serialises tests that change the staged settings file, and removes it afterwards.
static SETTINGS: Mutex<()> = Mutex::new(());

struct Settings<'a>(#[allow(dead_code)] MutexGuard<'a, ()>);

impl Settings<'_> {
    fn path() -> PathBuf {
        INSTALL.join(tw_core::config::PORTABLE_INI)
    }

    /// Installs `[Settings]` lines for the duration of the guard.
    fn set(body: &str) -> Settings<'static> {
        let guard = SETTINGS.lock().unwrap_or_else(PoisonError::into_inner);
        std::fs::write(Self::path(), format!("[Settings]\n{body}\n")).unwrap();
        Settings(guard)
    }
}

impl Drop for Settings<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(Self::path());
    }
}

fn factory(clsid: u128) -> IClassFactory {
    let clsid = GUID::from_u128(clsid);
    // SAFETY: the export has the DllGetClassObject signature; COM is initialised for this
    // test thread; the out-pointer is owned by the returned wrapper.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let p = GetProcAddress(
            HMODULE(*DLL as *mut _),
            windows::core::s!("DllGetClassObject"),
        )
        .unwrap();
        let get = std::mem::transmute::<unsafe extern "system" fn() -> isize, GetClassObject>(p);
        let mut raw = std::ptr::null_mut();
        get(&clsid, &IClassFactory::IID, &mut raw).ok().unwrap();
        IClassFactory::from_raw(raw)
    }
}

fn create<T: Interface>(class: ids::ClassId) -> T {
    // SAFETY: standard factory call.
    unsafe { factory(class.guid).CreateInstance(None).unwrap() }
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let d = std::env::temp_dir().join(format!("tw-cls-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        Self(d)
    }
    fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let p = self.0.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn png_bytes(img: &RgbaImage) -> Vec<u8> {
    let mut v = Vec::new();
    img.write_to(&mut Cursor::new(&mut v), ImageFormat::Png)
        .unwrap();
    v
}

fn red_png(w: u32, h: u32) -> Vec<u8> {
    png_bytes(&RgbaImage::from_pixel(w, h, Rgba([255, 0, 0, 255])))
}

fn file_stream(path: &Path) -> IStream {
    // SAFETY: opening a file we created, read-only; the stream reports the file name.
    unsafe {
        SHCreateStreamOnFileEx(
            &HSTRING::from(path.as_os_str()),
            STGM_READ.0,
            0,
            false,
            None,
        )
        .unwrap()
    }
}

fn shell_item(path: &Path) -> IShellItem {
    // SAFETY: COM is initialised for this test thread first; parsing an absolute path of a
    // file that exists.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        SHCreateItemFromParsingName(&HSTRING::from(path.as_os_str()), None).unwrap()
    }
}

fn lpwstr_to_string(p: PWSTR) -> String {
    // SAFETY: the callee returned a NUL-terminated CoTaskMem string we free here.
    unsafe {
        let s = p.to_string().unwrap();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

// ---------------------------------------------------------------- cloud-folder class

fn cloud(path: &Path) -> IThumbnailProvider {
    let p: IThumbnailProvider = create(ids::CLSID_CLOUD_THUMBNAIL);
    // SAFETY: plain COM call.
    unsafe {
        p.cast::<IInitializeWithItem>()
            .unwrap()
            .Initialize(&shell_item(path), STGM_READ.0)
            .unwrap();
    }
    p
}

fn cloud_thumbnail(path: &Path) -> windows::core::Result<(i32, i32)> {
    use windows::Win32::Graphics::Gdi::{BITMAP, GetObjectW, HBITMAP};
    let p = cloud(path);
    let mut hbmp = HBITMAP::default();
    let mut alpha = WTS_ALPHATYPE::default();
    // SAFETY: plain COM call; the bitmap is inspected and deleted here.
    unsafe {
        p.GetThumbnail(256, &mut hbmp, &mut alpha)?;
        assert_eq!(alpha, WTSAT_ARGB);
        let mut bm = BITMAP::default();
        GetObjectW(
            HGDIOBJ(hbmp.0),
            size_of::<BITMAP>() as i32,
            Some((&raw mut bm).cast()),
        );
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        Ok((bm.bmWidth, bm.bmHeight))
    }
}

#[test]
fn cloud_class_exposes_only_item_initialisation() {
    let p: IThumbnailProvider = create(ids::CLSID_CLOUD_THUMBNAIL);
    assert!(p.cast::<IInitializeWithItem>().is_ok());
    for (name, r) in [
        (
            "IInitializeWithStream",
            p.cast::<IInitializeWithStream>().map(|_| ()),
        ),
        (
            "IInitializeWithFile",
            p.cast::<IInitializeWithFile>().map(|_| ()),
        ),
    ] {
        assert_eq!(
            r.err().map(|e| e.code()),
            Some(E_NOINTERFACE),
            "{name} (REQ-COM-2)"
        );
    }
}

#[test]
fn cloud_class_is_off_until_the_setting_is_on() {
    let s = Scratch::new("cloud-off");
    let path = s.file("pic.png", &red_png(40, 20));
    {
        let _off = Settings::set("CloudThumbnails=0");
        assert_eq!(
            cloud_thumbnail(&path).err().map(|e| e.code()),
            Some(E_FAIL),
            "off by default (spec 4.3)"
        );
    }
    let _on = Settings::set("CloudThumbnails=1");
    assert_eq!(cloud_thumbnail(&path).unwrap(), (40, 20));
}

#[test]
fn cloud_class_refuses_a_file_marked_offline() {
    let s = Scratch::new("cloud-offline");
    let path = s.file("pic.png", &red_png(16, 16));
    let _on = Settings::set("CloudThumbnails=1");
    let wide = HSTRING::from(path.as_os_str());
    assert_eq!(
        cloud_thumbnail(&path).unwrap(),
        (16, 16),
        "local file works"
    );

    // SAFETY: attribute changes on a file we own; restored before the scratch dir goes.
    unsafe {
        SetFileAttributesW(&wide, FILE_ATTRIBUTE_OFFLINE).unwrap();
        assert_ne!(
            GetFileAttributesW(&wide) & FILE_ATTRIBUTE_OFFLINE.0,
            0,
            "this filesystem keeps the OFFLINE attribute"
        );
    }
    assert_eq!(
        cloud_thumbnail(&path).err().map(|e| e.code()),
        Some(E_FAIL),
        "REQ-ISO-5: an online-only file is never read"
    );
    // SAFETY: as above.
    unsafe { SetFileAttributesW(&wide, FILE_ATTRIBUTE_NORMAL).unwrap() };
    assert_eq!(cloud_thumbnail(&path).unwrap(), (16, 16));
}

// ---------------------------------------------------------------- property handler

const FMT_IMAGE: u128 = 0x6444048F_4C8B_11D1_8B70_080036B11A03;
const FMT_PHOTO: u128 = 0x14B81DA1_0135_4D31_96D9_6CBFC9671A99;

fn pkey(fmt: u128, pid: u32) -> windows::Win32::Foundation::PROPERTYKEY {
    windows::Win32::Foundation::PROPERTYKEY {
        fmtid: GUID::from_u128(fmt),
        pid,
    }
}

fn property_store(bytes: &[u8], name_hint: &Scratch, file: &str) -> IPropertyStore {
    let path = name_hint.file(file, bytes);
    let store: IPropertyStore = create(ids::CLSID_PROPERTY);
    // SAFETY: plain COM call.
    unsafe {
        store
            .cast::<IInitializeWithStream>()
            .unwrap()
            .Initialize(&file_stream(&path), STGM_READ.0)
            .unwrap();
    }
    store
}

fn value(store: &IPropertyStore, key: windows::Win32::Foundation::PROPERTYKEY) -> PROPVARIANT {
    // SAFETY: plain COM call with a valid key.
    unsafe { store.GetValue(&key).unwrap() }
}

/// A PNG carrying an `eXIf` chunk with Make = "Acme" and ISO 400.
fn png_with_exif() -> Vec<u8> {
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    let entry = |tag: u16, ty: u16, count: u32, val: u32| {
        let mut e = Vec::new();
        e.extend_from_slice(&tag.to_le_bytes());
        e.extend_from_slice(&ty.to_le_bytes());
        e.extend_from_slice(&count.to_le_bytes());
        e.extend_from_slice(&val.to_le_bytes());
        e
    };
    // IFD0 at 8: Make (ascii, at 38) and the Exif IFD pointer (at 44); Exif IFD: ISO.
    let mut exif = b"II*\0".to_vec();
    exif.extend_from_slice(&8u32.to_le_bytes());
    exif.extend_from_slice(&2u16.to_le_bytes());
    exif.extend(entry(0x010F, 2, 5, 38));
    exif.extend(entry(0x8769, 4, 1, 44));
    exif.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(exif.len(), 38);
    exif.extend_from_slice(b"Acme\0\0");
    exif.extend_from_slice(&1u16.to_le_bytes());
    exif.extend(entry(0x8827, 3, 1, 400));
    exif.extend_from_slice(&0u32.to_le_bytes());

    let plain = red_png(30, 10);
    let split = 8 + 25; // signature + IHDR chunk
    let mut chunk = (exif.len() as u32).to_be_bytes().to_vec();
    let mut body = b"eXIf".to_vec();
    body.extend_from_slice(&exif);
    chunk.extend_from_slice(&body);
    chunk.extend_from_slice(&crc32(&body).to_be_bytes());
    let mut out = plain[..split].to_vec();
    out.extend_from_slice(&chunk);
    out.extend_from_slice(&plain[split..]);
    out
}

#[test]
fn property_store_reports_dimensions_and_bit_depth() {
    let _settings = Settings::set("");
    let s = Scratch::new("prop");
    let store = property_store(&red_png(64, 32), &s, "pic.png");
    // SAFETY: plain COM calls.
    unsafe {
        assert!(store.GetCount().unwrap() >= 4);
        assert_eq!(
            u32::try_from(&value(&store, pkey(FMT_IMAGE, 3))).unwrap(),
            64,
            "System.Image.HorizontalSize"
        );
        assert_eq!(
            u32::try_from(&value(&store, pkey(FMT_IMAGE, 4))).unwrap(),
            32,
            "System.Image.VerticalSize"
        );
        assert_eq!(
            u32::try_from(&value(&store, pkey(FMT_IMAGE, 7))).unwrap(),
            32,
            "System.Image.BitDepth of an RGBA8 PNG"
        );
        assert_eq!(value(&store, pkey(FMT_IMAGE, 13)).to_string(), "64 x 32");

        // Every enumerated key resolves to a non-empty value.
        for i in 0..store.GetCount().unwrap() {
            let mut k = Default::default();
            store.GetAt(i, &mut k).unwrap();
            assert!(!value(&store, k).is_empty(), "key #{i}");
        }
        // An unknown property is S_OK and empty, not an error.
        assert!(value(&store, pkey(0x1234, 1)).is_empty());
    }
}

#[test]
fn property_store_exposes_exif_values() {
    let _settings = Settings::set("");
    let s = Scratch::new("prop-exif");
    let store = property_store(&png_with_exif(), &s, "photo.png");
    assert_eq!(value(&store, pkey(FMT_PHOTO, 271)).to_string(), "Acme");
    assert_eq!(
        u16::try_from(&value(&store, pkey(FMT_PHOTO, 34855))).unwrap(),
        400,
        "System.Photo.ISOSpeed"
    );
    assert_eq!(
        u32::try_from(&value(&store, pkey(FMT_IMAGE, 3))).unwrap(),
        30
    );
}

#[test]
fn property_store_is_read_only_and_survives_garbage() {
    let _settings = Settings::set("");
    let s = Scratch::new("prop-ro");
    let store = property_store(&red_png(8, 8), &s, "pic.png");
    let key = pkey(FMT_IMAGE, 3);
    let v = PROPVARIANT::from(1u32);
    // SAFETY: plain COM calls.
    unsafe {
        assert_eq!(
            store.SetValue(&key, &v).err().map(|e| e.code()),
            Some(STG_E_ACCESSDENIED)
        );
        assert_eq!(
            store.Commit().err().map(|e| e.code()),
            Some(STG_E_ACCESSDENIED)
        );
        let rw: IPropertyStore = create(ids::CLSID_PROPERTY);
        let init = rw.cast::<IInitializeWithStream>().unwrap();
        assert_eq!(
            init.Initialize(&mem_stream(b"x"), STGM_READWRITE.0)
                .err()
                .map(|e| e.code()),
            Some(STG_E_ACCESSDENIED),
            "a write request cannot be honoured"
        );
    }
    let junk = property_store(&[0xAB; 4096], &s, "noise.png");
    // SAFETY: plain COM call.
    assert_eq!(unsafe { junk.GetCount() }.unwrap(), 0);
}

fn mem_stream(bytes: &[u8]) -> IStream {
    // SAFETY: SHCreateMemStream copies the buffer.
    unsafe { SHCreateMemStream(Some(bytes)).unwrap() }
}

// ---------------------------------------------------------------- preview handler

fn hidden_parent() -> HWND {
    // SAFETY: a never-shown popup of the predefined STATIC class.
    unsafe {
        CreateWindowExW(
            Default::default(),
            w!("STATIC"),
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            300,
            200,
            None,
            None,
            None,
            None,
        )
        .unwrap()
    }
}

fn pump() {
    let mut msg = MSG::default();
    // SAFETY: draining this thread's queue.
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = DispatchMessageW(&msg);
        }
    }
}

/// Colour of the pixel at (`x`, `y`) of the preview window as it paints itself.
fn preview_pixel(window: HWND, size: (i32, i32), x: i32, y: i32) -> COLORREF {
    // SAFETY: WM_PRINTCLIENT into a memory DC we create and free here.
    unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, size.0, size.1);
        let old = SelectObject(dc, HGDIOBJ(bmp.0));
        SendMessageW(
            window,
            WM_PRINTCLIENT,
            Some(WPARAM(dc.0 as usize)),
            Some(LPARAM(PRF_CLIENT as isize)),
        );
        let px = GetPixel(dc, x, y);
        SelectObject(dc, old);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
        px
    }
}

#[test]
fn preview_handler_renders_the_image_scaled_into_its_window() {
    let _settings = Settings::set("MaxThumbPx=256");
    let s = Scratch::new("preview");
    let path = s.file("pic.png", &red_png(64, 32));
    let handler: IPreviewHandler = create(ids::CLSID_PREVIEW);
    let parent = hidden_parent();
    let rect = RECT {
        left: 0,
        top: 0,
        right: 200,
        bottom: 100,
    };
    // SAFETY: plain COM calls against windows and objects this test owns.
    unsafe {
        handler
            .cast::<IInitializeWithStream>()
            .unwrap()
            .Initialize(&file_stream(&path), STGM_READ.0)
            .unwrap();
        handler.SetWindow(parent, &rect).unwrap();
        handler.DoPreview().unwrap();
        let child = handler.cast::<IOleWindow>().unwrap().GetWindow().unwrap();
        assert!(IsWindow(Some(child)).as_bool());

        // The decode runs on a background thread: poll the painted result.
        let deadline = Instant::now() + Duration::from_secs(30);
        let centre = loop {
            pump();
            let px = preview_pixel(child, (200, 100), 100, 50);
            if px == COLORREF(0x0000FF) || Instant::now() > deadline {
                break px;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(centre, COLORREF(0x0000FF), "centre pixel is the red image");
        // A 64x32 image is never enlarged: the window corner stays background.
        assert_ne!(preview_pixel(child, (200, 100), 2, 2), COLORREF(0x0000FF));

        handler.SetRect(&RECT { right: 100, ..rect }).unwrap();
        handler.Unload().unwrap();
        assert!(
            !IsWindow(Some(child)).as_bool(),
            "Unload destroys the window"
        );
        let _ = DestroyWindow(parent);
    }
}

#[test]
fn preview_handler_shows_no_preview_for_undecodable_input() {
    let _settings = Settings::set("");
    let s = Scratch::new("preview-bad");
    let path = s.file("noise.png", &[0x5A; 2048]);
    let handler: IPreviewHandler = create(ids::CLSID_PREVIEW);
    let parent = hidden_parent();
    let rect = RECT {
        left: 0,
        top: 0,
        right: 200,
        bottom: 100,
    };
    // SAFETY: as above.
    unsafe {
        handler
            .cast::<IInitializeWithStream>()
            .unwrap()
            .Initialize(&file_stream(&path), STGM_READ.0)
            .unwrap();
        handler.SetWindow(parent, &rect).unwrap();
        handler.DoPreview().unwrap();
        let child = handler.cast::<IOleWindow>().unwrap().GetWindow().unwrap();
        std::thread::sleep(Duration::from_millis(500));
        pump();
        // Paints without crashing; no image pixel can appear.
        assert_ne!(
            preview_pixel(child, (200, 100), 100, 50),
            COLORREF(0x0000FF)
        );
        handler.Unload().unwrap();
        let _ = DestroyWindow(parent);
    }
}

#[test]
fn preview_handler_refuses_a_disabled_type_before_creating_a_window() {
    let _settings = Settings::set("DisabledTypes=png");
    let s = Scratch::new("preview-off");
    let path = s.file("pic.png", &red_png(8, 8));
    let handler: IPreviewHandler = create(ids::CLSID_PREVIEW);
    let parent = hidden_parent();
    let rect = RECT {
        left: 0,
        top: 0,
        right: 50,
        bottom: 50,
    };
    // SAFETY: as above.
    unsafe {
        handler
            .cast::<IInitializeWithStream>()
            .unwrap()
            .Initialize(&file_stream(&path), STGM_READ.0)
            .unwrap();
        handler.SetWindow(parent, &rect).unwrap();
        assert_eq!(handler.DoPreview().err().map(|e| e.code()), Some(E_FAIL));
        assert!(handler.cast::<IOleWindow>().unwrap().GetWindow().is_err());
        let _ = DestroyWindow(parent);
    }
}

#[test]
fn preview_handler_site_round_trips() {
    let handler: IPreviewHandler = create(ids::CLSID_PREVIEW);
    let site = handler.cast::<IObjectWithSite>().unwrap();
    let marker = mem_stream(b"site");
    // SAFETY: plain COM calls.
    unsafe {
        assert!(site.GetSite::<IStream>().is_err(), "no site yet");
        site.SetSite(&marker).unwrap();
        let back = site.GetSite::<IStream>().unwrap();
        assert_eq!(back.as_raw(), marker.as_raw());
        site.SetSite(None::<&windows::core::IUnknown>).unwrap();
        assert!(site.GetSite::<IStream>().is_err());
    }
}

// ---------------------------------------------------------------- modern menu

fn items_for(path: &Path) -> IShellItemArray {
    // SAFETY: wrapping one shell item.
    unsafe { SHCreateShellItemArrayFromShellItem(&shell_item(path)).unwrap() }
}

fn title(cmd: &IExplorerCommand, items: &IShellItemArray) -> String {
    // SAFETY: plain COM call.
    lpwstr_to_string(unsafe { cmd.GetTitle(items).unwrap() })
}

fn children(cmd: &IExplorerCommand) -> Vec<IExplorerCommand> {
    // SAFETY: plain COM calls on the enumerator.
    unsafe {
        let e = cmd.EnumSubCommands().unwrap();
        let mut out = Vec::new();
        loop {
            let mut slot = [None];
            let mut fetched = 0u32;
            let hr = e.Next(&mut slot, Some(&mut fetched));
            if fetched == 0 {
                assert_eq!(hr, windows::Win32::Foundation::S_FALSE);
                return out;
            }
            out.extend(slot[0].take());
        }
    }
}

#[test]
fn explorer_command_enumerates_the_menu_tree_for_a_png() {
    let _settings = Settings::set("");
    let s = Scratch::new("xcmd");
    let png = s.file("pic.png", &red_png(8, 8));
    let items = items_for(&png);
    let root: IExplorerCommand = create(ids::CLSID_EXPLORER_COMMAND);
    // SAFETY: plain COM calls.
    unsafe {
        assert_eq!(title(&root, &items), ids::PRODUCT_NAME);
        assert_eq!(root.GetFlags().unwrap() as i32 & ECF_HASSUBCOMMANDS.0, 1);
        assert_eq!(root.GetState(&items, false).unwrap(), ECS_ENABLED.0 as u32);
        assert_eq!(
            root.GetCanonicalName().unwrap(),
            GUID::from_u128(ids::CLSID_EXPLORER_COMMAND.guid)
        );
        assert_eq!(
            root.GetIcon(&items).err().map(|e| e.code()),
            Some(E_NOTIMPL)
        );
    }

    let subs = children(&root);
    // SAFETY: plain COM calls.
    let shown: Vec<_> = subs
        .iter()
        .filter(|c| unsafe { c.GetState(&items, false) }.unwrap() == ECS_ENABLED.0 as u32)
        .collect();
    let titles: Vec<String> = shown
        .iter()
        .map(|c| {
            // SAFETY: plain COM call.
            if unsafe { c.GetFlags() }.unwrap() as i32 & ECF_ISSEPARATOR.0 != 0 {
                "-".to_owned()
            } else {
                title(c, &items)
            }
        })
        .collect();
    assert_eq!(
        titles,
        [
            "Convert into",
            "Resize",
            "Rotate / flip",
            "-",
            "Shrink for email",
            "Compress to size",
            "Strip metadata (EXIF/GPS)",
            "-",
            "Copy to clipboard",
            "Copy as data URI",
            "-",
            "Files to folder",
            "-",
            "Image info",
            "-",
            "Set as wallpaper",
        ]
    );

    // A submenu enumerates its verbs in order and its leaves are not themselves menus.
    let convert = shown[0];
    let formats: Vec<String> = children(convert).iter().map(|c| title(c, &items)).collect();
    assert_eq!(
        formats,
        [
            "PNG",
            "JPG",
            "WebP (lossless)",
            "BMP",
            "GIF",
            "TIFF",
            "Icon (.ico)"
        ]
    );
    // SAFETY: plain COM calls.
    unsafe {
        let png_cmd = &children(convert)[0];
        assert_eq!(png_cmd.GetFlags().unwrap() as i32 & ECF_HASSUBCOMMANDS.0, 0);
        assert_eq!(
            png_cmd.EnumSubCommands().err().map(|e| e.code()),
            Some(E_NOTIMPL)
        );
        // Invoke launches `twctl.exe` from the install directory (a stand-in here).
        png_cmd.Invoke(&items, None).unwrap();
        assert_eq!(
            convert.Invoke(&items, None).err().map(|e| e.code()),
            Some(E_NOTIMPL),
            "a submenu is not invokable"
        );
    }
}

#[test]
fn explorer_command_hides_for_unsupported_files() {
    let _settings = Settings::set("");
    let s = Scratch::new("xcmd-exe");
    let exe = s.file("tool.exe", b"MZ");
    let items = items_for(&exe);
    let root: IExplorerCommand = create(ids::CLSID_EXPLORER_COMMAND);
    // SAFETY: plain COM call.
    assert_eq!(
        unsafe { root.GetState(&items, false) }.unwrap(),
        ECS_HIDDEN.0 as u32
    );
}

#[test]
fn every_new_class_is_creatable() {
    drop(create::<IThumbnailProvider>(ids::CLSID_CLOUD_THUMBNAIL));
    drop(create::<IPreviewHandler>(ids::CLSID_PREVIEW));
    drop(create::<IPropertyStore>(ids::CLSID_PROPERTY));
    drop(create::<IExplorerCommand>(ids::CLSID_EXPLORER_COMMAND));
}
