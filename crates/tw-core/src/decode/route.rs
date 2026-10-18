//! Format routing (FUNCTIONAL_SPEC.md section 3.4): maps a file to an ordered chain of
//! decoders. The first decoder that succeeds wins; a decoder failure falls through to the
//! next one only when [`DecodeError::falls_through`] allows it.

use image::DynamicImage;

use super::{DecodeError, Result};

/// One decoder stage. The discriminant is the worker wire code (see `protocol`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Decoder {
    /// `image` crate raster codecs, content-sniffed.
    Raster = 1,
    /// JPEG XL via `jxl-oxide`.
    Jxl = 2,
    /// ZSoft PCX / multi-page DCX.
    Pcx = 3,
    /// Silicon Graphics RGB.
    Sgi = 4,
    /// Photoshop: embedded thumbnail resource or merged composite.
    Psd = 5,
    /// Camera RAW: largest embedded JPEG preview.
    Raw = 6,
    /// Audio: embedded cover art.
    Audio = 7,
    /// ZIP-based packages, documents, ebooks and archives: embedded preview or contact sheet.
    Zip = 8,
    /// SVG / SVGZ via resvg.
    Svg = 9,
    /// Windows Imaging Component (OS codecs, C++): HEIF, JPEG XR, AVIF, RAW extension.
    Wic = 10,
    /// Previews embedded in CAD, slicer and drawing files (G-code, DWG, DXF, CLIP).
    Cad = 80,
    /// STL / OBJ / PLY meshes shaded by the software rasteriser (triangle caps, deadline-checked).
    Mesh = 81,
}

/// Where a decoder may run (REQ-ISO-2 / REQ-ISO-3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// Memory-safe Rust with header-checked allocation and bounded recursion: runs in the
    /// thumbnail surrogate on a budgeted, panic-contained thread.
    InProc,
    /// Not memory-safe (OS C++ codecs), not cancellable (vector rendering), or able to
    /// overflow the stack on hostile input (audio tag parsing recurses into nested frames):
    /// runs in a Job-Object-limited worker process that the parent kills on timeout.
    Worker,
}

impl Decoder {
    pub const ALL: [Decoder; 12] = [
        Self::Raster,
        Self::Jxl,
        Self::Pcx,
        Self::Sgi,
        Self::Psd,
        Self::Raw,
        Self::Audio,
        Self::Zip,
        Self::Svg,
        Self::Wic,
        Self::Cad,
        Self::Mesh,
    ];

    pub fn placement(self) -> Placement {
        match self {
            Self::Svg | Self::Wic | Self::Audio => Placement::Worker,
            _ => Placement::InProc,
        }
    }

    pub fn code(self) -> u8 {
        self as u8
    }

    pub fn from_code(code: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|d| d.code() == code)
    }

    /// Runs this decoder. `edge` is the requested thumbnail edge; vector decoders
    /// rasterise directly at that size, raster decoders ignore it.
    pub fn run(self, data: &[u8], ext: &str, edge: u32) -> Result<DynamicImage> {
        match self {
            Self::Raster => super::raster::decode(data, Some(ext)),
            Self::Jxl => super::jxl::decode(data),
            Self::Pcx => super::pcx::decode(data),
            Self::Sgi => super::sgi::decode(data),
            Self::Psd => super::psd::extract(data),
            Self::Raw => super::raw::extract(data),
            Self::Audio => super::audio::extract(data, ext),
            Self::Zip => super::zip_preview::extract(data, ext),
            Self::Svg => super::svg::render(data, edge),
            Self::Cad => super::cad::decode(data, ext),
            Self::Mesh => super::mesh::decode(data, ext),
            #[cfg(windows)]
            Self::Wic => super::wic::decode(data, edge),
            #[cfg(not(windows))]
            Self::Wic => Err(DecodeError::Unsupported),
        }
    }
}

const WIC_ONLY: &[&str] = &[
    "heic", "heif", "hif", "heics", "heifs", "avci", "avif", "avifs", "jxr", "wdp", "hdp", "wmp",
];

/// Ordered tier chain for an extension (lower-case, no dot). Empty means "not handled":
/// such types are never registered, so Windows' own handler stays in charge.
pub fn chain_for_ext(ext: &str) -> &'static [Decoder] {
    use Decoder::*;
    if super::raster::format_for_ext(ext).is_some() {
        return &[Raster, Wic];
    }
    if WIC_ONLY.contains(&ext) {
        return &[Wic];
    }
    let single: &'static [(&'static [&'static str], &'static [Decoder])] = &[
        (&["svg", "svgz"], &[Svg]),
        (&["jxl"], &[Jxl]),
        (super::cad::EXTENSIONS, &[Cad]),
        (super::mesh::EXTENSIONS, &[Mesh]),
        (super::pcx::EXTENSIONS, &[Pcx]),
        (super::sgi::EXTENSIONS, &[Sgi]),
        (super::psd::EXTENSIONS, &[Psd]),
        (super::raw::EXTENSIONS, &[Raw, Wic]),
        (super::audio::EXTENSIONS, &[Audio]),
        (super::zip_preview::EXTENSIONS, &[Zip]),
    ];
    single
        .iter()
        .find(|(exts, _)| exts.contains(&ext))
        .map_or(&[], |(_, chain)| chain)
}

fn looks_like_svg(head: &[u8]) -> bool {
    let head = &head[..head.len().min(4096)];
    let text = String::from_utf8_lossy(head);
    let t = text.trim_start_matches('\u{feff}').trim_start();
    t.starts_with('<') && t.contains("<svg")
}

/// Tier chain for a file: the extension decides when it is known; otherwise the content
/// is sniffed (tier 0). `head` is the start of the file (a few KiB suffice).
pub fn plan(ext: Option<&str>, head: &[u8]) -> &'static [Decoder] {
    use Decoder::*;
    if let Some(chain) = ext.map(chain_for_ext).filter(|c| !c.is_empty()) {
        return chain;
    }
    if looks_like_svg(head) {
        return &[Svg];
    }
    if image::guess_format(head).is_ok() {
        return &[Raster, Wic];
    }
    &[]
}

/// Runs `chain` in order (all stages in the current process), stopping at the first
/// success or at a failure that must not fall through.
pub fn run_chain(chain: &[Decoder], data: &[u8], ext: &str, edge: u32) -> Result<DynamicImage> {
    let mut last = DecodeError::Unsupported;
    for d in chain {
        match d.run(data, ext, edge) {
            Ok(img) => return Ok(img),
            Err(e) if e.falls_through() => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats;

    #[test]
    fn wire_codes_round_trip() {
        for d in Decoder::ALL {
            assert_eq!(Decoder::from_code(d.code()), Some(d));
        }
        assert_eq!(Decoder::from_code(0), None);
    }

    #[test]
    fn every_routed_extension_is_in_the_spec_matrix() {
        for ext in WIC_ONLY
            .iter()
            .chain(super::super::pcx::EXTENSIONS)
            .chain(super::super::sgi::EXTENSIONS)
            .chain(super::super::psd::EXTENSIONS)
            .chain(super::super::raw::EXTENSIONS)
            .chain(super::super::audio::EXTENSIONS)
            .chain(super::super::zip_preview::EXTENSIONS)
            .chain(super::super::cad::EXTENSIONS)
            .chain(super::super::mesh::EXTENSIONS)
        {
            assert!(formats::lookup(ext).is_some(), "{ext} not in matrix");
        }
    }

    #[test]
    fn non_memory_safe_vector_and_audio_decoders_run_out_of_process() {
        assert_eq!(Decoder::Wic.placement(), Placement::Worker);
        assert_eq!(Decoder::Svg.placement(), Placement::Worker);
        assert_eq!(Decoder::Audio.placement(), Placement::Worker);
        assert_eq!(Decoder::Raster.placement(), Placement::InProc);
    }

    #[test]
    fn plan_prefers_extension_then_sniffs() {
        assert_eq!(plan(Some("heic"), b""), &[Decoder::Wic]);
        assert_eq!(plan(Some("svg"), b""), &[Decoder::Svg]);
        assert_eq!(plan(None, b"<?xml version='1.0'?><svg>"), &[Decoder::Svg]);
        assert_eq!(
            plan(None, b"\x89PNG\r\n\x1a\n"),
            &[Decoder::Raster, Decoder::Wic]
        );
        assert!(plan(Some("exe"), b"MZ").is_empty());
    }
}
