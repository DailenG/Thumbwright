//! Settings (FUNCTIONAL_SPEC.md section 8, the subset this product implements), with this
//! product's own key names. Portable mode: `Thumbwright.ini` next to the binaries (its
//! presence is the portable marker). Installed mode: values under `HKCU\Software\Thumbwright`.
//! Missing or unparsable values fall back to defaults. Saving writes every key.

use std::io;
use std::path::{Path, PathBuf};

use crate::limits;

pub const PORTABLE_INI: &str = "Thumbwright.ini";
pub const PRODUCT_KEY: &str = "Software\\Thumbwright";
const INI_SECTION: &str = "Settings";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuPreview {
    Off,
    InSubmenu,
    InContextMenu,
}

impl MenuPreview {
    fn code(self) -> u32 {
        match self {
            Self::Off => 0,
            Self::InSubmenu => 1,
            Self::InContextMenu => 2,
        }
    }
}

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
    item("convert_into", "Convert into"),
    item("resize", "Resize"),
    item("rotate", "Rotate / flip"),
    item("email", "Shrink for email"),
    item("compress", "Compress to size"),
    item("strip_meta", "Strip metadata (EXIF/GPS)"),
    item("copy", "Copy to clipboard"),
    item("copy_text", "Copy text (OCR)"),
    item("copy_data_uri", "Copy as data URI"),
    item("upload", "Upload (copy link)"),
    item("combine_pdf", "Combine into PDF"),
    item(
        "combine_pdf_searchable",
        "Combine into searchable PDF (OCR)",
    ),
    item("combine_cbz", "Combine into CBZ (comic)"),
    item("rename", "Rename"),
    item("files_to_folder", "Files to folder"),
    item("sort", "Sort into folders"),
    item("image_info", "Image info"),
    item("pick_color", "Pick color (eyedropper)"),
    item("wallpaper", "Set as wallpaper"),
    item("lock_screen", "Set as lock screen"),
    item("folder_icon", "Set as folder icon"),
    item("settings", "Settings"),
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
    pub menu_preview: MenuPreview,
    /// Menu item ids in the user's order; empty means the built-in order.
    pub menu_order: Vec<String>,
    /// Extensions (lower-case, no dot) the user turned off in "File types".
    pub disabled_types: Vec<String>,
    /// Menu verb ids the user hid.
    pub hidden_menu_items: Vec<String>,
    pub verbose_log: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            thumbnails_enabled: true,
            use_embedded_thumbs: true,
            max_file_mb: limits::DEFAULT_MAX_FILE_MB,
            max_thumb_px: limits::DEFAULT_MAX_THUMB_EDGE,
            jpeg_quality: 90,
            png_compression: 9,
            keep_metadata: true,
            preserve_file_date: false,
            menu_enabled: true,
            menu_all_file_types: false,
            menu_build_here: true,
            menu_preview: MenuPreview::InSubmenu,
            menu_order: Vec::new(),
            disabled_types: Vec::new(),
            hidden_menu_items: Vec::new(),
            verbose_log: false,
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
            "MenuPreview" => {
                self.menu_preview = match v {
                    "0" => MenuPreview::Off,
                    "2" => MenuPreview::InContextMenu,
                    _ => MenuPreview::InSubmenu,
                }
            }
            "MenuOrder" => self.menu_order = parse_list(v),
            "DisabledTypes" => self.disabled_types = parse_list(v),
            "HiddenMenuItems" => {
                self.hidden_menu_items = parse_list(v)
                    .into_iter()
                    .filter(|id| MENU_ITEMS.iter().all(|i| i.id != id || i.hideable))
                    .collect();
            }
            "VerboseLog" => self.verbose_log = parse_bool(v).unwrap_or(false),
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
            ("MenuPreview", Stored::Number(self.menu_preview.code())),
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
        let key = windows_registry::CURRENT_USER.open(key_path).ok()?;
        let mut s = Self::default();
        if let Ok(values) = key.values() {
            for (name, value) in values {
                let text = match value.ty() {
                    windows_registry::Type::U32 => u32::try_from(value).ok().map(|n| n.to_string()),
                    _ => String::try_from(value).ok(),
                };
                if let Some(text) = text {
                    s.apply(&name, &text);
                }
            }
        }
        Some(s)
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

    /// Loads settings for binaries installed in `dir`.
    pub fn load(dir: &Path) -> Self {
        if let Ok(text) = std::fs::read_to_string(portable_ini(dir)) {
            return Self::from_ini(&text);
        }
        #[cfg(windows)]
        if let Some(s) = Self::load_registry(PRODUCT_KEY) {
            return s;
        }
        Self::default()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_follow_spec() {
        let s = Settings::default();
        assert_eq!(s.max_file_bytes(), 4096 * 1024 * 1024);
        assert_eq!(s.max_thumb_px, 1024);
        assert!(s.menu_enabled && !s.menu_all_file_types && s.thumbnails_enabled);
        assert_eq!(s.menu_preview, MenuPreview::InSubmenu);
        assert!(s.keep_metadata, "Keep metadata defaults on");
        assert!(!Settings::from_ini("[Settings]\nKeepMetadata=0\n").keep_metadata);
        assert!(Settings::from_ini("[Settings]\nKeepMetadata=junk\n").keep_metadata);
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
        assert_eq!(s.menu_preview, MenuPreview::Off);
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
            max_file_mb: 250,
            max_thumb_px: 512,
            jpeg_quality: 72,
            png_compression: 4,
            keep_metadata: false,
            preserve_file_date: true,
            menu_enabled: false,
            menu_all_file_types: true,
            menu_build_here: false,
            menu_preview: MenuPreview::InContextMenu,
            menu_order: vec!["wallpaper".into(), "resize".into()],
            disabled_types: vec!["png".into(), "svg".into()],
            hidden_menu_items: vec!["email".into()],
            verbose_log: true,
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
        let order: Vec<String> = ["wallpaper", "bogus", "resize", "wallpaper"]
            .map(String::from)
            .into();
        let ids = ordered_menu_ids(&order);
        assert_eq!(&ids[..2], ["wallpaper", "resize"]);
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
        let s = Settings::from_ini("[Settings]\nHiddenMenuItems=settings,email,future_item\n");
        assert_eq!(s.hidden_menu_items, ["email", "future_item"]);
    }
}
