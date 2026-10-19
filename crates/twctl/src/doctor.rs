//! `doctor`: the sectioned self-check of FUNCTIONAL_SPEC.md 7.1 with an `[ ok ]` /
//! `[warn]` / `[FAIL]` verdict and FIX hints. Read-only: it never changes the system.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::path::Path;
use std::sync::Arc;

use tw_core::config::{self, Settings};
use tw_core::decode::route::{self, Decoder};
use tw_core::formats::{self, OsCodec, Source};
use tw_core::registration::{self, Layout, Scope};
use tw_core::{ids, log, pipeline, worker};
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::Graphics::Gdi::{BITMAP, DeleteObject, GetObjectW, HGDIOBJ};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatHeif, GUID_ContainerFormatWmp, IWICImagingFactory,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
    GetDriveTypeW, GetFileAttributesW, INVALID_FILE_ATTRIBUTES,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName};
use windows::core::{GUID, HRESULT, HSTRING, Interface, PCSTR};
use windows_registry::{CLASSES_ROOT, CURRENT_USER, Key, LOCAL_MACHINE};

use crate::ops;

const LOG_TAIL_BYTES: u64 = 256 * 1024;
const EXAMPLES: usize = 5;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Level {
    Info,
    Ok,
    Warn,
    Fail,
}

impl Level {
    fn tag(self) -> &'static str {
        match self {
            Self::Info => "[info]",
            Self::Ok => "[ ok ]",
            Self::Warn => "[warn]",
            Self::Fail => "[FAIL]",
        }
    }
}

struct Problem {
    level: Level,
    what: String,
    fix: String,
}

#[derive(Default)]
struct Doc {
    text: String,
    problems: Vec<Problem>,
}

impl Doc {
    fn section(&mut self, n: usize, title: &str) {
        if !self.text.is_empty() {
            self.text.push('\n');
        }
        self.text.push_str(&format!("== {n}. {title} ==\n"));
    }

    fn line(&mut self, level: Level, text: impl AsRef<str>) {
        self.text
            .push_str(&format!("{} {}\n", level.tag(), text.as_ref()));
    }

    fn plain(&mut self, text: impl AsRef<str>) {
        self.text.push_str(&format!("       {}\n", text.as_ref()));
    }

    /// A finding that also lands in the verdict list.
    fn problem(&mut self, level: Level, what: impl Into<String>, fix: impl Into<String>) {
        let (what, fix) = (what.into(), fix.into());
        self.line(level, &what);
        self.problems.push(Problem { level, what, fix });
    }
}

fn reg_string(root: &Key, path: &str, name: &str) -> Option<String> {
    root.open(path).ok()?.get_string(name).ok()
}

fn reg_u32(root: &Key, path: &str, name: &str) -> Option<u32> {
    root.open(path).ok()?.get_u32(name).ok()
}

fn human(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= KIB * KIB * KIB {
        format!("{:.1} GiB", b / (KIB * KIB * KIB))
    } else if b >= KIB * KIB {
        format!("{:.1} MiB", b / (KIB * KIB))
    } else {
        format!("{:.1} KiB", b / KIB)
    }
}

fn log_path() -> Option<std::path::PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| Path::new(&d).join("Thumbwright.log"))
}

/// The last `LOG_TAIL_BYTES` of the diagnostics log as lines.
pub fn log_tail() -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    let Some(path) = log_path() else {
        return Vec::new();
    };
    let Ok(mut f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(LOG_TAIL_BYTES)));
    let mut bytes = Vec::new();
    let _ = f.take(LOG_TAIL_BYTES).read_to_end(&mut bytes);
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    if len > LOG_TAIL_BYTES && !lines.is_empty() {
        lines.remove(0); // a partial first line
    }
    lines
}

/// The OS line. The registry's `ProductName` still reads "Windows 10 ..." on Windows 11, so
/// the build number decides (Windows 11 starts at build 22000).
fn os_label(product: &str, display: &str, build: &str, ubr: &str) -> String {
    let product = match product.strip_prefix("Windows 10") {
        Some(rest) if build.parse::<u32>().is_ok_and(|b| b >= 22000) => {
            format!("Windows 11{rest}")
        }
        _ => product.to_owned(),
    };
    format!("{product} {display} (build {build}{ubr})")
}

fn environment(doc: &mut Doc, dir: &Path) {
    doc.section(1, "Environment");
    doc.line(
        Level::Info,
        format!("Thumbwright {} ({})", ids::VERSION, std::env::consts::ARCH),
    );
    let nt = "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion";
    let product = reg_string(LOCAL_MACHINE, nt, "ProductName").unwrap_or_else(|| "Windows".into());
    let display = reg_string(LOCAL_MACHINE, nt, "DisplayVersion").unwrap_or_default();
    let build = reg_string(LOCAL_MACHINE, nt, "CurrentBuild").unwrap_or_else(|| "?".into());
    let ubr = reg_u32(LOCAL_MACHINE, nt, "UBR").map_or(String::new(), |u| format!(".{u}"));
    doc.line(
        Level::Info,
        format!("OS: {}", os_label(&product, &display, &build, &ubr)),
    );
    let dll = dir.join(ids::SHELL_DLL_NAME);
    match std::fs::metadata(&dll) {
        Ok(m) => doc.line(
            Level::Ok,
            format!("DLL: {} ({})", dll.display(), human(m.len())),
        ),
        Err(_) => doc.problem(
            Level::Fail,
            format!("DLL not found next to twctl.exe: {}", dll.display()),
            format!(
                "Reinstall Thumbwright, or keep {} in the same folder as twctl.exe.",
                ids::SHELL_DLL_NAME
            ),
        ),
    }
    match log_path() {
        Some(p) => doc.line(Level::Info, format!("Log: {}", p.display())),
        None => doc.line(Level::Warn, "Log path unknown (LOCALAPPDATA is not set)"),
    }
    let recent: Vec<String> = log_tail()
        .into_iter()
        .filter(|l| l.contains(" ERROR ") || l.contains("PANIC"))
        .collect();
    if recent.is_empty() {
        doc.line(Level::Ok, "No recent ERROR/PANIC lines in the log");
    } else {
        doc.line(
            Level::Warn,
            format!("{} recent ERROR/PANIC line(s); newest:", recent.len()),
        );
        for l in recent.iter().rev().take(EXAMPLES).rev() {
            doc.plain(l);
        }
    }
}

fn windows_settings(doc: &mut Doc) {
    doc.section(2, "Windows thumbnail settings");
    let adv = "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced";
    match reg_u32(CURRENT_USER, adv, "IconsOnly") {
        Some(1) => doc.problem(
            Level::Fail,
            "Explorer is set to always show icons, never thumbnails (IconsOnly=1)",
            "File Explorer > View > Options > View: untick \"Always show icons, never thumbnails\".",
        ),
        Some(v) => doc.line(Level::Ok, format!("IconsOnly={v} (thumbnails are shown)")),
        None => doc.line(Level::Ok, "IconsOnly is not set (thumbnails are shown)"),
    }
    match reg_u32(CURRENT_USER, adv, "ShowTypeOverlay") {
        Some(v) => doc.line(Level::Info, format!("ShowTypeOverlay={v}")),
        None => doc.line(Level::Info, "ShowTypeOverlay is not set (Windows default)"),
    }
    let mut found = false;
    for (hive_name, hive) in [("HKCU", &CURRENT_USER), ("HKLM", &LOCAL_MACHINE)] {
        for path in [
            "Software\\Microsoft\\Windows\\CurrentVersion\\Policies\\Explorer",
            "Software\\Policies\\Microsoft\\Windows\\Explorer",
        ] {
            for name in [
                "DisableThumbnails",
                "DisableThumbnailCache",
                "NoThumbnailCache",
                "DisableThumbnailsOnNetworkFolders",
                "DisableThumbsDBOnNetworkFolders",
            ] {
                if let Some(v) = reg_u32(hive, path, name)
                    && v != 0
                {
                    found = true;
                    let blocking = matches!(
                        name,
                        "DisableThumbnails" | "DisableThumbnailCache" | "NoThumbnailCache"
                    );
                    doc.problem(
                        if blocking { Level::Fail } else { Level::Warn },
                        format!("policy {hive_name}\\{path}\\{name}={v}"),
                        "A group policy or registry setting disables thumbnails; ask your administrator to remove it, or delete the value if it was set by hand.",
                    );
                }
            }
        }
    }
    if !found {
        doc.line(Level::Ok, "No thumbnail-disabling policies");
    }
}

type GetClassObject =
    unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;

/// Loads `dll` and asks it for `clsid`'s class factory, as COM would.
fn dll_serves(dll: &Path, clsid: &ids::ClassId) -> Result<(), String> {
    // SAFETY: loading our own shipped DLL, calling its documented export with valid
    // pointers, releasing the factory and unloading it again.
    unsafe {
        let module = LoadLibraryW(&HSTRING::from(dll.as_os_str()))
            .map_err(|e| format!("cannot load: {e}"))?;
        let result = (|| {
            let export = GetProcAddress(module, PCSTR(c"DllGetClassObject".as_ptr().cast()))
                .ok_or("no DllGetClassObject export")?;
            let get: GetClassObject = std::mem::transmute(export);
            let guid = GUID::from_u128(clsid.guid);
            let mut out: *mut c_void = std::ptr::null_mut();
            let hr = get(
                &guid,
                &windows::Win32::System::Com::IClassFactory::IID,
                &mut out,
            );
            if hr.is_err() {
                return Err(format!("DllGetClassObject returned {hr:?}"));
            }
            drop(windows::Win32::System::Com::IClassFactory::from_raw(out));
            Ok(())
        })();
        let _ = FreeLibrary(HMODULE(module.0));
        result
    }
}

fn com_registration(doc: &mut Doc, dir: &Path, settings: &Settings) {
    doc.section(3, "COM registration");
    for (scope, name) in [(Scope::User, "per-user"), (Scope::Machine, "machine-wide")] {
        match registration::status(&Layout::for_scope(scope)) {
            Some(s) => doc.line(
                Level::Info,
                format!(
                    "{name} registration: {} file types, context menu {} ({})",
                    s.thumbnail_extensions,
                    if s.context_menu { "on" } else { "off" },
                    s.dll_path
                ),
            ),
            None => doc.line(Level::Info, format!("{name} registration: none")),
        }
    }
    let classes: [(&ids::ClassId, &str, bool); 5] = [
        (
            &ids::CLSID_THUMBNAIL,
            "thumbnail provider",
            settings.thumbnails_enabled,
        ),
        (
            &ids::CLSID_CONTEXT_MENU,
            "classic context menu",
            settings.menu_enabled,
        ),
        (&ids::CLSID_PREVIEW, "preview handler", false),
        (&ids::CLSID_PROPERTY, "property handler", false),
        (
            &ids::CLSID_CLOUD_THUMBNAIL,
            "cloud-folder thumbnail provider",
            false,
        ),
    ];
    for (class, label, wanted) in classes {
        let braced = class.braced();
        let server = reg_string(
            CLASSES_ROOT,
            &format!("CLSID\\{braced}\\InprocServer32"),
            "",
        );
        match server {
            None if wanted => doc.problem(
                Level::Fail,
                format!("{label} {braced}: missing"),
                "Run `twctl register` (portable copy) or repair the installation.",
            ),
            None => doc.line(Level::Info, format!("{label} {braced}: not registered (optional)")),
            Some(path) if !Path::new(&path).is_file() => doc.problem(
                Level::Fail,
                format!("{label} {braced}: registered to {path}, which does not exist"),
                "Run `twctl register --off` then `twctl register` from the folder that holds the DLL, or repair the installation.",
            ),
            Some(path) => match dll_serves(Path::new(&path), class) {
                Ok(()) => doc.line(Level::Ok, format!("{label} {braced}: registered, loads OK ({path})")),
                Err(e) => doc.problem(
                    if wanted { Level::Fail } else { Level::Warn },
                    format!("{label} {braced}: registered ({path}) but does not load: {e}"),
                    "The registered DLL is stale or damaged; re-register from the current folder.",
                ),
            },
        }
        let listed = [&LOCAL_MACHINE, &CURRENT_USER].iter().any(|h| {
            reg_string(
                h,
                "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Shell Extensions\\Approved",
                &braced,
            )
            .is_some()
        });
        if listed {
            doc.plain("listed in Approved Shell Extensions");
        } else {
            doc.plain(
                "not in Approved Shell Extensions (required only for machine-wide registration)",
            );
        }
    }
    let _ = dir;
}

fn clsid_name(clsid: &str) -> String {
    reg_string(CLASSES_ROOT, &format!("CLSID\\{clsid}"), "").unwrap_or_else(|| "unnamed".into())
}

fn associations(doc: &mut Doc, settings: &Settings) {
    doc.section(4, "Per-format file associations");
    let routable = formats::all()
        .iter()
        .filter(|f| !route::chain_for_ext(f.ext).is_empty())
        .count();
    let enabled_exts: Vec<&str> = formats::all()
        .iter()
        .filter(|f| !route::chain_for_ext(f.ext).is_empty() && settings.type_enabled(f.ext))
        .map(|f| f.ext)
        .collect();
    let enabled = enabled_exts.len();
    let registration::Coverage {
        hooked, foreign, ..
    } = registration::coverage(enabled_exts);
    doc.line(
        Level::Info,
        format!("{enabled} of {routable} file types enabled"),
    );
    if enabled > 0 && hooked == 0 {
        doc.problem(
            Level::Warn,
            format!("0 of {enabled} enabled types are hooked to Thumbwright"),
            "Run `twctl register` (portable) or repair the installation; until then Windows uses its own thumbnails.",
        );
    } else {
        doc.line(Level::Ok, format!("{hooked} hooked to Thumbwright"));
    }
    if foreign.is_empty() {
        doc.line(Level::Ok, "0 owned by another program");
    } else {
        let eg: Vec<String> = foreign
            .iter()
            .take(EXAMPLES)
            .map(|(e, c)| format!(".{e} -> {} {c}", clsid_name(c)))
            .collect();
        doc.problem(
            Level::Warn,
            format!("{} owned by another program, e.g. {}", foreign.len(), eg.join("; ")),
            "Another program's thumbnail handler is registered for these types. Re-run `twctl register` to take them over, or turn the types off in Settings > File types.",
        );
    }
    for (scope, name) in [(Scope::User, "per-user"), (Scope::Machine, "machine-wide")] {
        let taken = registration::taken_over(&Layout::for_scope(scope));
        if taken.is_empty() {
            continue;
        }
        let eg: Vec<String> = taken
            .iter()
            .take(EXAMPLES)
            .map(|(path, _)| path.clone())
            .collect();
        doc.line(
            Level::Info,
            format!(
                "{} {name} registry value(s) taken over by another program, left alone: {}",
                taken.len(),
                eg.join("; ")
            ),
        );
    }
}

fn progid_handlers(doc: &mut Doc, settings: &Settings) {
    doc.section(5, "ProgID-level thumbnail handlers");
    let mut by_progid: BTreeMap<(String, String), Vec<&str>> = BTreeMap::new();
    for f in formats::all() {
        if route::chain_for_ext(f.ext).is_empty() || !settings.type_enabled(f.ext) {
            continue;
        }
        if let Some(key) = registration::foreign_progid_handler(f.ext) {
            by_progid.entry(key).or_default().push(f.ext);
        }
    }
    if by_progid.is_empty() {
        doc.line(
            Level::Ok,
            "No foreign ProgID-level handlers take precedence",
        );
        return;
    }
    for ((progid, clsid), exts) in &by_progid {
        let list: Vec<String> = exts
            .iter()
            .take(EXAMPLES)
            .map(|e| format!(".{e}"))
            .collect();
        doc.problem(
            Level::Warn,
            format!(
                "ProgID {progid} has handler {clsid} ({}) that takes precedence for {} type(s): {}{}",
                clsid_name(clsid),
                exts.len(),
                list.join(" "),
                if exts.len() > EXAMPLES { " ..." } else { "" }
            ),
            format!(
                "Windows consults the ProgID before the extension, so Thumbwright is not used for these types. Remove the handler under HKEY_CLASSES_ROOT\\{progid}\\ShellEx, or uninstall the program that owns it."
            ),
        );
    }
}

fn cloud_folders(doc: &mut Doc, settings: &Settings) {
    doc.section(6, "Cloud sync folders");
    doc.line(
        Level::Info,
        format!(
            "Thumbnails in cloud folders: {} (the cloud provider class is registered separately, see section 3)",
            if settings.cloud_thumbnails { "on" } else { "off" }
        ),
    );
    let path = "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Explorer\\SyncRootManager";
    let mut any = false;
    for root in [&LOCAL_MACHINE, &CURRENT_USER] {
        let Ok(key) = root.open(path) else { continue };
        let Ok(names) = key.keys() else { continue };
        for name in names {
            any = true;
            let provider = key
                .open(&name)
                .ok()
                .and_then(|k| k.get_string("ThumbnailProvider").ok());
            let state = match provider {
                Some(c) if c.eq_ignore_ascii_case(&ids::CLSID_CLOUD_THUMBNAIL.braced()) => {
                    "linked to Thumbwright".to_owned()
                }
                Some(c) => format!("uses thumbnail provider {c}"),
                None => "uses the Windows default thumbnails".to_owned(),
            };
            doc.line(Level::Info, format!("{name}: {state}"));
        }
    }
    if !any {
        doc.line(Level::Info, "No cloud sync roots registered");
    }
}

/// How many values `HKCU\Software\Thumbwright` holds; `None` when the key does not exist.
fn store_value_count() -> Option<usize> {
    let key = CURRENT_USER.open(config::PRODUCT_KEY).ok()?;
    Some(key.values().map_or(0, Iterator::count))
}

fn settings_summary(doc: &mut Doc, dir: &Path, settings: &Settings) {
    doc.section(7, "Settings summary");
    if config::is_portable(dir) {
        doc.line(
            Level::Info,
            format!(
                "Settings store: portable ({})",
                config::portable_ini(dir).display()
            ),
        );
    } else {
        // The settings window must write the real HKCU key. A packaged process would have its
        // writes redirected into the package's private hive, and Explorer would never see them.
        let key = format!("HKCU\\{}", config::PRODUCT_KEY);
        match store_value_count() {
            Some(n) if n > 0 => {
                doc.line(Level::Info, format!("Settings store: {key} has {n} values"))
            }
            _ => doc.line(
                Level::Info,
                format!(
                    "Settings store: {key} has 0 values (defaults are in use; nothing saved yet)"
                ),
            ),
        }
    }
    doc.line(
        if settings.thumbnails_enabled {
            Level::Ok
        } else {
            Level::Warn
        },
        format!(
            "Thumbnails enabled: {}",
            if settings.thumbnails_enabled {
                "yes"
            } else {
                "no"
            }
        ),
    );
    doc.line(
        Level::Info,
        format!("Max file size: {} MB", settings.max_file_mb),
    );
    doc.line(
        Level::Info,
        format!("Max thumbnail size: {} px", settings.max_thumb_px),
    );
    doc.line(
        Level::Info,
        format!(
            "Prefer a file's embedded thumbnail: {}",
            if settings.use_embedded_thumbs {
                "yes"
            } else {
                "no"
            }
        ),
    );
    if !settings.disabled_types.is_empty() {
        doc.line(
            Level::Info,
            format!("{} file type(s) turned off", settings.disabled_types.len()),
        );
    }
}

fn licence_and_updates(doc: &mut Doc) {
    doc.section(8, "Licence");
    doc.line(
        Level::Ok,
        "GPL-3.0-or-later free software; no licence key is needed",
    );
    doc.section(9, "Updates");
    doc.line(
        Level::Info,
        "Updates: manual (Thumbwright makes no network connections)",
    );
}

fn self_test_png() -> Vec<u8> {
    let img = image::RgbaImage::from_fn(64, 64, |x, y| {
        image::Rgba([(x * 4) as u8, (y * 4) as u8, 128, 255])
    });
    ops::encode(&img.into(), "png", None).unwrap_or_default()
}

const SELF_TEST_SVG: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64"><rect width="64" height="64" fill="#36c"/></svg>"##;

fn module_loads(name: &str) -> bool {
    // SAFETY: loads a system DLL by name and releases it immediately.
    unsafe {
        match LoadLibraryW(&HSTRING::from(name)) {
            Ok(m) => {
                let _ = FreeLibrary(HMODULE(m.0));
                true
            }
            Err(_) => false,
        }
    }
}

fn decode_engine(doc: &mut Doc, dir: &Path) {
    doc.section(10, "Decode engine");
    let engine = pipeline::Engine::new(dir.join(ids::WORKER_EXE_NAME));
    match engine.thumbnail(Arc::new(self_test_png()), Some("png"), 64) {
        Ok(t) if t.width == 64 && t.height == 64 => {
            doc.line(
                Level::Ok,
                "64x64 in-process self-test: decoded and rendered",
            );
        }
        Ok(t) => doc.problem(
            Level::Fail,
            format!("64x64 self-test produced {}x{}", t.width, t.height),
            "The decode pipeline is damaged; reinstall Thumbwright.",
        ),
        Err(e) => doc.problem(
            Level::Fail,
            format!("64x64 self-test failed: {e}"),
            "The decode pipeline is damaged; reinstall Thumbwright.",
        ),
    }
    let worker_exe = dir.join(ids::WORKER_EXE_NAME);
    match worker::run(
        &worker_exe,
        Decoder::Svg,
        "svg",
        Arc::new(SELF_TEST_SVG.to_vec()),
        64,
        tw_core::limits::DEFAULT_VIDEO_PERCENT,
        worker::budget(Decoder::Svg),
    ) {
        Ok(t) if t.width > 0 => doc.line(Level::Ok, "Isolated worker self-test (SVG): OK"),
        Ok(_) => doc.problem(
            Level::Fail,
            "Isolated worker returned an empty image",
            "Reinstall Thumbwright.",
        ),
        Err(e) => doc.problem(
            Level::Fail,
            format!("Isolated worker self-test failed: {e}"),
            format!(
                "Keep {} next to the DLL and twctl.exe; reinstall if it is missing.",
                ids::WORKER_EXE_NAME
            ),
        ),
    }
    if module_loads("mfplat.dll") {
        doc.line(Level::Ok, "Media Foundation: present");
    } else {
        doc.line(
            Level::Warn,
            "Media Foundation: not present (video frames are unavailable)",
        );
    }
}

/// True when a WIC decoder for `container` is installed.
fn wic_has(container: &GUID) -> bool {
    // SAFETY: COM on this thread, a documented in-proc factory.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        CoCreateInstance::<_, IWICImagingFactory>(
            &CLSID_WICImagingFactory,
            None,
            CLSCTX_INPROC_SERVER,
        )
        .and_then(|f| f.CreateDecoder(container, std::ptr::null()))
        .is_ok()
    }
}

fn hevc_extension_installed() -> bool {
    let path = "Local Settings\\Software\\Microsoft\\Windows\\CurrentVersion\\AppModel\\Repository\\Packages";
    let Ok(key) = CLASSES_ROOT.open(path) else {
        return false;
    };
    key.keys().is_ok_and(|mut names| {
        names.any(|n| {
            let n = n.to_ascii_lowercase();
            n.starts_with("microsoft.hevcvideoextension")
                || n.starts_with("microsoft.hevcvideoextensions")
        })
    })
}

fn capability(doc: &mut Doc) {
    doc.section(11, "Format capability");
    let routed = formats::all()
        .iter()
        .filter(|f| !route::chain_for_ext(f.ext).is_empty())
        .count();
    doc.line(
        if routed > 0 { Level::Ok } else { Level::Fail },
        format!(
            "{routed} of {} formats have a decoder in this build",
            formats::all().len()
        ),
    );
    for (source, label) in [
        (Source::FullDecode, "full decode"),
        (Source::EmbeddedPreview, "embedded preview"),
        (Source::CoverOrFirstPage, "cover or first page"),
        (Source::CoverArt, "cover art"),
        (Source::VideoFrame, "video frame"),
        (Source::ContactSheet, "contact sheet"),
    ] {
        let total = formats::all().iter().filter(|f| f.source == source).count();
        let have = formats::all()
            .iter()
            .filter(|f| f.source == source && !route::chain_for_ext(f.ext).is_empty())
            .count();
        doc.plain(format!("{label}: {have} of {total} available"));
    }
    let jxr = wic_has(&GUID_ContainerFormatWmp);
    let heif = wic_has(&GUID_ContainerFormatHeif);
    let hevc = hevc_extension_installed();
    let os = |c: OsCodec| formats::all().iter().filter(|f| f.os_codec == c).count();
    doc.line(
        if jxr { Level::Ok } else { Level::Warn },
        format!(
            "WIC JPEG XR: {} ({} formats depend on it)",
            if jxr { "present" } else { "missing" },
            os(OsCodec::WicJpegXr)
        ),
    );
    doc.line(
        if heif { Level::Ok } else { Level::Warn },
        format!(
            "WIC HEIF: {} ({} formats depend on it)",
            if heif { "present" } else { "missing" },
            os(OsCodec::WicHeif)
        ),
    );
    doc.line(
        if hevc { Level::Ok } else { Level::Info },
        format!(
            "HEVC video extension: {} (needed for HEIC photos)",
            if hevc { "installed" } else { "not found" }
        ),
    );
}

fn this_file(doc: &mut Doc, dir: &Path, path: &Path, settings: &Settings) {
    let path = crate::win::shell_path(path);
    let path = path.as_path();
    doc.section(12, "This file");
    doc.line(Level::Info, format!("File: {}", path.display()));
    let Ok(meta) = std::fs::metadata(path) else {
        doc.problem(
            Level::Fail,
            format!("{}: cannot read the file", path.display()),
            "Check the path and your permissions.",
        );
        return;
    };
    let wide = HSTRING::from(path.as_os_str());
    // SAFETY: valid NUL-terminated wide strings.
    let (attrs, drive) = unsafe {
        let root = path.ancestors().last().map(|r| {
            HSTRING::from(format!(
                "{}\\",
                r.display().to_string().trim_end_matches('\\')
            ))
        });
        let drive = match path.to_string_lossy().starts_with(r"\\") {
            true => 4, // DRIVE_REMOTE
            false => root.map_or(0, |r| GetDriveTypeW(&r)),
        };
        (GetFileAttributesW(&wide), drive)
    };
    let volume = match drive {
        2 => "removable",
        3 => "fixed disk",
        4 => "network",
        5 => "optical",
        6 => "RAM disk",
        _ => "unknown",
    };
    doc.line(Level::Info, format!("Volume type: {volume}"));
    if drive == 4 {
        doc.line(Level::Warn, "Explorer does not always thumbnail network folders (see policy DisableThumbnailsOnNetworkFolders)");
    }
    if attrs != INVALID_FILE_ATTRIBUTES
        && attrs
            & (FILE_ATTRIBUTE_OFFLINE.0
                | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS.0
                | FILE_ATTRIBUTE_RECALL_ON_OPEN.0)
            != 0
    {
        doc.line(
            Level::Warn,
            "The file is a cloud placeholder; Windows may not download it just to thumbnail it",
        );
    }
    let ext = ops::ext_of(path);
    let chain = route::chain_for_ext(&ext);
    if chain.is_empty() {
        doc.problem(
            Level::Warn,
            format!(".{ext} is not a type Thumbwright handles"),
            "Windows' own handler (if any) is used for this type.",
        );
    } else if !settings.type_enabled(&ext) {
        doc.problem(
            Level::Warn,
            format!(".{ext} is turned off in Settings > File types"),
            "Turn the type back on in Settings, then run `twctl register` again.",
        );
    } else {
        doc.line(
            Level::Ok,
            format!(
                ".{ext} is supported ({})",
                formats::lookup(&ext).map_or("", |f| f.description)
            ),
        );
    }
    let ours = ids::CLSID_THUMBNAIL.braced();
    match registration::extension_handler(&ext) {
        Some(c) if c.eq_ignore_ascii_case(&ours) => doc.line(
            Level::Ok,
            "Windows resolves the thumbnail handler to Thumbwright",
        ),
        Some(c) => doc.problem(
            Level::Warn,
            format!(
                "Windows resolves the thumbnail handler to {c} ({}), not Thumbwright",
                clsid_name(&c)
            ),
            "Re-run `twctl register` to take the type over.",
        ),
        None => doc.line(
            Level::Info,
            "No thumbnail handler is registered for this extension",
        ),
    }
    if let Some((progid, clsid)) = registration::foreign_progid_handler(&ext) {
        doc.problem(
            Level::Warn,
            format!("ProgID {progid} has handler {clsid} that takes precedence over the extension"),
            format!("Remove the handler under HKEY_CLASSES_ROOT\\{progid}\\ShellEx, or uninstall the program that owns it."),
        );
    }
    let marker = format!("ext={ext} ");
    let asked = log_tail()
        .into_iter()
        .filter(|l| l.contains("GetThumbnail") && l.contains(&marker))
        .count();
    if asked > 0 {
        doc.line(
            Level::Info,
            format!("Explorer asked the handler for .{ext} and the log records {asked} failure(s)"),
        );
    } else {
        doc.line(
            Level::Info,
            format!("No failed request for .{ext} in the log (the handler logs failures only)"),
        );
    }
    let engine = pipeline::Engine::new(dir.join(ids::WORKER_EXE_NAME));
    match ops::read_input(path, settings.max_file_bytes()).and_then(|data| {
        engine
            .thumbnail(Arc::new(data), Some(&ext), 256.min(settings.max_thumb_px))
            .map_err(|e| e.to_string())
    }) {
        Ok(t) => doc.line(
            Level::Ok,
            format!("Direct decode: OK ({}x{} px)", t.width, t.height),
        ),
        Err(e) => doc.problem(
            Level::Fail,
            format!("Direct decode failed: {e}"),
            "The file may be damaged or use an unsupported variant of the format.",
        ),
    }
    let shell = shell_thumbnail_size(path);
    match shell {
        Ok((w, h)) => doc.line(Level::Ok, format!("Shell thumbnail: {w}x{h} px")),
        Err(e) => doc.problem(
            Level::Warn,
            format!("The shell returned no thumbnail: {e}"),
            "Check section 3 (registration) and 5 (competing handlers); run Rebuild thumbnail cache if the handler was just registered.",
        ),
    }
    let _ = meta;
}

/// Size of the thumbnail the OS shell produces for `path` (through the cache and surrogate).
fn shell_thumbnail_size(path: &Path) -> Result<(i32, i32), String> {
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::UI::Shell::SIIGBF_THUMBNAILONLY;
    // SAFETY: STA COM, shell item APIs with valid arguments; the bitmap is freed.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let factory: IShellItemImageFactory =
            SHCreateItemFromParsingName(&HSTRING::from(path.as_os_str()), None)
                .map_err(|e| e.to_string())?;
        let bmp = factory
            .GetImage(SIZE { cx: 256, cy: 256 }, SIIGBF_THUMBNAILONLY)
            .map_err(|e| e.message())?;
        let mut info = BITMAP::default();
        let got = GetObjectW(
            HGDIOBJ(bmp.0),
            size_of::<BITMAP>() as i32,
            Some((&raw mut info).cast()),
        );
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        if got == 0 {
            return Err("cannot read the bitmap".into());
        }
        Ok((info.bmWidth, info.bmHeight.abs()))
    }
}

fn verdict(doc: &mut Doc) {
    let n = 13;
    doc.section(n, "Verdict");
    let problems: Vec<(Level, String, String)> = doc
        .problems
        .iter()
        .filter(|p| p.level >= Level::Warn)
        .map(|p| (p.level, p.what.clone(), p.fix.clone()))
        .collect();
    let worst = problems.iter().map(|p| p.0).max().unwrap_or(Level::Ok);
    if problems.is_empty() {
        doc.line(Level::Ok, "No problems found.");
        return;
    }
    doc.line(worst, format!("{} problem(s):", problems.len()));
    for (i, (level, what, fix)) in problems.iter().enumerate() {
        doc.plain(format!("{}. {} {what}", i + 1, level.tag()));
        doc.plain(format!("   FIX: {fix}"));
    }
}

/// The full report text.
pub fn report(dir: &Path, file: Option<&Path>) -> String {
    let settings = Settings::load(dir);
    let mut doc = Doc::default();
    environment(&mut doc, dir);
    windows_settings(&mut doc);
    com_registration(&mut doc, dir, &settings);
    associations(&mut doc, &settings);
    progid_handlers(&mut doc, &settings);
    cloud_folders(&mut doc, &settings);
    settings_summary(&mut doc, dir, &settings);
    licence_and_updates(&mut doc);
    decode_engine(&mut doc, dir);
    capability(&mut doc);
    match file {
        Some(f) => this_file(&mut doc, dir, f, &settings),
        None => {
            doc.section(12, "This file");
            doc.line(
                Level::Info,
                "No file given (run `twctl doctor <file>` to test one)",
            );
        }
    }
    verdict(&mut doc);
    log::info("doctor ran");
    doc.text.trim_end().to_owned()
}

/// `--bundle out.zip`: the report plus the log tail and a settings dump, for support.
pub fn bundle(out: &Path, dir: &Path, report_text: &str) -> Result<(), String> {
    use std::io::Write;
    let settings = Settings::load(dir);
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default();
        for (name, body) in [
            ("doctor.txt", report_text.to_owned()),
            ("log-tail.txt", log_tail().join("\n")),
            ("settings.txt", format!("{settings:#?}")),
        ] {
            zip.start_file(name, opts).map_err(|e| e.to_string())?;
            zip.write_all(body.as_bytes()).map_err(|e| e.to_string())?;
        }
        zip.finish().map_err(|e| e.to_string())?;
    }
    ops::write_atomic(out, &buf.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_has_every_section_and_a_verdict() {
        let dir = std::env::temp_dir();
        let text = report(&dir, None);
        for n in 1..=13 {
            assert!(
                text.contains(&format!("== {n}. ")),
                "section {n} missing:\n{text}"
            );
        }
        assert!(text.contains("[ ok ] 64x64 in-process self-test"), "{text}");
    }

    /// The shell DLL cargo builds next to the test executables.
    fn built_dll() -> Option<std::path::PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let dll = exe.parent()?.parent()?.join(ids::SHELL_DLL_NAME);
        dll.is_file().then_some(dll)
    }

    #[test]
    fn dll_check_loads_the_real_dll_and_rejects_unknown_classes() {
        let Some(dll) = built_dll() else {
            eprintln!("shell DLL not built; skipping");
            return;
        };
        dll_serves(&dll, &ids::CLSID_THUMBNAIL).unwrap();
        dll_serves(&dll, &ids::CLSID_CONTEXT_MENU).unwrap();
        let unknown = ids::ClassId {
            guid: 0x1234_5678_9ABC_4DEF_8123_4567_89AB_CDEF,
            display_name: "unknown",
        };
        let err = dll_serves(&dll, &unknown).unwrap_err();
        assert!(err.contains("DllGetClassObject"), "{err}");
        assert!(dll_serves(Path::new(r"C:\no\such.dll"), &ids::CLSID_THUMBNAIL).is_err());
    }

    #[test]
    fn level_ordering_makes_fail_worst() {
        assert!(Level::Fail > Level::Warn && Level::Warn > Level::Ok && Level::Ok > Level::Info);
    }

    #[test]
    fn windows_11_is_named_from_the_build_number() {
        // The registry's ProductName still says "Windows 10" on Windows 11.
        assert_eq!(
            os_label("Windows 10 Pro", "26H2", "26340", ".9750"),
            "Windows 11 Pro 26H2 (build 26340.9750)"
        );
        assert_eq!(
            os_label("Windows 10 Home", "21H2", "22000", ""),
            "Windows 11 Home 21H2 (build 22000)"
        );
        assert_eq!(
            os_label("Windows 10 Pro", "22H2", "19045", ".5000"),
            "Windows 10 Pro 22H2 (build 19045.5000)"
        );
        assert_eq!(
            os_label("Windows Server 2025 Standard", "24H2", "26100", ""),
            "Windows Server 2025 Standard 24H2 (build 26100)"
        );
    }
}
