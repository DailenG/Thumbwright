//! Settings (FUNCTIONAL_SPEC.md section 8, shell-relevant subset), with this product's own
//! key names. Portable mode: `Thumbwright.ini` next to the binaries (its presence is the
//! portable marker). Installed mode: values under `HKCU\Software\Thumbwright`.
//! Missing or unparsable values fall back to defaults.

use std::path::{Path, PathBuf};

use crate::limits;

pub const PORTABLE_INI: &str = "Thumbwright.ini";
pub const PRODUCT_KEY: &str = "Software\\Thumbwright";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuPreview {
    Off,
    InSubmenu,
    InContextMenu,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub thumbnails_enabled: bool,
    pub max_file_mb: u64,
    pub max_thumb_px: u32,
    pub menu_enabled: bool,
    pub menu_all_file_types: bool,
    pub menu_preview: MenuPreview,
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
            max_file_mb: limits::DEFAULT_MAX_FILE_MB,
            max_thumb_px: limits::DEFAULT_MAX_THUMB_EDGE,
            menu_enabled: true,
            menu_all_file_types: false,
            menu_preview: MenuPreview::InSubmenu,
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

impl Settings {
    pub fn max_file_bytes(&self) -> u64 {
        self.max_file_mb.saturating_mul(1024 * 1024)
    }

    pub fn type_enabled(&self, ext: &str) -> bool {
        !self.disabled_types.iter().any(|d| d == ext)
    }

    /// Applies one stored `key = value`; unknown keys and bad values are ignored.
    pub fn apply(&mut self, key: &str, value: &str) {
        let v = value.trim();
        match key.trim() {
            "ThumbnailsEnabled" => self.thumbnails_enabled = parse_bool(v).unwrap_or(true),
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
            "MenuEnabled" => self.menu_enabled = parse_bool(v).unwrap_or(true),
            "MenuAllFileTypes" => self.menu_all_file_types = parse_bool(v).unwrap_or(false),
            "MenuPreview" => {
                self.menu_preview = match v {
                    "0" => MenuPreview::Off,
                    "2" => MenuPreview::InContextMenu,
                    _ => MenuPreview::InSubmenu,
                }
            }
            "DisabledTypes" => self.disabled_types = parse_list(v),
            "HiddenMenuItems" => self.hidden_menu_items = parse_list(v),
            "VerboseLog" => self.verbose_log = parse_bool(v).unwrap_or(false),
            _ => {}
        }
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
                in_settings = section.trim().eq_ignore_ascii_case("Settings");
            } else if in_settings && let Some((k, v)) = line.split_once('=') {
                s.apply(k, v);
            }
        }
        s
    }

    /// Loads settings for binaries installed in `dir`.
    pub fn load(dir: &Path) -> Self {
        if let Ok(text) = std::fs::read_to_string(portable_ini(dir)) {
            return Self::from_ini(&text);
        }
        #[cfg(windows)]
        if let Some(s) = Self::from_registry(PRODUCT_KEY) {
            return s;
        }
        Self::default()
    }

    /// Reads the values under `HKCU\<subkey>`; `None` when the key does not exist.
    #[cfg(windows)]
    pub fn from_registry(subkey: &str) -> Option<Self> {
        let key = windows_registry::CURRENT_USER.open(subkey).ok()?;
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
}
