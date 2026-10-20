//! Settings (FUNCTIONAL_SPEC.md section 8, the subset this product implements), with this
//! product's own key names. Portable mode: `Thumbwright.ini` next to the binaries (its
//! presence is the portable marker). Installed mode: values under `HKCU\Software\Thumbwright`.
//! Missing or unparsable values fall back to defaults. Saving writes every key.

use std::io;
use std::path::{Path, PathBuf};

use crate::formats::{self, Category};
use crate::limits;

pub const PORTABLE_INI: &str = "Thumbwright.ini";
pub const PRODUCT_KEY: &str = "Software\\Thumbwright";
/// Machine-wide first-run defaults (HKLM), written by the MSI properties `MENU` and `KIT`.
pub const MACHINE_DEFAULTS_KEY: &str = "Software\\Thumbwright\\Defaults";
/// The starter kits the installer's `KIT` property accepts, as stored in `StarterKit`.
pub const STARTER_KITS: [&str; 6] = [
    "photos",
    "paperwork",
    "design",
    "cad",
    "thumbnails",
    "everything",
];
const INI_SECTION: &str = "Settings";

/// One right-click menu entry that has a place in the item order and, unless it is
/// `settings` (always present, FUNCTIONAL_SPEC.md 6.3), a visibility key.
#[derive(Debug, PartialEq, Eq)]
pub struct MenuItem {
    /// Stored in `HiddenMenuItems` / `MenuOrder`.
    pub id: &'static str,
    pub label: &'static str,
    pub hideable: bool,
}

const fn item(id: &'static str, label: &'static str) -> MenuItem {
    MenuItem {
        id,
        label,
        hideable: !matches!(id.as_bytes(), b"settings"),
    }
}

/// Every reorderable top-level menu entry, in the default order. The shell menu tree
/// (`tw-shell/src/menu.rs`) is checked against this list by a test.
pub const MENU_ITEMS: &[MenuItem] = &[
    item("convert_into", "Convert to"),
    item("resize", "Resize"),
    item("rotate", "Rotate & flip"),
    item("make_smaller", "Make smaller"),
    item("copy_text", "Copy text from image"),
    item("copy", "Copy"),
    item("combine", "Combine into"),
    item("organise", "Organise"),
    item("image_info", "Details"),
    item("set_as", "Set as"),
    item("settings", "Thumbwright Workbench"),
];

/// Menu item ids in display order: the known ids of `order` first (duplicates and unknown
/// ids dropped), then every item `order` does not mention, in the default order.
pub fn ordered_menu_ids(order: &[String]) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::with_capacity(MENU_ITEMS.len());
    for want in order {
        if let Some(item) = MENU_ITEMS.iter().find(|i| i.id == want)
            && !out.contains(&item.id)
        {
            out.push(item.id);
        }
    }
    for item in MENU_ITEMS {
        if !out.contains(&item.id) {
            out.push(item.id);
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub thumbnails_enabled: bool,
    /// Prefer a camera-embedded thumbnail over decoding the full image.
    pub use_embedded_thumbs: bool,
    /// "Thumbnails in cloud folders" (spec 4.3): the cloud-folder class refuses every
    /// request while this is off.
    pub cloud_thumbnails: bool,
    pub max_file_mb: u64,
    pub max_thumb_px: u32,
    /// Conversion default: JPEG quality, 1-100.
    pub jpeg_quality: u8,
    /// Conversion default: PNG compression level, 0-9.
    pub png_compression: u8,
    /// Conversion default: carry EXIF and other metadata over.
    pub keep_metadata: bool,
    /// Conversion default: copy the source timestamps to saved files.
    pub preserve_file_date: bool,
    pub menu_enabled: bool,
    pub menu_all_file_types: bool,
    /// Offer "Build thumbnails here" on the folder menu.
    pub menu_build_here: bool,
    /// Show a preview of the file at the top of the menu.
    pub menu_preview: bool,
    /// Menu item ids in the user's order; empty means the built-in order.
    pub menu_order: Vec<String>,
    /// Extensions (lower-case, no dot) the user turned off in "File types".
    pub disabled_types: Vec<String>,
    /// Menu verb ids the user hid.
    pub hidden_menu_items: Vec<String>,
    /// Add a margin around pages of combined PDFs.
    pub pdf_margin: bool,
    pub verbose_log: bool,
    /// Video thumbnail frame offset in percent of the duration (0-100).
    pub video_frame_percent: u8,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            thumbnails_enabled: true,
            use_embedded_thumbs: true,
            cloud_thumbnails: false,
            max_file_mb: limits::DEFAULT_MAX_FILE_MB,
            max_thumb_px: limits::DEFAULT_MAX_THUMB_EDGE,
            jpeg_quality: 90,
            png_compression: 9,
            keep_metadata: true,
            preserve_file_date: false,
            menu_enabled: true,
            menu_all_file_types: false,
            menu_build_here: true,
            menu_preview: true,
            menu_order: Vec::new(),
            disabled_types: Vec::new(),
            hidden_menu_items: Vec::new(),
            pdf_margin: false,
            verbose_log: false,
            video_frame_percent: limits::DEFAULT_VIDEO_PERCENT,
        }
    }
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.trim() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn parse_list(v: &str) -> Vec<String> {
    v.split([',', ';', ' '])
        .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// A value as persisted: REG_DWORD (or a bare INI number) or REG_SZ text.
enum Stored {
    Number(u32),
    Text(String),
}

impl Stored {
    fn flag(on: bool) -> Self {
        Self::Number(u32::from(on))
    }

    fn list(items: &[String]) -> Self {
        Self::Text(items.join(","))
    }

    fn ini_text(&self) -> String {
        match self {
            Self::Number(n) => n.to_string(),
            Self::Text(t) => t.clone(),
        }
    }
}

impl Settings {
    pub fn max_file_bytes(&self) -> u64 {
        self.max_file_mb.saturating_mul(1024 * 1024)
    }

    pub fn type_enabled(&self, ext: &str) -> bool {
        !self.disabled_types.iter().any(|d| d == ext)
    }

    pub fn menu_item_visible(&self, id: &str) -> bool {
        !self.hidden_menu_items.iter().any(|h| h == id)
    }

    /// Applies one stored `key = value`; unknown keys and bad values are ignored.
    pub fn apply(&mut self, key: &str, value: &str) {
        let v = value.trim();
        match key.trim() {
            "ThumbnailsEnabled" => self.thumbnails_enabled = parse_bool(v).unwrap_or(true),
            "UseEmbeddedThumbs" => self.use_embedded_thumbs = parse_bool(v).unwrap_or(true),
            "CloudThumbnails" => self.cloud_thumbnails = parse_bool(v).unwrap_or(false),
            "MaxFileMB" => {
                if let Ok(n) = v.parse::<u64>() {
                    self.max_file_mb = n.clamp(1, limits::DEFAULT_MAX_FILE_MB);
                }
            }
            "MaxThumbPx" => {
                if let Ok(n) = v.parse::<u32>() {
                    self.max_thumb_px = n.clamp(16, limits::DEFAULT_MAX_THUMB_EDGE);
                }
            }
            "JpegQuality" => {
                if let Ok(n) = v.parse::<u8>() {
                    self.jpeg_quality = n.clamp(1, 100);
                }
            }
            "PngCompression" => {
                if let Ok(n) = v.parse::<u8>() {
                    self.png_compression = n.min(9);
                }
            }
            "KeepMetadata" => self.keep_metadata = parse_bool(v).unwrap_or(true),
            "PreserveFileDate" => self.preserve_file_date = parse_bool(v).unwrap_or(false),
            "MenuEnabled" => self.menu_enabled = parse_bool(v).unwrap_or(true),
            "MenuAllFileTypes" => self.menu_all_file_types = parse_bool(v).unwrap_or(false),
            "MenuBuildHere" => self.menu_build_here = parse_bool(v).unwrap_or(true),
            // Older builds stored 1 (in the submenu) and 2 (in the menu itself): both are "on".
            "MenuPreview" => self.menu_preview = v.trim() != "0",
            "MenuOrder" => self.menu_order = parse_list(v),
            "DisabledTypes" => self.disabled_types = parse_list(v),
            "HiddenMenuItems" => {
                self.hidden_menu_items = parse_list(v)
                    .into_iter()
                    .filter(|id| MENU_ITEMS.iter().all(|i| i.id != id || i.hideable))
                    .collect();
            }
            "PdfMargin" => self.pdf_margin = parse_bool(v).unwrap_or(false),
            "VerboseLog" => self.verbose_log = parse_bool(v).unwrap_or(false),
            "VideoFramePercent" => {
                if let Ok(n) = v.parse::<u64>() {
                    self.video_frame_percent = n.min(100) as u8;
                }
            }
            _ => {}
        }
    }

    /// Every setting as `(key, value)`, the exact inverse of [`Settings::apply`].
    fn stored(&self) -> Vec<(&'static str, Stored)> {
        vec![
            ("ThumbnailsEnabled", Stored::flag(self.thumbnails_enabled)),
            ("UseEmbeddedThumbs", Stored::flag(self.use_embedded_thumbs)),
            (
                "MaxFileMB",
                Stored::Number(self.max_file_mb.min(u64::from(u32::MAX)) as u32),
            ),
            ("MaxThumbPx", Stored::Number(self.max_thumb_px)),
            (
                "VideoFramePercent",
                Stored::Number(u32::from(self.video_frame_percent)),
            ),
            ("CloudThumbnails", Stored::flag(self.cloud_thumbnails)),
            ("JpegQuality", Stored::Number(u32::from(self.jpeg_quality))),
            (
                "PngCompression",
                Stored::Number(u32::from(self.png_compression)),
            ),
            ("KeepMetadata", Stored::flag(self.keep_metadata)),
            ("PreserveFileDate", Stored::flag(self.preserve_file_date)),
            ("MenuEnabled", Stored::flag(self.menu_enabled)),
            ("MenuAllFileTypes", Stored::flag(self.menu_all_file_types)),
            ("MenuBuildHere", Stored::flag(self.menu_build_here)),
            ("PdfMargin", Stored::flag(self.pdf_margin)),
            ("MenuPreview", Stored::flag(self.menu_preview)),
            ("MenuOrder", Stored::list(&self.menu_order)),
            ("DisabledTypes", Stored::list(&self.disabled_types)),
            ("HiddenMenuItems", Stored::list(&self.hidden_menu_items)),
            ("VerboseLog", Stored::flag(self.verbose_log)),
        ]
    }

    /// Parses the `[Settings]` section of an INI document.
    pub fn from_ini(text: &str) -> Self {
        let mut s = Self::default();
        let mut in_settings = false;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with([';', '#']) {
                continue;
            }
            if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                in_settings = section.trim().eq_ignore_ascii_case(INI_SECTION);
            } else if in_settings && let Some((k, v)) = line.split_once('=') {
                s.apply(k, v);
            }
        }
        s
    }

    /// `existing` INI text with its `[Settings]` section replaced by these settings. Other
    /// sections, comments and their order are kept; the new section takes the place of the
    /// first old one, or goes at the end when there was none.
    pub fn to_ini(&self, existing: &str) -> String {
        let mut section = format!("[{INI_SECTION}]\r\n");
        for (key, value) in self.stored() {
            section.push_str(&format!("{key}={}\r\n", value.ini_text()));
        }
        let mut out = String::with_capacity(existing.len() + section.len());
        let mut in_settings = false;
        let mut placed = false;
        for line in existing.lines() {
            let t = line.trim();
            if let Some(name) = t.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                in_settings = name.trim().eq_ignore_ascii_case(INI_SECTION);
                if in_settings {
                    if !placed {
                        out.push_str(&section);
                        placed = true;
                    }
                    continue;
                }
            }
            if !in_settings {
                out.push_str(line);
                out.push_str("\r\n");
            }
        }
        if !placed {
            if !out.is_empty() && !out.ends_with("\r\n\r\n") {
                out.push_str("\r\n");
            }
            out.push_str(&section);
        }
        out
    }

    /// Writes the INI at `path` atomically (staging file, then rename), keeping the file's
    /// other sections.
    pub fn save_ini(&self, path: &Path) -> io::Result<()> {
        let existing = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        let mut staging = path.as_os_str().to_owned();
        staging.push(".tmp");
        let staging = PathBuf::from(staging);
        std::fs::write(&staging, self.to_ini(&existing))?;
        std::fs::rename(&staging, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&staging);
        })
    }

    /// Loads the values stored under `HKCU\<key_path>`; `None` when the key does not exist.
    #[cfg(windows)]
    pub fn load_registry(key_path: &str) -> Option<Self> {
        Self::read_registry(windows_registry::CURRENT_USER, key_path).map(|(s, _)| s)
    }

    /// Reads `<root>\<key_path>` into settings and counts the values applied (or ignored as
    /// unknown); `None` when the key does not exist.
    #[cfg(windows)]
    fn read_registry(root: &windows_registry::Key, key_path: &str) -> Option<(Self, usize)> {
        let key = root.open(key_path).ok()?;
        let mut s = Self::default();
        let mut count = 0;
        if let Ok(values) = key.values() {
            for (name, value) in values {
                count += 1;
                if let Some(text) = registry_value_text(value) {
                    s.apply(&name, &text);
                }
            }
        }
        Some((s, count))
    }

    /// Writes every value under `HKCU\<key_path>`, creating the key. Numbers and flags are
    /// REG_DWORD, lists are REG_SZ.
    #[cfg(windows)]
    pub fn save_registry(&self, key_path: &str) -> io::Result<()> {
        let key = windows_registry::CURRENT_USER
            .create(key_path)
            .map_err(io::Error::other)?;
        for (name, value) in self.stored() {
            match value {
                Stored::Number(n) => key.set_u32(name, n),
                Stored::Text(t) => key.set_string(name, &t),
            }
            .map_err(io::Error::other)?;
        }
        Ok(())
    }

    /// Loads settings for binaries installed in `dir`: the portable INI, else the user's
    /// values under HKCU, else (a user who has saved nothing yet) the machine-wide defaults.
    pub fn load(dir: &Path) -> Self {
        if let Ok(text) = std::fs::read_to_string(portable_ini(dir)) {
            return Self::from_ini(&text);
        }
        Self::load_installed()
    }

    /// The installed (non-portable) settings: HKCU when the user has any value there, else the
    /// machine-wide defaults.
    #[cfg(windows)]
    fn load_installed() -> Self {
        match Self::read_registry(windows_registry::CURRENT_USER, PRODUCT_KEY) {
            Some((s, n)) if n > 0 => s,
            _ => Self::machine_defaults(),
        }
    }

    #[cfg(not(windows))]
    fn load_installed() -> Self {
        Self::default()
    }

    /// Machine-wide defaults for a user who has saved no settings yet: the starter kit the
    /// installer preselected (`KIT=<name>`), if any, with the values under
    /// `HKLM\Software\Thumbwright\Defaults` laid over it (see [`Settings::layered_defaults`]).
    /// The MSI writes `MenuEnabled=0` there for `MENU=0`; an administrator may add any other
    /// setting name. Absent key: the built-in defaults.
    #[cfg(windows)]
    pub fn machine_defaults() -> Self {
        let values: Vec<(String, String)> = windows_registry::LOCAL_MACHINE
            .open(MACHINE_DEFAULTS_KEY)
            .ok()
            .map(|key| {
                key.values()
                    .into_iter()
                    .flatten()
                    .filter_map(|(name, value)| Some((name, registry_value_text(value)?)))
                    .collect()
            })
            .unwrap_or_default();
        Self::layered_defaults(Self::machine_starter_kit(), values)
    }

    /// The machine-wide defaults as layers: the starter kit's settings (the built-in defaults
    /// without a kit), then every `key = value` of `values` applied on top, so an explicit
    /// machine value such as `MenuEnabled=0` (`MENU=0`) wins over the kit.
    pub fn layered_defaults<K, V>(
        kit: Option<&str>,
        values: impl IntoIterator<Item = (K, V)>,
    ) -> Self
    where
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut s = kit.map(starter_kit_defaults).unwrap_or_default();
        for (key, value) in values {
            s.apply(key.as_ref(), value.as_ref());
        }
        s
    }

    /// The starter kit the installer was told to preselect (`KIT=<name>`), as one of
    /// [`STARTER_KITS`]; `None` when none was given or the stored name is not a known kit.
    #[cfg(windows)]
    pub fn machine_starter_kit() -> Option<&'static str> {
        let key = windows_registry::LOCAL_MACHINE
            .open(MACHINE_DEFAULTS_KEY)
            .ok()?;
        starter_kit_name(&key.get_string("StarterKit").ok()?)
    }

    /// Saves settings for binaries installed in `dir`, to wherever [`Settings::load`] reads
    /// them: the portable INI when present, otherwise HKCU.
    #[cfg(windows)]
    pub fn save(&self, dir: &Path) -> io::Result<()> {
        if is_portable(dir) {
            self.save_ini(&portable_ini(dir))
        } else {
            self.save_registry(PRODUCT_KEY)
        }
    }
}

pub fn portable_ini(dir: &Path) -> PathBuf {
    dir.join(PORTABLE_INI)
}

/// Portable copies carry the INI marker next to their binaries (REQ-REG-6).
pub fn is_portable(dir: &Path) -> bool {
    portable_ini(dir).is_file()
}

/// The canonical name of the starter kit `text` names (case-insensitive), if it is one.
pub fn starter_kit_name(text: &str) -> Option<&'static str> {
    let text = text.trim();
    STARTER_KITS
        .into_iter()
        .find(|kit| kit.eq_ignore_ascii_case(text))
}

/// A registry value as the text [`Settings::apply`] parses: numbers in decimal, strings as is;
/// `None` for other types.
#[cfg(windows)]
fn registry_value_text(value: windows_registry::Value) -> Option<String> {
    match value.ty() {
        windows_registry::Type::U32 => u32::try_from(value).ok().map(|n| n.to_string()),
        _ => String::try_from(value).ok(),
    }
}

/// Menu items each kit keeps; every other hideable item is hidden. `settings` is always kept.
const PHOTOS_MENU: &[&str] = &[
    "convert_into",
    "resize",
    "rotate",
    "make_smaller",
    "copy_text",
    "combine",
    "organise",
    "image_info",
];
const PAPERWORK_MENU: &[&str] = &["convert_into", "make_smaller", "copy_text", "combine"];
const DESIGN_MENU: &[&str] = &["convert_into", "resize", "copy", "set_as"];

/// The settings a starter kit starts a user with (spec: UX review 4.2): the built-in defaults
/// with `disabled_types` (every type Thumbwright handles whose family the kit does not list)
/// and `hidden_menu_items` (every hideable menu item the kit does not keep) set, or the whole
/// menu turned off. `kit` is matched like [`starter_kit_name`]; an unknown name, and
/// `everything`, give the plain defaults.
///
/// | kit | families on | menu |
/// |---|---|---|
/// | `photos` | CameraRaw, Raster, Video | convert_into, resize, rotate, make_smaller, copy_text, combine, organise, image_info, settings |
/// | `paperwork` | Document, Ebook (incl. CBZ/CBR), Raster | convert_into, make_smaller, copy_text, combine, settings |
/// | `design` | Design, Raster | convert_into, resize, copy, set_as, settings |
/// | `cad` | Design | off |
/// | `thumbnails` | all | off |
/// | `everything` | all | full |
pub fn starter_kit_defaults(kit: &str) -> Settings {
    let mut s = Settings::default();
    let (families, menu): (&[Category], Option<&[&str]>) = match starter_kit_name(kit) {
        Some("photos") => (
            &[Category::CameraRaw, Category::Raster, Category::Video],
            Some(PHOTOS_MENU),
        ),
        Some("paperwork") => (
            &[Category::Document, Category::Ebook, Category::Raster],
            Some(PAPERWORK_MENU),
        ),
        Some("design") => (&[Category::Design, Category::Raster], Some(DESIGN_MENU)),
        Some("cad") => (&[Category::Design], None),
        Some("thumbnails") => (ALL_FAMILIES, None),
        _ => return s,
    };
    s.disabled_types = formats::all()
        .iter()
        .filter(|f| !families.contains(&f.category))
        .filter(|f| !crate::decode::route::chain_for_ext(f.ext).is_empty())
        .map(|f| f.ext.to_string())
        .collect();
    match menu {
        Some(keep) => {
            s.hidden_menu_items = MENU_ITEMS
                .iter()
                .filter(|i| i.hideable && !keep.contains(&i.id))
                .map(|i| i.id.to_string())
                .collect();
        }
        None => s.menu_enabled = false,
    }
    s
}

const ALL_FAMILIES: &[Category] = &[
    Category::Raster,
    Category::Design,
    Category::CameraRaw,
    Category::Document,
    Category::Ebook,
    Category::Video,
    Category::Archive,
    Category::Package,
    Category::Audio,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_follow_spec() {
        let s = Settings::default();
        assert_eq!(s.max_file_bytes(), 4096 * 1024 * 1024);
        assert_eq!(s.max_thumb_px, 1024);
        assert!(s.menu_enabled && !s.menu_all_file_types && s.thumbnails_enabled);
        assert!(
            !s.cloud_thumbnails,
            "cloud folders are off by default (spec 4.3)"
        );
        assert!(s.menu_preview);
        assert!(s.keep_metadata, "Keep metadata defaults on");
        assert!(!Settings::from_ini("[Settings]\nKeepMetadata=0\n").keep_metadata);
        assert!(Settings::from_ini("[Settings]\nKeepMetadata=junk\n").keep_metadata);
        assert_eq!(s.video_frame_percent, 30);
    }

    #[test]
    fn ini_overrides_only_settings_section() {
        let s = Settings::from_ini(
            "[Other]\nMenuEnabled=0\n[Settings]\nMaxThumbPx = 512\nDisabledTypes=.PNG, svg\n\
             MenuPreview=0\nMaxFileMB=junk\n",
        );
        assert!(s.menu_enabled);
        assert_eq!(s.max_thumb_px, 512);
        assert_eq!(s.disabled_types, ["png", "svg"]);
        assert!(!s.type_enabled("png") && s.type_enabled("jpg"));
        assert!(!s.menu_preview);
        assert_eq!(s.max_file_mb, 4096);
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let s = Settings::from_ini("[Settings]\nMaxThumbPx=99999\nMaxFileMB=0\n");
        assert_eq!(s.max_thumb_px, 1024);
        assert_eq!(s.max_file_mb, 1);
    }

    #[test]
    fn conversion_defaults_are_clamped() {
        let s = Settings::from_ini("[Settings]\nJpegQuality=0\nPngCompression=15\n");
        assert_eq!((s.jpeg_quality, s.png_compression), (1, 9));
        let s = Settings::from_ini("[Settings]\nJpegQuality=250\nPngCompression=3\n");
        assert_eq!((s.jpeg_quality, s.png_compression), (100, 3));
        let s = Settings::from_ini("[Settings]\nJpegQuality=300\nPngCompression=-1\n");
        assert_eq!(
            (s.jpeg_quality, s.png_compression),
            (90, 9),
            "values that do not parse keep the default"
        );
    }

    /// Every field set away from its default, so a key missing from save or load shows.
    fn non_default() -> Settings {
        Settings {
            thumbnails_enabled: false,
            use_embedded_thumbs: false,
            cloud_thumbnails: true,
            max_file_mb: 250,
            max_thumb_px: 512,
            jpeg_quality: 72,
            png_compression: 4,
            keep_metadata: false,
            preserve_file_date: true,
            menu_enabled: false,
            menu_all_file_types: true,
            menu_build_here: false,
            pdf_margin: true,
            menu_preview: false,
            menu_order: vec!["set_as".into(), "resize".into()],
            disabled_types: vec!["png".into(), "svg".into()],
            hidden_menu_items: vec!["make_smaller".into()],
            verbose_log: true,
            video_frame_percent: 75,
        }
    }

    #[test]
    fn ini_round_trip_keeps_every_field() {
        let s = non_default();
        assert_eq!(Settings::from_ini(&s.to_ini("")), s);
        let d = Settings::default();
        assert_eq!(Settings::from_ini(&d.to_ini("")), d);
    }

    #[test]
    fn to_ini_replaces_only_the_settings_section() {
        let existing = "; keep me\r\n[Other]\r\nA=1\r\n[settings]\r\nMaxThumbPx=64\r\nStale=1\r\n\
                        [Tail]\r\nB=2\r\n[Settings]\r\nMenuEnabled=0\r\n";
        let out = non_default().to_ini(existing);
        assert_eq!(out.matches("[Settings]").count(), 1, "{out}");
        assert!(
            !out.contains("Stale=") && !out.contains("[settings]"),
            "{out}"
        );
        let (before, after) = out.split_once("[Settings]").unwrap();
        assert_eq!(before, "; keep me\r\n[Other]\r\nA=1\r\n");
        assert!(after.ends_with("[Tail]\r\nB=2\r\n"), "{out}");
        assert_eq!(Settings::from_ini(&out), non_default());
    }

    #[test]
    fn to_ini_appends_a_section_after_foreign_content() {
        let out = Settings::default().to_ini("[Other]\r\nA=1\r\n");
        assert!(
            out.starts_with("[Other]\r\nA=1\r\n\r\n[Settings]\r\n"),
            "{out}"
        );
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "tw-config-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn portable_save_is_read_back_by_load_and_leaves_no_staging_file() {
        let dir = TempDir::new("portable");
        std::fs::write(portable_ini(&dir.0), "[Other]\r\nA=1\r\n").unwrap();
        assert!(is_portable(&dir.0));
        non_default().save(&dir.0).unwrap();
        assert_eq!(Settings::load(&dir.0), non_default());
        let text = std::fs::read_to_string(portable_ini(&dir.0)).unwrap();
        assert!(text.contains("[Other]\r\nA=1\r\n"), "{text}");
        let names: Vec<_> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [PORTABLE_INI], "staging file must be renamed away");
    }

    #[test]
    fn registry_save_is_read_back() {
        let key = format!(
            "Software\\ThumbwrightTest-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        assert_eq!(Settings::load_registry(&key), None);
        non_default().save_registry(&key).unwrap();
        let loaded = Settings::load_registry(&key);
        // A later save replaces every value, including lists that became empty.
        Settings::default().save_registry(&key).unwrap();
        let reset = Settings::load_registry(&key);
        let _ = windows_registry::CURRENT_USER.remove_tree(&key);
        assert_eq!(loaded, Some(non_default()));
        assert_eq!(reset, Some(Settings::default()));
    }

    #[test]
    fn menu_order_lists_unknown_duplicate_and_missing_ids_sanely() {
        let order: Vec<String> = ["set_as", "bogus", "resize", "set_as"]
            .map(String::from)
            .into();
        let ids = ordered_menu_ids(&order);
        assert_eq!(&ids[..2], ["set_as", "resize"]);
        assert_eq!(ids.len(), MENU_ITEMS.len());
        assert_eq!(
            &ids[2..4],
            ["convert_into", "rotate"],
            "rest in default order"
        );
        let default: Vec<_> = MENU_ITEMS.iter().map(|i| i.id).collect();
        assert_eq!(ordered_menu_ids(&[]), default);
    }

    #[test]
    fn menu_items_are_unique_and_settings_cannot_be_hidden() {
        for (i, item) in MENU_ITEMS.iter().enumerate() {
            assert!(
                MENU_ITEMS[i + 1..].iter().all(|o| o.id != item.id),
                "{}",
                item.id
            );
            assert_eq!(item.hideable, item.id != "settings");
        }
        let s =
            Settings::from_ini("[Settings]\nHiddenMenuItems=settings,make_smaller,future_item\n");
        assert_eq!(s.hidden_menu_items, ["make_smaller", "future_item"]);
    }

    #[test]
    fn the_menu_preview_is_on_or_off_and_old_location_codes_mean_on() {
        let preview =
            |v: &str| Settings::from_ini(&format!("[Settings]\nMenuPreview={v}\n")).menu_preview;
        assert!(!preview("0"));
        // 1 (in the submenu) and 2 (in the menu itself) were the two "on" places.
        assert!(preview("1") && preview("2"));
        assert!(
            Settings::from_ini("[Settings]\n").menu_preview,
            "on by default"
        );
        // Saving writes the plain flag.
        let off = Settings {
            menu_preview: false,
            ..Settings::default()
        };
        assert!(off.to_ini("").contains("MenuPreview=0"));
        assert!(Settings::default().to_ini("").contains("MenuPreview=1"));
    }

    #[test]
    fn video_frame_percent_is_clamped_and_junk_is_ignored() {
        let f = |v: &str| Settings::from_ini(&format!("[Settings]\nVideoFramePercent={v}\n"));
        assert_eq!(f("0").video_frame_percent, 0);
        assert_eq!(f("75").video_frame_percent, 75);
        assert_eq!(f("100").video_frame_percent, 100);
        assert_eq!(f("101").video_frame_percent, 100);
        assert_eq!(f("99999999999").video_frame_percent, 100);
        assert_eq!(f("-5").video_frame_percent, 30);
        assert_eq!(f("abc").video_frame_percent, 30);
    }

    #[test]
    fn starter_kit_names_are_canonical_and_unknown_ones_rejected() {
        assert_eq!(starter_kit_name("photos"), Some("photos"));
        assert_eq!(starter_kit_name(" CAD "), Some("cad"));
        assert_eq!(starter_kit_name("Everything"), Some("everything"));
        assert_eq!(starter_kit_name(""), None);
        assert_eq!(starter_kit_name("photo"), None);
        assert_eq!(starter_kit_name("photos;calc"), None);
    }

    #[test]
    fn machine_default_values_apply_like_user_values() {
        // What the MSI writes for MENU=0: a REG_DWORD MenuEnabled of 0.
        let mut s = Settings::default();
        s.apply("MenuEnabled", "0");
        assert!(!s.menu_enabled);
        // The StarterKit value is not a setting and must not disturb anything.
        let mut t = Settings::default();
        t.apply("StarterKit", "photos");
        assert_eq!(t, Settings::default());
    }

    #[test]
    fn registry_values_are_counted_so_an_empty_key_counts_as_nothing_saved() {
        let key = format!(
            "Software\\ThumbwrightTest-config-count-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = windows_registry::CURRENT_USER;
        assert!(Settings::read_registry(root, &key).is_none());
        root.create(&key).unwrap();
        let empty = Settings::read_registry(root, &key);
        non_default().save_registry(&key).unwrap();
        let filled = Settings::read_registry(root, &key);
        let _ = root.remove_tree(&key);
        assert_eq!(empty.map(|(_, n)| n), Some(0));
        let (s, n) = filled.unwrap();
        assert_eq!(s, non_default());
        assert!(n > 0);
    }

    fn routable() -> Vec<&'static str> {
        formats::all()
            .iter()
            .map(|f| f.ext)
            .filter(|e| !crate::decode::route::chain_for_ext(e).is_empty())
            .collect()
    }

    fn hidden(s: &Settings) -> Vec<&str> {
        s.hidden_menu_items.iter().map(String::as_str).collect()
    }

    fn visible_items(s: &Settings) -> Vec<&'static str> {
        MENU_ITEMS
            .iter()
            .map(|i| i.id)
            .filter(|id| s.menu_item_visible(id))
            .collect()
    }

    fn enabled_categories(s: &Settings) -> Vec<Category> {
        ALL_FAMILIES
            .iter()
            .copied()
            .filter(|c| {
                formats::all()
                    .iter()
                    .filter(|f| {
                        f.category == *c && !crate::decode::route::chain_for_ext(f.ext).is_empty()
                    })
                    .all(|f| s.type_enabled(f.ext))
            })
            .collect()
    }

    #[test]
    fn every_kit_name_has_a_shape_and_everything_is_the_default() {
        assert_eq!(starter_kit_defaults("everything"), Settings::default());
        assert_eq!(starter_kit_defaults("no-such-kit"), Settings::default());
        assert_eq!(
            starter_kit_defaults(" Photos "),
            starter_kit_defaults("photos")
        );
        for kit in STARTER_KITS.into_iter().filter(|k| *k != "everything") {
            assert_ne!(starter_kit_defaults(kit), Settings::default(), "{kit}");
        }
    }

    #[test]
    fn kit_menu_ids_all_exist_and_settings_stays() {
        for keep in [PHOTOS_MENU, PAPERWORK_MENU, DESIGN_MENU] {
            for id in keep {
                assert!(
                    MENU_ITEMS.iter().any(|i| i.id == *id),
                    "unknown menu id {id}"
                );
            }
        }
        for kit in ["photos", "paperwork", "design"] {
            let s = starter_kit_defaults(kit);
            assert!(s.menu_enabled && s.menu_item_visible("settings"), "{kit}");
            assert!(
                s.hidden_menu_items
                    .iter()
                    .all(|h| MENU_ITEMS.iter().any(|i| i.id == h && i.hideable)),
                "{kit}"
            );
        }
    }

    #[test]
    fn photos_kit_keeps_raw_and_heic_and_drops_documents() {
        let s = starter_kit_defaults("photos");
        assert!(s.type_enabled("cr3") && s.type_enabled("heic") && s.type_enabled("png"));
        assert!(s.type_enabled("mp4"));
        assert!(!s.type_enabled("pdf") && !s.type_enabled("dwg") && !s.type_enabled("zip"));
        assert_eq!(
            enabled_categories(&s),
            [Category::Raster, Category::CameraRaw, Category::Video]
        );
        assert_eq!(hidden(&s), ["copy", "set_as"]);
        assert!(s.menu_enabled);
    }

    #[test]
    fn paperwork_kit_is_documents_ebooks_comics_and_scans() {
        let s = starter_kit_defaults("paperwork");
        assert!(s.type_enabled("pdf") && s.type_enabled("epub"));
        assert!(s.type_enabled("cbz") && s.type_enabled("cbr") && s.type_enabled("png"));
        assert!(!s.type_enabled("cr3") && !s.type_enabled("dwg") && !s.type_enabled("mp4"));
        assert_eq!(
            visible_items(&s),
            [
                "convert_into",
                "make_smaller",
                "copy_text",
                "combine",
                "settings"
            ]
        );
    }

    #[test]
    fn design_kit_is_design_and_raster() {
        let s = starter_kit_defaults("design");
        assert!(s.type_enabled("psd") && s.type_enabled("svg") && s.type_enabled("png"));
        assert!(!s.type_enabled("pdf") && !s.type_enabled("cr3"));
        assert_eq!(
            visible_items(&s),
            ["convert_into", "resize", "copy", "set_as", "settings"]
        );
    }

    #[test]
    fn cad_kit_is_design_thumbnails_with_the_menu_off() {
        let s = starter_kit_defaults("cad");
        assert!(!s.menu_enabled);
        assert!(s.type_enabled("dwg") && s.type_enabled("dxf"));
        assert!(!s.type_enabled("png") && !s.type_enabled("pdf"));
        assert_eq!(enabled_categories(&s), [Category::Design]);
        assert!(s.hidden_menu_items.is_empty());
    }

    #[test]
    fn thumbnails_kit_turns_the_menu_off_and_every_routable_type_on() {
        let s = starter_kit_defaults("thumbnails");
        assert!(!s.menu_enabled);
        assert!(s.disabled_types.is_empty());
        assert!(routable().into_iter().all(|e| s.type_enabled(e)));
        assert_eq!(enabled_categories(&s), ALL_FAMILIES);
    }

    #[test]
    fn machine_values_are_laid_over_the_kit_and_menu_zero_wins() {
        let none: [(&str, &str); 0] = [];
        assert_eq!(Settings::layered_defaults(None, none), Settings::default());
        assert_eq!(
            Settings::layered_defaults(Some("photos"), none),
            starter_kit_defaults("photos")
        );
        // MENU=0 plus KIT=photos: the kit's types stay, the menu is off.
        let s = Settings::layered_defaults(
            Some("photos"),
            [("StarterKit", "photos"), ("MenuEnabled", "0")],
        );
        assert!(!s.menu_enabled);
        assert_eq!(
            s.disabled_types,
            starter_kit_defaults("photos").disabled_types
        );
        assert_eq!(
            s.hidden_menu_items,
            starter_kit_defaults("photos").hidden_menu_items
        );
        // Any stored machine value wins over the kit, also an administrator's MenuEnabled=1
        // over a kit that turns the menu off.
        let off = Settings::layered_defaults(Some("thumbnails"), [("MenuEnabled", "1")]);
        assert!(off.menu_enabled);
        // An administrator's DisabledTypes replaces the kit's list.
        let s = Settings::layered_defaults(Some("photos"), [("DisabledTypes", "png")]);
        assert_eq!(s.disabled_types, ["png"]);
    }
}
