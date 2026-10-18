//! Format routing (FUNCTIONAL_SPEC.md section 3.4): maps a file to an ordered chain of
//! decoders. The first decoder that succeeds wins; a decoder failure falls through to the
//! next one only when [`DecodeError::falls_through`] allows it.

use std::borrow::Cow;

use image::DynamicImage;

use super::{DecodeError, Result};
use crate::limits;

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
    /// Design/package embedded previews: InDesign, Corel, EPS, Blender, ICNS, Aseprite, Paint.NET.
    Design = 70,
    /// Previews embedded in CAD, slicer and drawing files (G-code, DWG, DXF, CLIP).
    Cad = 80,
    /// STL / OBJ / PLY meshes shaded by the software rasteriser (triangle caps, deadline-checked).
    Mesh = 81,
    /// Legacy bitmap formats: XBM/XPM, Sun raster, MacPaint, IFF ILBM, PIX, CUT, TIM, VTF,
    /// PFM/PHM, PGX, SCR, ANI, CALS (pure Rust, bounded).
    Bitmap = 30,
    /// Legacy Office / OLE compound files: SummaryInformation thumbnail (DIB, PNG, JPEG).
    Ole = 50,
    /// Metafile (EMF/WMF) thumbnail of a legacy Office / OLE document, played by GDI.
    OleMetafile = 52,
    /// DjVu: first page rendered via `djvu-rs`, `TH44` thumbnail fallback.
    Djvu = 51,
    /// Video: representative frame via Media Foundation (OS codecs, C++).
    Video = 20,
    /// PDF / Illustrator: first page via `Windows.Data.Pdf` (OS renderer, C++).
    Pdf = 21,
    /// Windows metafiles (EMF/WMF and gzip-compressed forms): GDI playback.
    Metafile = 22,
    /// Mobipocket/Kindle (Palm database), FictionBook 2 and TAR comics: cover image.
    Ebook = 60,
    /// 7-Zip and RAR (incl. comic variants): contact sheet or first image. Worker: real
    /// decompression with attacker-chosen dictionary sizes.
    Archive = 61,
    /// Apple Live Photo bundle: ZIP holding a HEIC/JPEG still (HEIC needs WIC, so worker).
    Livp = 62,
    /// Scientific, medical, film, texture and network formats (FITS, DPX, DICOM, XCF, ...).
    Sci = 40,
    /// JPEG 2000 via `hayro-jpeg2000` (pure Rust, not cancellable: runs in the worker).
    J2k = 41,
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
    pub const ALL: [Decoder; 25] = [
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
        Self::Design,
        Self::Cad,
        Self::Mesh,
        Self::Bitmap,
        Self::Ole,
        Self::OleMetafile,
        Self::Djvu,
        Self::Video,
        Self::Pdf,
        Self::Metafile,
        Self::Ebook,
        Self::Archive,
        Self::Livp,
        Self::Sci,
        Self::J2k,
    ];

    pub fn placement(self) -> Placement {
        match self {
            Self::Svg
            | Self::Wic
            | Self::Audio
            | Self::Djvu
            | Self::Video
            | Self::Pdf
            | Self::Metafile
            | Self::Archive
            | Self::Livp
            | Self::J2k
            | Self::OleMetafile => Placement::Worker,
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
    /// rasterise directly at that size, raster decoders ignore it. Video frames are taken at
    /// the default offset; see [`Decoder::run_with`].
    pub fn run(self, data: &[u8], ext: &str, edge: u32) -> Result<DynamicImage> {
        self.run_with(data, ext, edge, limits::DEFAULT_VIDEO_PERCENT)
    }

    /// Like [`Decoder::run`], with the generic decoder parameter: the video frame offset in
    /// percent of the duration (0-100, larger values are clamped).
    pub fn run_with(
        self,
        data: &[u8],
        ext: &str,
        edge: u32,
        video_percent: u8,
    ) -> Result<DynamicImage> {
        match self {
            Self::Raster => super::raster::decode(data, Some(ext)),
            Self::Jxl => super::jxl::decode(data),
            Self::Pcx => super::pcx::decode(data),
            Self::Sgi => super::sgi::decode(data),
            Self::Psd => super::psd::extract(data),
            Self::Raw => super::raw::extract(data),
            Self::Audio => super::audio::extract(data, ext),
            Self::Zip => super::zip_preview::extract(data, ext),
            Self::Design => super::design::decode(data, ext),
            Self::Sci => super::sci::decode(data, ext),
            Self::J2k => super::j2k::decode(data, edge),
            Self::Svg => super::svg::render(data, edge),
            Self::Cad => super::cad::decode(data, ext),
            Self::Mesh => super::mesh::decode(data, ext),
            Self::Bitmap => super::bitmap::decode(data, ext),
            Self::Ole => super::ole::extract(data),
            Self::Djvu => super::djvu::render(data, edge),
            Self::Ebook => super::ebook::extract(data, ext),
            Self::Archive => super::archive::extract(data, ext),
            Self::Livp => super::livp::extract(data, edge),
            #[cfg(windows)]
            Self::Wic => super::wic::decode(data, edge),
            #[cfg(not(windows))]
            Self::Wic => Err(DecodeError::Unsupported),
            #[cfg(windows)]
            Self::Video => super::video::extract(data, ext, video_percent.min(100)),
            #[cfg(windows)]
            Self::Pdf => super::pdf::render(data, edge),
            #[cfg(windows)]
            Self::Metafile => super::metafile::render(data, edge),
            #[cfg(windows)]
            Self::OleMetafile => super::ole::extract_metafile(data)
                .and_then(|metafile| super::metafile::render(&metafile, edge)),
            #[cfg(not(windows))]
            Self::Video | Self::Pdf | Self::Metafile | Self::OleMetafile => {
                Err(DecodeError::Unsupported)
            }
        }
    }
}

/// Video containers decoded through Media Foundation (spec 5.6). Only containers whose
/// Media Foundation source opened real files on the reference OS (Windows 11 26340) are
/// routed. Not routed, because the OS cannot open them there (no source, or the MPEG-1/2
/// source fails with 0xC00D36E5 "operation on the current offset is not permitted",
/// even for file URLs, without the optional MPEG-2 video extension):
/// flv, mpg, mpeg, m1v, m2v, mpv, mp2v, m2p, vob.
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "m4v", "mov", "qt", "mkv", "webm", "avi", "wmv", "asf", "f4v", "3gp", "3g2", "ts",
    "m2ts", "mts", "ogv", "divx",
];
/// PDF and PDF-compatible Illustrator files.
pub const PDF_EXTENSIONS: &[&str] = &["pdf", "ai"];
/// Windows metafiles, plain and gzip-compressed.
pub const METAFILE_EXTENSIONS: &[&str] = &["emf", "emz", "wmf", "wmz", "emg"];

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
        (super::design::EXTENSIONS, &[Design]),
        (super::bitmap::EXTENSIONS, &[Bitmap]),
        (super::ole::EXTENSIONS, &[Ole, OleMetafile]),
        (super::djvu::EXTENSIONS, &[Djvu]),
        (VIDEO_EXTENSIONS, &[Video]),
        (PDF_EXTENSIONS, &[Pdf]),
        (METAFILE_EXTENSIONS, &[Metafile]),
        (super::ebook::EXTENSIONS, &[Ebook]),
        (super::archive::EXTENSIONS, &[Archive]),
        (super::livp::EXTENSIONS, &[Livp]),
        (super::sci::EXTENSIONS, &[Sci]),
        (super::j2k::EXTENSIONS, &[J2k]),
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

/// `chain` in the order to try it for the `UseEmbeddedThumbs` setting. By default a camera
/// RAW file's embedded JPEG preview ([`Decoder::Raw`]) is tried first because it is far
/// cheaper than a full decode. With `prefer_embedded` off, the full decoder that follows it
/// in the chain (the OS RAW codec) goes first and the embedded preview stays as the
/// fallback. Chains without an embedded-preview stage, and those where nothing else could
/// decode the file, are unchanged.
pub fn ordered(chain: &'static [Decoder], prefer_embedded: bool) -> Cow<'static, [Decoder]> {
    if prefer_embedded || !chain.contains(&Decoder::Raw) {
        return Cow::Borrowed(chain);
    }
    let (embedded, full): (Vec<Decoder>, Vec<Decoder>) =
        chain.iter().partition(|d| **d == Decoder::Raw);
    Cow::Owned(full.into_iter().chain(embedded).collect())
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
    fn embedded_preview_preference_orders_raw_chains_only() {
        let raw = chain_for_ext("cr2");
        assert_eq!(raw, &[Decoder::Raw, Decoder::Wic]);
        assert_eq!(&*ordered(raw, true), raw);
        assert_eq!(&*ordered(raw, false), &[Decoder::Wic, Decoder::Raw]);
        // Nothing else could decode these, so the setting cannot change them.
        for ext in ["crw", "psd", "docx", "png"] {
            let chain = chain_for_ext(ext);
            if !chain.is_empty() {
                assert_eq!(&*ordered(chain, false), chain, "{ext}");
            }
        }
        assert_eq!(&*ordered(&[Decoder::Raw], false), &[Decoder::Raw]);
    }

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
            .chain(super::super::design::EXTENSIONS)
            .chain(super::super::cad::EXTENSIONS)
            .chain(super::super::mesh::EXTENSIONS)
            .chain(super::super::bitmap::EXTENSIONS)
            .chain(super::super::ole::EXTENSIONS)
            .chain(super::super::djvu::EXTENSIONS)
            .chain(VIDEO_EXTENSIONS)
            .chain(PDF_EXTENSIONS)
            .chain(METAFILE_EXTENSIONS)
            .chain(super::super::ebook::EXTENSIONS)
            .chain(super::super::archive::EXTENSIONS)
            .chain(super::super::livp::EXTENSIONS)
            .chain(super::super::sci::EXTENSIONS)
            .chain(super::super::j2k::EXTENSIONS)
        {
            assert!(formats::lookup(ext).is_some(), "{ext} not in matrix");
        }
    }

    #[test]
    fn non_memory_safe_vector_and_audio_decoders_run_out_of_process() {
        assert_eq!(Decoder::Wic.placement(), Placement::Worker);
        assert_eq!(Decoder::Svg.placement(), Placement::Worker);
        assert_eq!(Decoder::Audio.placement(), Placement::Worker);
        assert_eq!(Decoder::Video.placement(), Placement::Worker);
        assert_eq!(Decoder::Pdf.placement(), Placement::Worker);
        assert_eq!(Decoder::Metafile.placement(), Placement::Worker);
        assert_eq!(Decoder::J2k.placement(), Placement::Worker);
        assert_eq!(Decoder::Raster.placement(), Placement::InProc);
        assert_eq!(Decoder::Djvu.placement(), Placement::Worker);
        assert_eq!(Decoder::Ole.placement(), Placement::InProc);
        assert_eq!(Decoder::OleMetafile.placement(), Placement::Worker);
    }

    #[test]
    fn video_pdf_and_metafile_extensions_route_to_their_workers() {
        for ext in VIDEO_EXTENSIONS {
            assert_eq!(chain_for_ext(ext), &[Decoder::Video], "{ext}");
        }
        assert_eq!(plan(Some("mp4"), b"\0\0\0\x18ftypmp42"), &[Decoder::Video]);
        assert_eq!(chain_for_ext("pdf"), &[Decoder::Pdf]);
        assert_eq!(chain_for_ext("ai"), &[Decoder::Pdf]);
        for ext in ["emf", "emz", "wmf", "wmz"] {
            assert_eq!(chain_for_ext(ext), &[Decoder::Metafile], "{ext}");
        }
        // Containers Media Foundation cannot open on the reference OS stay unrouted.
        for ext in [
            "flv", "mpg", "mpeg", "m1v", "m2v", "mpv", "mp2v", "m2p", "vob",
        ] {
            assert!(chain_for_ext(ext).is_empty(), "{ext}");
        }
        // Archives decompress with attacker-chosen dictionary sizes; Live Photo stills may
        // need WIC. Ebook containers are walked with bounds-checked Rust only.
        assert_eq!(Decoder::Archive.placement(), Placement::Worker);
        assert_eq!(Decoder::Livp.placement(), Placement::Worker);
        assert_eq!(Decoder::Ebook.placement(), Placement::InProc);
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
