//! Supported file types (FUNCTIONAL_SPEC.md section 5).

mod table;

use std::collections::HashMap;
use std::sync::LazyLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Category {
    Raster,
    Design,
    CameraRaw,
    Document,
    Ebook,
    Video,
    Archive,
    Audio,
}

/// Where thumbnail pixels come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    FullDecode,
    EmbeddedPreview,
    CoverOrFirstPage,
    CoverArt,
    VideoFrame,
    ContactSheet,
}

/// Windows component the format depends on, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OsCodec {
    None,
    WicHeif,
    WicJpegXr,
    MediaFoundation,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Format {
    /// Lower-case, without the leading dot.
    pub ext: &'static str,
    pub description: &'static str,
    pub category: Category,
    pub source: Source,
    pub os_codec: OsCodec,
    pub convertible: bool,
}

pub fn all() -> &'static [Format] {
    &table::FORMATS
}

static BY_EXT: LazyLock<HashMap<&'static str, &'static Format>> =
    LazyLock::new(|| table::FORMATS.iter().map(|f| (f.ext, f)).collect());

/// Case-insensitive lookup; accepts `"png"`, `".PNG"`.
pub fn lookup(ext: &str) -> Option<&'static Format> {
    let ext = ext.strip_prefix('.').unwrap_or(ext);
    if ext.bytes().any(|b| b.is_ascii_uppercase()) {
        BY_EXT.get(ext.to_ascii_lowercase().as_str()).copied()
    } else {
        BY_EXT.get(ext).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(source: Source) -> usize {
        all().iter().filter(|f| f.source == source).count()
    }

    /// Totals reported by the reference diagnostic tool (spec section 5).
    #[test]
    fn matrix_matches_spec_totals() {
        assert_eq!(all().len(), 363);
        assert_eq!(BY_EXT.len(), 363, "extensions must be unique");
        assert_eq!(count(Source::FullDecode), 161);
        assert_eq!(count(Source::EmbeddedPreview), 97);
        assert_eq!(count(Source::CoverOrFirstPage), 58);
        assert_eq!(count(Source::VideoFrame), 26);
        assert_eq!(count(Source::CoverArt), 18);
        assert_eq!(count(Source::ContactSheet), 3);
    }

    #[test]
    fn extensions_are_canonical() {
        for f in all() {
            assert!(!f.ext.is_empty() && !f.ext.starts_with('.'), "{}", f.ext);
            assert!(!f.ext.bytes().any(|b| b.is_ascii_uppercase()), "{}", f.ext);
        }
    }

    #[test]
    fn lookup_is_case_and_dot_insensitive() {
        assert_eq!(lookup(".PNG").map(|f| f.ext), Some("png"));
        assert_eq!(lookup("Heic").map(|f| f.os_codec), Some(OsCodec::WicHeif));
        assert!(lookup("exe").is_none());
        assert!(!lookup("zip").unwrap().convertible);
    }
}
