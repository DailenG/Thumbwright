//! Shell-integrated verbs: folder icon, lock screen, launching the settings app.

use std::path::{Path, PathBuf};

use image::codecs::ico::{IcoEncoder, IcoFrame};
use image::imageops::FilterType;
use image::{DynamicImage, ExtendedColorType, RgbaImage};
use windows::Storage::StorageFile;
use windows::System::UserProfile::LockScreen;
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_SYSTEM, GetFileAttributesW,
    INVALID_FILE_ATTRIBUTES, SetFileAttributesW,
};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
use windows::Win32::UI::Shell::{
    SHCNE_UPDATEDIR, SHCNE_UPDATEITEM, SHCNF_FLUSH, SHCNF_PATHW, SHChangeNotify,
};
use windows::core::{HSTRING, PCWSTR};

use crate::ops::{self, Result};

/// File name of the settings application, next to `twctl.exe`.
pub const SETTINGS_EXE: &str = "thumbwright-settings.exe";

/// Pads `img` with transparency to a square, then returns it at `edge` x `edge`.
fn square(img: &DynamicImage, edge: u32) -> RgbaImage {
    let side = img.width().max(img.height());
    let mut canvas = RgbaImage::new(side, side);
    image::imageops::overlay(
        &mut canvas,
        &img.to_rgba8(),
        i64::from((side - img.width()) / 2),
        i64::from((side - img.height()) / 2),
    );
    if side == edge {
        canvas
    } else {
        image::imageops::resize(&canvas, edge, edge, FilterType::Lanczos3)
    }
}

/// Multi-resolution `.ico` (PNG-compressed frames) of the square-padded image.
pub fn make_ico(img: &DynamicImage) -> Result<Vec<u8>> {
    let mut frames = Vec::new();
    for edge in [256u32, 64, 48, 32, 16] {
        let px = square(img, edge);
        frames.push(
            IcoFrame::as_png(px.as_raw(), edge, edge, ExtendedColorType::Rgba8)
                .map_err(|e| e.to_string())?,
        );
    }
    let mut out = Vec::new();
    IcoEncoder::new(&mut out)
        .encode_images(&frames)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

/// Sets `[.ShellClassInfo]` icon keys in `existing` (or a fresh file), keeping every other
/// line and section.
pub fn merge_desktop_ini(existing: Option<&str>, icon_file: &str) -> String {
    const KEYS: [&str; 3] = ["IconFile", "IconIndex", "IconResource"];
    let ours = [
        format!("IconResource={icon_file},0"),
        format!("IconFile={icon_file}"),
        "IconIndex=0".to_owned(),
    ];
    let mut out: Vec<String> = Vec::new();
    let (mut in_section, mut inserted) = (false, false);
    for line in existing.unwrap_or("").lines() {
        let t = line.trim();
        if let Some(name) = t.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            if in_section && !inserted {
                out.extend(ours.iter().cloned());
                inserted = true;
            }
            in_section = name.eq_ignore_ascii_case(".ShellClassInfo");
            out.push(line.to_owned());
            continue;
        }
        let key = t.split('=').next().unwrap_or("").trim();
        if in_section && KEYS.iter().any(|k| key.eq_ignore_ascii_case(k)) {
            continue;
        }
        out.push(line.to_owned());
    }
    if !inserted {
        if in_section {
            out.extend(ours.iter().cloned());
        } else {
            if out.last().is_some_and(|l| !l.is_empty()) {
                out.push(String::new());
            }
            out.push("[.ShellClassInfo]".to_owned());
            out.extend(ours.iter().cloned());
        }
    }
    out.join("\r\n") + "\r\n"
}

/// desktop.ini is read as UTF-16 with a BOM, UTF-8, or the ANSI page; UTF-16 and UTF-8 are
/// handled, anything else is treated as UTF-8 with replacement.
fn decode_ini(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let units: Vec<u16> = rest
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes))
            .into_owned()
    }
}

fn encode_ini(text: &str) -> Vec<u8> {
    let mut out = vec![0xFF, 0xFE];
    out.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    out
}

fn attributes(p: &Path) -> u32 {
    // SAFETY: valid NUL-terminated wide path.
    unsafe { GetFileAttributesW(&HSTRING::from(p.as_os_str())) }
}

fn set_attributes(p: &Path, attrs: u32) -> Result<()> {
    // SAFETY: valid NUL-terminated wide path.
    unsafe {
        SetFileAttributesW(
            &HSTRING::from(p.as_os_str()),
            windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(attrs),
        )
    }
    .map_err(|e| format!("{}: {e}", p.display()))
}

/// Writes `bytes` over a possibly hidden/system file, then marks it hidden + system.
fn write_hidden(path: &Path, bytes: &[u8]) -> Result<()> {
    let current = attributes(path);
    if current != INVALID_FILE_ATTRIBUTES {
        set_attributes(path, FILE_ATTRIBUTE_NORMAL.0)?;
    }
    ops::write_atomic(path, bytes)?;
    set_attributes(path, FILE_ATTRIBUTE_HIDDEN.0 | FILE_ATTRIBUTE_SYSTEM.0)
}

/// `folder-icon`: the image becomes the icon of its containing folder (hidden `.ico` plus
/// `desktop.ini`; the folder gets the system attribute so Explorer honours the file).
pub fn folder_icon(input: &Path, max: u64) -> Result<String> {
    let folder = crate::win::shell_path(input)
        .parent()
        .map(Path::to_path_buf)
        .ok_or("file has no parent folder")?;
    if folder.parent().is_none() {
        return Err("a drive root cannot take a folder icon".into());
    }
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let icon_name = format!("{stem}.folder.ico");
    let ico = make_ico(&ops::decode_full(input, max)?)?;
    let ini_path = folder.join("desktop.ini");
    let existing = std::fs::read(&ini_path).ok().map(|b| decode_ini(&b));
    write_hidden(&folder.join(&icon_name), &ico)?;
    write_hidden(
        &ini_path,
        &encode_ini(&merge_desktop_ini(existing.as_deref(), &icon_name)),
    )?;
    let attrs = attributes(&folder);
    if attrs == INVALID_FILE_ATTRIBUTES {
        return Err(format!("{}: cannot read attributes", folder.display()));
    }
    set_attributes(&folder, attrs | FILE_ATTRIBUTE_SYSTEM.0)?;
    let wide = HSTRING::from(folder.as_os_str());
    // SAFETY: valid NUL-terminated wide path for SHCNF_PATHW.
    unsafe {
        let p = Some(PCWSTR(wide.as_ptr()).0.cast());
        SHChangeNotify(SHCNE_UPDATEITEM, SHCNF_PATHW | SHCNF_FLUSH, p, None);
        SHChangeNotify(SHCNE_UPDATEDIR, SHCNF_PATHW | SHCNF_FLUSH, p, None);
    }
    Ok(format!("{} now uses {icon_name}", folder.display()))
}

fn winrt<T>(r: windows::core::Result<T>) -> Result<T> {
    r.map_err(|e| e.to_string())
}

/// `lock-screen`: the image (converted to PNG under `%LOCALAPPDATA%\Thumbwright`) becomes
/// the lock screen picture.
pub fn lock_screen(input: &Path, max: u64) -> Result<String> {
    let img = ops::decode_full(input, max)?;
    let store = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or("LOCALAPPDATA not set")?
        .join(tw_core::ids::PRODUCT_NAME);
    std::fs::create_dir_all(&store).map_err(|e| e.to_string())?;
    let out = store.join("lockscreen.png");
    ops::write_atomic(&out, &ops::encode(&img, "png", None)?)?;
    let path = HSTRING::from(out.as_os_str());
    std::thread::scope(|s| {
        s.spawn(|| {
            // SAFETY: initialises WinRT for this thread only.
            let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
            let file = winrt(winrt(StorageFile::GetFileFromPathAsync(&path))?.join())?;
            winrt(winrt(LockScreen::SetImageFileAsync(&file))?.join())
        })
        .join()
        .unwrap_or_else(|_| Err("lock screen: the call panicked".into()))
    })?;
    Ok(format!("lock screen set from {}", input.display()))
}

/// Opens `path` with the user's default application for its type.
pub fn open_in_editor(path: &Path) -> Result<()> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::w;
    // SAFETY: valid NUL-terminated strings; the call only starts another process.
    let h = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            &HSTRING::from(path.as_os_str()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // Values above 32 mean success.
    if h.0 as usize > 32 {
        Ok(())
    } else {
        Err(format!("cannot open {}", path.display()))
    }
}

/// `settings`: starts the settings application that ships next to `twctl.exe`.
pub fn settings(dir: &Path) -> Result<String> {
    let exe = dir.join(SETTINGS_EXE);
    if !exe.is_file() {
        return Err(format!(
            "the settings app ({SETTINGS_EXE}) is not installed next to twctl.exe ({})",
            dir.display()
        ));
    }
    std::process::Command::new(&exe)
        .current_dir(dir)
        .spawn()
        .map_err(|e| format!("{}: {e}", exe.display()))?;
    Ok("settings opened".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_ini_merge_keeps_foreign_content() {
        let existing = "[.ShellClassInfo]\r\nLocalizedResourceName=@x,-1\r\nIconFile=old.ico\r\nIconIndex=3\r\n[ViewState]\r\nMode=\r\n";
        let merged = merge_desktop_ini(Some(existing), "new.ico");
        assert!(merged.contains("LocalizedResourceName=@x,-1"));
        assert!(merged.contains("IconResource=new.ico,0"));
        assert!(merged.contains("IconFile=new.ico"));
        assert!(!merged.contains("old.ico") && !merged.contains("IconIndex=3"));
        assert!(merged.contains("[ViewState]\r\nMode="));
        // The icon keys land inside [.ShellClassInfo], before the next section.
        assert!(merged.find("IconResource").unwrap() < merged.find("[ViewState]").unwrap());

        let fresh = merge_desktop_ini(None, "a b.ico");
        assert_eq!(
            fresh,
            "[.ShellClassInfo]\r\nIconResource=a b.ico,0\r\nIconFile=a b.ico\r\nIconIndex=0\r\n"
        );
        let other = merge_desktop_ini(Some("[Other]\r\nA=1\r\n"), "i.ico");
        assert!(other.starts_with("[Other]\r\nA=1\r\n\r\n[.ShellClassInfo]"));
    }

    #[test]
    fn ini_roundtrips_utf16() {
        let text = "[.ShellClassInfo]\r\nIconFile=\u{e9}.ico\r\n";
        assert_eq!(decode_ini(&encode_ini(text)), text);
    }

    #[test]
    fn ico_has_all_frame_sizes_and_pads_to_square() {
        let ico = make_ico(&DynamicImage::new_rgba8(40, 10)).unwrap();
        assert_eq!(&ico[..4], &[0, 0, 1, 0]);
        assert_eq!(u16::from_le_bytes([ico[4], ico[5]]), 5);
    }
}
