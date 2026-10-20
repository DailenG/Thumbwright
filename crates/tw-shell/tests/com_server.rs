//! Exercises the built `thumbwright_shell.dll` exactly as the OS does: LoadLibrary,
//! DllGetClassObject, IClassFactory, IInitializeWithStream / IShellExtInit, then the
//! IThumbnailProvider / IContextMenu calls. The worker (`tw-worker.exe`) is built into the
//! same directory by Cargo, mirroring the shipped layout.
#![cfg(windows)]

use std::ffi::c_void;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use windows::Win32::Foundation::{CLASS_E_CLASSNOTAVAILABLE, E_FAIL, E_NOINTERFACE, HMODULE};
use windows::Win32::Graphics::Gdi::{DIBSECTION, DeleteObject, GetObjectW, HBITMAP, HGDIOBJ};
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, CoInitializeEx, IClassFactory, IDataObject, IStream, STGM_READ,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Shell::PropertiesSystem::{IInitializeWithFile, IInitializeWithStream};
use windows::Win32::UI::Shell::{
    BHID_DataObject, CMF_NORMAL, GCS_VERBW, IContextMenu, IContextMenu2, IContextMenu3,
    IExtractImage, IInitializeWithItem, IShellExtInit, IShellItem, IThumbnailProvider,
    SHCreateItemFromParsingName, SHCreateMemStream, SHCreateStreamOnFileEx, WTS_ALPHATYPE,
    WTSAT_ARGB,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DestroyMenu, GetMenuItemCount, GetMenuItemInfoW, GetMenuStringW, GetSubMenu,
    MENUITEMINFOW, MF_BYPOSITION, MFT_BITMAP, MFT_OWNERDRAW, MIIM_BITMAP, MIIM_FTYPE, MIIM_TYPE,
    WM_INITMENUPOPUP,
};
use windows::core::{GUID, HRESULT, HSTRING, Interface, PSTR};

const CLSID_THUMBNAIL: GUID = GUID::from_u128(tw_core::ids::CLSID_THUMBNAIL.guid);
const CLSID_MENU: GUID = GUID::from_u128(tw_core::ids::CLSID_CONTEXT_MENU.guid);

type GetClassObject =
    unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;

/// Stages the shipped layout (DLL + worker side by side) from Cargo's build outputs:
/// `cargo test` leaves the cdylib in `deps/` and the worker as `CARGO_BIN_EXE_tw-worker`.
fn install_dir() -> PathBuf {
    let worker = Path::new(env!("CARGO_BIN_EXE_tw-worker"));
    let target = worker.parent().unwrap();
    // Earlier runs' staging dirs; ones still loaded by a live test process fail to delete
    // and are left alone.
    for entry in std::fs::read_dir(target).unwrap().flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with("tw-test-install-")
        {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    let dir = target.join(format!("tw-test-install-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dll = target.join("deps").join(tw_core::ids::SHELL_DLL_NAME);
    assert!(dll.is_file(), "{} missing", dll.display());
    std::fs::copy(&dll, dir.join(tw_core::ids::SHELL_DLL_NAME)).unwrap();
    std::fs::copy(worker, dir.join(tw_core::ids::WORKER_EXE_NAME)).unwrap();
    dir
}

/// The staged install directory, created once per test process.
static INSTALL: LazyLock<PathBuf> = LazyLock::new(install_dir);

/// Loaded once per test process, like the surrogate does.
static DLL: LazyLock<usize> = LazyLock::new(|| {
    // On a Windows 11 PC with the package registered the classic menu deliberately adds
    // nothing (the packaged entry is the menu); these tests exercise the classic one.
    // SAFETY: set before the first menu object exists; Windows environment access is
    // serialised by the OS.
    unsafe { std::env::set_var(tw_core::menu::FORCE_CLASSIC_ENV, "1") };
    let path = INSTALL.join(tw_core::ids::SHELL_DLL_NAME);
    // SAFETY: loading our own DLL by absolute path.
    let module: HMODULE = unsafe { LoadLibraryW(&HSTRING::from(path.as_os_str())).unwrap() };
    module.0 as usize
});

/// Launches the staged worker once. Antivirus scans a freshly copied executable on its
/// first launch (seconds on a busy machine), which is not part of any decode budget.
fn warm_worker() {
    let worker = INSTALL.join(tw_core::ids::WORKER_EXE_NAME);
    let ok = std::process::Command::new(worker).arg("--version").output();
    assert!(ok.is_ok_and(|o| o.status.success()), "staged worker runs");
}

fn get_class_object() -> GetClassObject {
    // SAFETY: the export has the DllGetClassObject signature.
    unsafe {
        let p = GetProcAddress(
            HMODULE(*DLL as *mut _),
            windows::core::s!("DllGetClassObject"),
        )
        .unwrap();
        std::mem::transmute::<unsafe extern "system" fn() -> isize, GetClassObject>(p)
    }
}

fn factory(clsid: &GUID) -> windows::core::Result<IClassFactory> {
    // SAFETY: COM initialised per test thread; out-pointer owned by the returned wrapper.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let mut raw = std::ptr::null_mut();
        get_class_object()(clsid, &IClassFactory::IID, &mut raw).ok()?;
        Ok(IClassFactory::from_raw(raw))
    }
}

fn provider() -> IThumbnailProvider {
    // SAFETY: standard factory call.
    unsafe {
        factory(&CLSID_THUMBNAIL)
            .unwrap()
            .CreateInstance(None)
            .unwrap()
    }
}

fn png(img: &RgbaImage) -> Vec<u8> {
    let mut v = Vec::new();
    img.write_to(&mut Cursor::new(&mut v), ImageFormat::Png)
        .unwrap();
    v
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let d = std::env::temp_dir().join(format!("tw-com-{tag}-{}", std::process::id()));
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

/// A named file stream, as the thumbnail cache provides (Stat reports the file name).
fn file_stream(path: &Path) -> IStream {
    // SAFETY: opening a file we created for reading.
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

struct Bitmap {
    width: i32,
    height: i32,
    bpp: u16,
    /// The section's memory, row 0 first. Top-down orientation (REQ-COM-6) is asserted
    /// by tests that place a marker pixel near the top of the source image.
    pixels: Vec<u8>,
}

/// Runs GetThumbnail and decodes the resulting HBITMAP through GDI.
fn thumbnail(stream: &IStream, cx: u32) -> windows::core::Result<Bitmap> {
    let p = provider();
    // SAFETY: plain COM calls; the bitmap is inspected and deleted here.
    unsafe {
        p.cast::<IInitializeWithStream>()?
            .Initialize(stream, STGM_READ.0)?;
        let mut hbmp = HBITMAP::default();
        let mut alpha = WTS_ALPHATYPE::default();
        p.GetThumbnail(cx, &mut hbmp, &mut alpha)?;
        assert_eq!(alpha, WTSAT_ARGB);
        let mut ds = DIBSECTION::default();
        let n = GetObjectW(
            HGDIOBJ(hbmp.0),
            size_of::<DIBSECTION>() as i32,
            Some((&raw mut ds).cast()),
        );
        assert_eq!(
            n as usize,
            size_of::<DIBSECTION>(),
            "HBITMAP must be a DIB section"
        );
        let (w, h) = (ds.dsBm.bmWidth, ds.dsBm.bmHeight);
        let len = (w * h * 4) as usize;
        let pixels = std::slice::from_raw_parts(ds.dsBm.bmBits.cast::<u8>(), len).to_vec();
        let out = Bitmap {
            width: w,
            height: h,
            bpp: ds.dsBm.bmBitsPixel,
            pixels,
        };
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        Ok(out)
    }
}

impl Bitmap {
    fn px(&self, x: i32, y: i32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        self.pixels[i..i + 4].try_into().unwrap()
    }
    fn assert_argb(&self) {
        assert_eq!(self.bpp, 32);
    }
}

fn mem_stream(bytes: &[u8]) -> IStream {
    // SAFETY: SHCreateMemStream copies the buffer.
    unsafe { SHCreateMemStream(Some(bytes)).unwrap() }
}

#[test]
fn png_produces_top_down_premultiplied_bgra() {
    let mut img = RgbaImage::from_pixel(64, 32, Rgba([255, 0, 0, 255]));
    img.put_pixel(1, 1, Rgba([0, 0, 255, 128]));
    let bmp = thumbnail(&mem_stream(&png(&img)), 256).unwrap();
    bmp.assert_argb();
    assert_eq!((bmp.width, bmp.height), (64, 32), "never enlarged");
    assert_eq!(bmp.px(0, 0), [0, 0, 255, 255], "opaque red as BGRA");
    assert_eq!(
        bmp.px(1, 1),
        [128, 0, 0, 128],
        "half-transparent blue, premultiplied, in memory row 1 (top-down)"
    );
    assert_eq!(bmp.px(1, 30), [0, 0, 255, 255]);
}

#[test]
fn large_image_is_shrunk_to_requested_edge() {
    let img = RgbaImage::from_pixel(1000, 500, Rgba([0, 255, 0, 255]));
    let bmp = thumbnail(&mem_stream(&png(&img)), 96).unwrap();
    bmp.assert_argb();
    assert_eq!((bmp.width, bmp.height), (96, 48));
    assert_eq!(bmp.px(48, 24), [0, 255, 0, 255]);
}

#[test]
fn named_jpeg_file_stream() {
    let s = Scratch::new("jpg");
    let mut jpg = Vec::new();
    DynamicImage::ImageRgba8(RgbaImage::from_pixel(40, 30, Rgba([0, 0, 255, 255])))
        .to_rgb8()
        .write_to(&mut Cursor::new(&mut jpg), ImageFormat::Jpeg)
        .unwrap();
    let path = s.file("photo.jpg", &jpg);
    let bmp = thumbnail(&file_stream(&path), 256).unwrap();
    bmp.assert_argb();
    assert_eq!((bmp.width, bmp.height), (40, 30));
    let [b, g, r, a] = bmp.px(20, 15);
    assert!(
        b > 200 && g < 40 && r < 40 && a == 255,
        "blue-ish: {:?}",
        [b, g, r, a]
    );
}

#[test]
fn svg_renders_in_worker_at_requested_edge() {
    let s = Scratch::new("svg");
    let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 200 100" width="200" height="100">
        <rect width="200" height="100" fill="#00ff00"/></svg>"##;
    let path = s.file("drawing.svg", svg);
    let bmp = thumbnail(&file_stream(&path), 96).unwrap();
    bmp.assert_argb();
    assert_eq!((bmp.width, bmp.height), (96, 48));
    assert_eq!(bmp.px(48, 24), [0, 255, 0, 255]);
}

#[test]
fn hostile_inputs_fail_cleanly_with_e_fail() {
    let s = Scratch::new("hostile");
    warm_worker();
    let mut noise = vec![0u8; 64 * 1024];
    let mut x = 0x9E37_79B9u32;
    for b in &mut noise {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        *b = x as u8;
    }
    for name in [
        "noise.png",
        "noise.psd",
        "noise.tga",
        "noise.dds",
        "noise.svg",
        "noise.jxl",
        "empty.psd",
    ] {
        let bytes = if name.starts_with("empty") {
            &[][..]
        } else {
            &noise[..]
        };
        let path = s.file(name, bytes);
        let start = Instant::now();
        let err = thumbnail(&file_stream(&path), 256)
            .err()
            .unwrap_or_else(|| panic!("{name} produced a thumbnail"));
        assert_eq!(err.code(), E_FAIL, "{name}: REQ-COM-7 failure HRESULT");
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "{name} took {:?}",
            start.elapsed()
        );
    }
}

#[test]
fn decompression_bomb_rejected_quickly() {
    // 33-byte PNG declaring 65535 x 65535.
    let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    v.extend_from_slice(&65535u32.to_be_bytes());
    v.extend_from_slice(&65535u32.to_be_bytes());
    v.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(v.len(), 33);
    // Load the DLL first: its one-time load (and any AV scan of the fresh copy) is not
    // part of the decode budget being measured.
    drop(provider());
    let start = Instant::now();
    assert_eq!(
        thumbnail(&mem_stream(&v), 256).err().map(|e| e.code()),
        Some(E_FAIL)
    );
    assert!(
        start.elapsed() < Duration::from_millis(1200),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn truncated_png_yields_bitmap_or_failure_never_garbage() {
    let img = RgbaImage::from_fn(300, 300, |x, y| Rgba([x as u8, y as u8, 7, 255]));
    let data = png(&img);
    match thumbnail(&mem_stream(&data[..200]), 256) {
        Ok(bmp) => {
            bmp.assert_argb();
            assert!(bmp.width > 0 && bmp.width <= 256 && bmp.height <= 256);
        }
        Err(e) => assert_eq!(e.code(), E_FAIL),
    }
}

#[test]
fn thumbnail_class_exposes_only_stream_initialisation() {
    let p = provider();
    assert!(p.cast::<IInitializeWithStream>().is_ok());
    for (name, r) in [
        (
            "IInitializeWithFile",
            p.cast::<IInitializeWithFile>().map(|_| ()),
        ),
        (
            "IInitializeWithItem",
            p.cast::<IInitializeWithItem>().map(|_| ()),
        ),
        ("IExtractImage", p.cast::<IExtractImage>().map(|_| ())),
        ("IShellExtInit", p.cast::<IShellExtInit>().map(|_| ())),
    ] {
        assert_eq!(
            r.err().map(|e| e.code()),
            Some(E_NOINTERFACE),
            "{name} (REQ-COM-1)"
        );
    }
}

#[test]
fn second_initialize_is_rejected() {
    let p = provider();
    let init = p.cast::<IInitializeWithStream>().unwrap();
    // SAFETY: plain COM calls.
    unsafe {
        init.Initialize(&mem_stream(b"a"), STGM_READ.0).unwrap();
        assert!(init.Initialize(&mem_stream(b"b"), STGM_READ.0).is_err());
    }
}

#[test]
fn unknown_clsid_is_not_available() {
    let err = factory(&GUID::from_u128(0x1234)).err().unwrap();
    assert_eq!(err.code(), CLASS_E_CLASSNOTAVAILABLE);
}

fn menu_for(
    path: &Path,
) -> (
    IContextMenu,
    windows::Win32::UI::WindowsAndMessaging::HMENU,
    u32,
) {
    // SAFETY: shell item -> data object -> IShellExtInit, then QueryContextMenu into a
    // fresh popup menu owned by the caller.
    unsafe {
        let menu: IContextMenu = factory(&CLSID_MENU).unwrap().CreateInstance(None).unwrap();
        let item: IShellItem =
            SHCreateItemFromParsingName(&HSTRING::from(path.as_os_str()), None).unwrap();
        let data: IDataObject = item.BindToHandler(None, &BHID_DataObject).unwrap();
        menu.cast::<IShellExtInit>()
            .unwrap()
            .Initialize(None, &data, None)
            .unwrap();
        let hmenu = CreatePopupMenu().unwrap();
        let hr = menu.QueryContextMenu(hmenu, 0, 1, 0x7FFF, CMF_NORMAL);
        assert!(hr.is_ok(), "{hr:?}");
        (menu, hmenu, (hr.0 & 0xFFFF) as u32)
    }
}

fn label(menu: windows::Win32::UI::WindowsAndMessaging::HMENU, pos: i32) -> String {
    let mut buf = [0u16; 128];
    // SAFETY: reading a menu item's text into a local buffer.
    let n = unsafe { GetMenuStringW(menu, pos as u32, Some(&mut buf), MF_BYPOSITION) };
    String::from_utf16_lossy(&buf[..n as usize])
}

/// The classic menu's text of every row of `menu` ("" for separators).
fn rows(menu: windows::Win32::UI::WindowsAndMessaging::HMENU) -> Vec<String> {
    // SAFETY: counting rows of a menu the caller owns.
    let n = unsafe { GetMenuItemCount(Some(menu)) };
    (0..n).map(|i| label(menu, i)).collect()
}

fn item_info(
    menu: windows::Win32::UI::WindowsAndMessaging::HMENU,
    pos: u32,
    mask: windows::Win32::UI::WindowsAndMessaging::MENU_ITEM_MASK,
) -> MENUITEMINFOW {
    let mut info = MENUITEMINFOW {
        cbSize: size_of::<MENUITEMINFOW>() as u32,
        fMask: mask,
        ..Default::default()
    };
    // SAFETY: reading one row of a menu the caller owns.
    unsafe { GetMenuItemInfoW(menu, pos, true, &mut info) }.unwrap();
    info
}

#[test]
fn context_menu_tree_for_single_png() {
    let s = Scratch::new("menu");
    let path = s.file(
        "pic.png",
        &png(&RgbaImage::from_pixel(8, 8, Rgba([1, 2, 3, 255]))),
    );
    let (menu, hmenu, count) = menu_for(&path);
    assert!(count > 30, "command ids used: {count}");
    // SAFETY: inspecting menus we own.
    unsafe {
        assert_eq!(GetMenuItemCount(Some(hmenu)), 1);
        assert_eq!(label(hmenu, 0), tw_core::ids::PRODUCT_NAME);
        let sub = GetSubMenu(hmenu, 0);
        let labels = rows(sub);
        assert!(
            labels[0].starts_with("pic.png  ") && labels[0].ends_with(" bytes"),
            "file name and size header first: {:?}",
            labels[0]
        );
        // Every row is a plain themed item: an owner-drawn one switches the whole menu to
        // the non-themed look (light preview row in dark mode).
        for i in 0..labels.len() as u32 {
            let info = item_info(sub, i, MIIM_FTYPE);
            assert_eq!(info.fType.0 & MFT_OWNERDRAW.0, 0, "row {i} is owner-drawn");
        }
        // UX-REVIEW 3.4 order; separators are the empty labels, '&' is doubled for the menu.
        let named: Vec<&str> = labels[1..]
            .iter()
            .map(String::as_str)
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(
            named,
            [
                "Convert to",
                "Resize",
                "Rotate && flip",
                "Make smaller",
                "Copy text from image",
                "Copy",
                "Organise",
                "Details\u{2026}",
                "Set as",
                "Thumbwright Workbench\u{2026}",
            ]
        );
        assert!(
            !labels
                .iter()
                .any(|l| l.contains("Combine") || l.contains("allpaper")),
            "Combine needs 2+ files and wallpaper is gone: {labels:?}"
        );
        let conv = GetSubMenu(
            sub,
            labels.iter().position(|l| l == "Convert to").unwrap() as i32,
        );
        assert_eq!(
            rows(conv),
            [
                "JPEG",
                "PNG",
                "WebP \u{b7} lossless",
                "AVIF",
                "JPEG XL \u{b7} lossless",
                "More formats"
            ]
        );
        assert_eq!(
            rows(GetSubMenu(conv, 5)),
            ["BMP", "GIF", "TIFF", "Icon (.ico)"]
        );

        // Command offset 1 is the first convert target (offset 0 is the root, which has one).
        let mut buf = [0u16; 64];
        menu.GetCommandString(
            1,
            GCS_VERBW,
            None,
            PSTR(buf.as_mut_ptr().cast()),
            buf.len() as u32,
        )
        .unwrap();
        let verb = String::from_utf16_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap()]);
        assert_eq!(verb, "tw.convert.jpg");
        assert!(menu.cast::<IContextMenu2>().is_ok() && menu.cast::<IContextMenu3>().is_ok());
        let _ = DestroyMenu(hmenu);
    }
}

/// Opening the submenu turns the header row into a bitmap row: the thumbnail with the file's
/// name and size beside it. No row carries an item bitmap, because that would widen the bitmap
/// column and push every row's text right.
#[test]
fn opening_the_submenu_attaches_the_preview_bitmap() {
    let s = Scratch::new("menu-preview");
    let path = s.file(
        "pic.png",
        &png(&RgbaImage::from_pixel(64, 64, Rgba([200, 30, 30, 255]))),
    );
    let (menu, hmenu, _) = menu_for(&path);
    // SAFETY: inspecting and messaging a menu we own.
    unsafe {
        let sub = GetSubMenu(hmenu, 0);
        let rows_before = GetMenuItemCount(Some(sub));
        assert_eq!(item_info(sub, 0, MIIM_FTYPE).fType.0 & MFT_BITMAP.0, 0);
        menu.cast::<IContextMenu2>()
            .unwrap()
            .HandleMenuMsg(
                WM_INITMENUPOPUP,
                windows::Win32::Foundation::WPARAM(sub.0 as usize),
                windows::Win32::Foundation::LPARAM(0),
            )
            .unwrap();
        assert_eq!(GetMenuItemCount(Some(sub)), rows_before, "same rows");
        let header = item_info(sub, 0, MIIM_TYPE);
        assert_eq!(
            header.fType.0 & MFT_BITMAP.0,
            MFT_BITMAP.0,
            "the header row is a bitmap row"
        );
        let bitmap = HBITMAP(header.dwTypeData.0.cast());
        assert!(
            !bitmap.is_invalid(),
            "the header row carries the composed bitmap"
        );
        let mut bm = windows::Win32::Graphics::Gdi::BITMAP::default();
        GetObjectW(
            HGDIOBJ(bitmap.0),
            size_of::<windows::Win32::Graphics::Gdi::BITMAP>() as i32,
            Some((&raw mut bm).cast()),
        );
        assert_eq!(
            bm.bmBitsPixel, 32,
            "premultiplied 32-bpp, blended with its alpha"
        );
        // The 64 px thumbnail is kept as is (never enlarged), with padding above and below
        // and the text to its right.
        assert!(
            bm.bmHeight.abs() > 64 && bm.bmHeight.abs() <= 128 + 32,
            "{}x{}",
            bm.bmWidth,
            bm.bmHeight
        );
        assert!(bm.bmWidth > 64, "room for the text: {}", bm.bmWidth);
        // No other row has an item bitmap: nothing pushes the text of the rows to the right.
        for i in 1..rows_before as u32 {
            assert!(
                item_info(sub, i, MIIM_BITMAP).hbmpItem.is_invalid(),
                "row {i} has an item bitmap"
            );
        }
        // Another menu's init does nothing; opening again keeps the row.
        menu.cast::<IContextMenu2>()
            .unwrap()
            .HandleMenuMsg(
                WM_INITMENUPOPUP,
                windows::Win32::Foundation::WPARAM(hmenu.0 as usize),
                windows::Win32::Foundation::LPARAM(0),
            )
            .unwrap();
        drop(menu);
        let _ = DestroyMenu(hmenu);
    }
}

/// The shell hands a short id range to the menu; the root used to take the last id and the
/// whole menu vanished when none was left. Now the root comes first and what fits follows.
#[test]
fn a_short_id_range_still_shows_the_thumbwright_menu() {
    let s = Scratch::new("menu-short");
    let path = s.file(
        "pic.png",
        &png(&RgbaImage::from_pixel(8, 8, Rgba([1, 2, 3, 255]))),
    );
    for last in [1u32, 2, 5, 12] {
        // SAFETY: shell item -> data object -> IShellExtInit, then QueryContextMenu into a
        // fresh popup menu owned here.
        unsafe {
            let menu: IContextMenu = factory(&CLSID_MENU).unwrap().CreateInstance(None).unwrap();
            let item: IShellItem =
                SHCreateItemFromParsingName(&HSTRING::from(path.as_os_str()), None).unwrap();
            let data: IDataObject = item.BindToHandler(None, &BHID_DataObject).unwrap();
            menu.cast::<IShellExtInit>()
                .unwrap()
                .Initialize(None, &data, None)
                .unwrap();
            let hmenu = CreatePopupMenu().unwrap();
            let hr = menu.QueryContextMenu(hmenu, 0, 1, last, CMF_NORMAL);
            assert!(hr.is_ok(), "{hr:?}");
            let used = hr.0 & 0xFFFF;
            assert_eq!(
                used as u32, last,
                "range 1..={last} is used up, not exceeded"
            );
            assert_eq!(
                label(hmenu, 0),
                tw_core::ids::PRODUCT_NAME,
                "range 1..={last}"
            );
            let sub = GetSubMenu(hmenu, 0);
            assert!(GetMenuItemCount(Some(sub)) >= 1, "range 1..={last}");
            let _ = DestroyMenu(hmenu);
        }
    }
}

#[test]
fn undrawn_menu_previews_do_not_leak_gdi_bitmaps() {
    use windows::Win32::System::Threading::{GR_GDIOBJECTS, GetCurrentProcess, GetGuiResources};
    let s = Scratch::new("menu-gdi");
    let path = s.file(
        "pic.png",
        &png(&RgbaImage::from_pixel(64, 64, Rgba([9, 8, 7, 255]))),
    );
    // SAFETY: querying this process's own GDI handle count.
    let gdi = || unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };
    // Long enough for the fetch thread to queue its bitmap from a warm thumbnail cache.
    let settle = Duration::from_millis(400);
    let round = || {
        let (menu, hmenu, _) = menu_for(&path);
        std::thread::sleep(settle);
        // SAFETY: a second query on the same object replaces the pending preview.
        unsafe {
            let hr = menu.QueryContextMenu(hmenu, 0, 1, 0x7FFF, CMF_NORMAL);
            assert!(hr.is_ok(), "{hr:?}");
        }
        std::thread::sleep(settle);
        drop(menu);
        // SAFETY: destroying the popup menu we own.
        let _ = unsafe { DestroyMenu(hmenu) };
    };
    // Warm-up: the shell's first thumbnail request allocates lasting GDI state.
    round();
    let before = gdi();
    for _ in 0..20 {
        round();
    }
    let after = gdi();
    // Each round used to leak up to two bitmaps (40 total); allow slack for
    // concurrently running tests' short-lived bitmaps.
    assert!(after <= before + 10, "GDI objects grew {before} -> {after}");
}

#[test]
fn folder_menu_offers_only_build_thumbnails() {
    let s = Scratch::new("menu-folder");
    let (menu, hmenu, count) = menu_for(&s.0);
    // SAFETY: inspecting a menu we own.
    unsafe {
        assert_eq!(count, 1);
        assert_eq!(GetMenuItemCount(Some(hmenu)), 1);
        assert_eq!(label(hmenu, 0), "Build thumbnails for this folder\u{2026}");
        let mut buf = [0u16; 64];
        menu.GetCommandString(
            0,
            GCS_VERBW,
            None,
            PSTR(buf.as_mut_ptr().cast()),
            buf.len() as u32,
        )
        .unwrap();
        let verb = String::from_utf16_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap()]);
        assert_eq!(verb, "tw.prebuild");
        let _ = DestroyMenu(hmenu);
    }
}

#[test]
fn folder_background_menu_is_built_from_the_folder_pidl() {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::SHParseDisplayName;
    let s = Scratch::new("menu-bg");
    // SAFETY: the background handler gets the folder's PIDL and no data object; the PIDL is
    // freed after Initialize, and the menu is one we own.
    unsafe {
        let menu: IContextMenu = factory(&CLSID_MENU).unwrap().CreateInstance(None).unwrap();
        let mut pidl = std::ptr::null_mut();
        SHParseDisplayName(&HSTRING::from(s.0.as_os_str()), None, &mut pidl, 0, None).unwrap();
        menu.cast::<IShellExtInit>()
            .unwrap()
            .Initialize(Some(pidl), None::<&IDataObject>, None)
            .unwrap();
        CoTaskMemFree(Some(pidl.cast()));
        let hmenu = CreatePopupMenu().unwrap();
        let hr = menu.QueryContextMenu(hmenu, 0, 1, 0x7FFF, CMF_NORMAL);
        assert_eq!(hr.0 & 0xFFFF, 1);
        assert_eq!(label(hmenu, 0), "Build thumbnails for this folder\u{2026}");
        let _ = DestroyMenu(hmenu);
    }
}

#[test]
fn context_menu_hidden_for_unsupported_types() {
    let s = Scratch::new("menu-exe");
    let path = s.file("tool.exe", b"MZ");
    let (_menu, hmenu, count) = menu_for(&path);
    assert_eq!(count, 0);
    // SAFETY: inspecting a menu we own.
    unsafe {
        assert_eq!(GetMenuItemCount(Some(hmenu)), 0);
        let _ = DestroyMenu(hmenu);
    }
}
