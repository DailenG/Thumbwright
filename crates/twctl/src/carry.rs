//! Metadata that travels with the pixels of a conversion: the EXIF block, the ICC colour
//! profile and the XMP packet.
//!
//! [`Metadata::read`] pulls them out of a JPEG, PNG, WebP or TIFF source (JPEG `APP1`/`APP2`,
//! PNG `eXIf`/`iCCP`/`iTXt`, WebP `EXIF`/`ICCP`/`XMP ` chunks, TIFF IFD0 and its EXIF and
//! GPS IFDs plus tags 34675 and 700). [`prepare`] turns them into what a given output format
//! can hold for the pixels actually written:
//!
//! - EXIF and XMP are rewritten for pixels that were already oriented and possibly resized
//!   (Orientation 1, pixel dimensions follow the image).
//! - The ICC profile is written byte for byte and the pixels are **not** colour converted,
//!   so it is only kept when its data colour space is the one the pixels are written in
//!   (RGB, or grey for grey PNG/TIFF output). A CMYK profile, or a grey profile for pixels
//!   the writer makes RGB, is dropped with a note instead of mislabelling the colours.
//! - AVIF cannot embed an ICC profile or XMP and JPEG XL cannot embed an ICC profile with the
//!   writers used here; those are dropped with a note.
//!
//! The helpers here that parse hostile data (chunk sequences, lengths, compressed streams)
//! never allocate more than a fixed cap and never index unchecked.

use std::io::Read;

use image::DynamicImage;

use crate::exif;

/// Largest ICC profile read or written (a JPEG can spread 255 chunks of 65519 bytes).
pub const MAX_ICC: usize = 16 << 20;
/// Largest XMP packet read or written.
pub const MAX_XMP: usize = 8 << 20;
/// Payload of one JPEG `APP2` ICC chunk: a segment holds 65533 bytes, less the 14 byte
/// `ICC_PROFILE\0` + sequence header.
const ICC_CHUNK: usize = 65533 - 14;
const ICC_CHUNKS_MAX: usize = 255;
pub const JPEG_XMP_NS: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
const JPEG_XMP_EXT_NS: &[u8] = b"http://ns.adobe.com/xmp/extension/\0";
/// Largest XMP packet one JPEG `APP1` segment holds.
const MAX_JPEG_XMP: usize = 65533 - JPEG_XMP_NS.len();
/// Largest EXIF block an `APP1` segment holds (less length and `Exif\0\0`).
pub const MAX_JPEG_EXIF: usize = 65535 - 2 - 6;
const PNG_XMP_KEY: &[u8] = b"XML:com.adobe.xmp\0";

/// EXIF block, ICC profile and XMP packet of a source file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Metadata {
    /// TIFF-structured EXIF block (no `Exif\0\0` prefix).
    pub exif: Option<Vec<u8>>,
    /// Whole ICC profile, reassembled and header-checked.
    pub icc: Option<Vec<u8>>,
    /// XMP packet as stored.
    pub xmp: Option<Vec<u8>>,
    /// Things found in the source that were unusable (damaged chunk sequences, ...).
    pub notes: Vec<String>,
}

impl Metadata {
    /// Reads the metadata of `d`. `ext` (lower case) gates TIFF: only a `.tif`/`.tiff` file
    /// is read as a TIFF source (TIFF-based camera RAW files are not).
    pub fn read(ext: &str, d: &[u8]) -> Self {
        let mut notes = Vec::new();
        let (exif, icc, xmp) = if d.starts_with(&[0xFF, 0xD8]) {
            let segments = exif::jpeg_segments(d);
            (
                exif::from_container(d).map(<[u8]>::to_vec),
                jpeg_icc(&segments, &mut notes),
                jpeg_xmp(&segments, &mut notes),
            )
        } else if exif::is_png(d) {
            (
                exif::from_container(d).map(<[u8]>::to_vec),
                png_icc(d, &mut notes),
                png_xmp(d, &mut notes),
            )
        } else if exif::is_webp(d) {
            let chunk = |name: &[u8; 4]| {
                exif::webp_chunks(d)
                    .find(|(t, _)| t == name)
                    .map(|(_, b)| b.to_vec())
            };
            (
                exif::from_container(d).map(<[u8]>::to_vec),
                chunk(b"ICCP"),
                chunk(b"XMP "),
            )
        } else if matches!(ext, "tif" | "tiff") && exif::is_tiff(d) {
            (
                exif::from_tiff(d),
                exif::tiff_bytes(d, exif::TAG_ICC_PROFILE).map(<[u8]>::to_vec),
                exif::tiff_bytes(d, exif::TAG_XMP)
                    .map(|x| x.strip_suffix(&[0]).unwrap_or(x).to_vec()),
            )
        } else {
            (None, None, None)
        };
        let icc = icc.and_then(|p| {
            if valid_icc(&p) {
                Some(p)
            } else {
                notes.push(
                    "the source's ICC profile is damaged or truncated; not carried over".into(),
                );
                None
            }
        });
        let xmp = xmp.filter(|x| !x.is_empty()).and_then(|x| {
            if x.len() <= MAX_XMP {
                Some(x)
            } else {
                notes.push(format!(
                    "the source's XMP packet ({} bytes) is too large; not carried over",
                    x.len()
                ));
                None
            }
        });
        Self {
            exif,
            icc,
            xmp,
            notes,
        }
    }
    /// No EXIF, ICC profile or XMP (notes do not count).
    pub fn is_empty(&self) -> bool {
        self.exif.is_none() && self.icc.is_none() && self.xmp.is_none()
    }
}

/// An ICC profile worth carrying: header present, `acsp` signature, a declared size that
/// the data covers.
pub fn valid_icc(p: &[u8]) -> bool {
    let declared = p
        .get(..4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize);
    p.len() >= 128
        && p.len() <= MAX_ICC
        && p.get(36..40) == Some(b"acsp")
        && declared.is_some_and(|d| (128..=p.len()).contains(&d))
}

/// Data colour space signature of a (valid) profile: `RGB `, `GRAY`, `CMYK`, `Lab `, ...
fn icc_space(p: &[u8]) -> [u8; 4] {
    p.get(16..20)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .unwrap_or_default()
}

/// Zlib inflate of at most `cap` bytes; `None` for a damaged stream or a larger result.
fn inflate(z: &[u8], cap: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(z)
        .take(cap as u64 + 1)
        .read_to_end(&mut out)
        .ok()?;
    (out.len() <= cap).then_some(out)
}

#[cfg(test)]
fn deflate(data: &[u8]) -> Option<Vec<u8>> {
    use std::io::Write;
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(data).ok()?;
    z.finish().ok()
}

/// Reassembles the `APP2 ICC_PROFILE` chunks: every chunk must agree on the count, and the
/// sequence numbers must be exactly 1..=count, each once.
fn jpeg_icc(segments: &[(u8, &[u8])], notes: &mut Vec<String>) -> Option<Vec<u8>> {
    let mut chunks: Vec<(u8, u8, &[u8])> = segments
        .iter()
        .filter(|(m, p)| *m == 0xE2 && p.starts_with(b"ICC_PROFILE\0") && p.len() >= 14)
        .map(|(_, p)| (p[12], p[13], &p[14..]))
        .collect();
    let first = chunks.first()?;
    let count = usize::from(first.1);
    chunks.sort_by_key(|c| c.0);
    let consistent = count > 0
        && chunks.len() == count
        && chunks
            .iter()
            .enumerate()
            .all(|(i, c)| usize::from(c.0) == i + 1 && usize::from(c.1) == count);
    if !consistent {
        notes.push(
            "the source's ICC profile chunks are incomplete or inconsistent; not carried over"
                .into(),
        );
        return None;
    }
    let total: usize = chunks.iter().map(|c| c.2.len()).sum();
    if total > MAX_ICC {
        notes.push("the source's ICC profile is too large; not carried over".into());
        return None;
    }
    let mut out = Vec::with_capacity(total);
    chunks.iter().for_each(|c| out.extend_from_slice(c.2));
    Some(out)
}

fn jpeg_xmp(segments: &[(u8, &[u8])], notes: &mut Vec<String>) -> Option<Vec<u8>> {
    if segments
        .iter()
        .any(|(m, p)| *m == 0xE1 && p.starts_with(JPEG_XMP_EXT_NS))
    {
        notes.push("the source has Extended XMP (more than one APP1 segment); only the main packet is carried over".into());
    }
    segments
        .iter()
        .find(|(m, p)| *m == 0xE1 && p.starts_with(JPEG_XMP_NS))
        .map(|(_, p)| p[JPEG_XMP_NS.len()..].to_vec())
}

fn png_icc(d: &[u8], notes: &mut Vec<String>) -> Option<Vec<u8>> {
    let (_, body) = exif::png_chunks(d).find(|(t, _)| t == b"iCCP")?;
    // Profile name (1..=79 bytes), NUL, compression method 0, zlib stream.
    let nul = body.iter().position(|&b| b == 0);
    let stream = nul
        .filter(|n| (1..=79).contains(n) && body.get(n + 1) == Some(&0))
        .and_then(|n| body.get(n + 2..));
    let icc = stream.and_then(|z| inflate(z, MAX_ICC));
    if icc.is_none() {
        notes.push("the source's PNG iCCP chunk is malformed; not carried over".into());
    }
    icc
}

fn png_xmp(d: &[u8], notes: &mut Vec<String>) -> Option<Vec<u8>> {
    let (_, body) =
        exif::png_chunks(d).find(|(t, b)| t == b"iTXt" && b.starts_with(PNG_XMP_KEY))?;
    let text = itxt_text(&body[PNG_XMP_KEY.len()..]);
    if text.is_none() {
        notes.push("the source's PNG XMP chunk is malformed; not carried over".into());
    }
    text
}

/// The text of an `iTXt` chunk body after its keyword: compression flag, compression
/// method, language tag NUL, translated keyword NUL, text (inflated when flagged).
fn itxt_text(rest: &[u8]) -> Option<Vec<u8>> {
    let (&flag, rest) = rest.split_first()?;
    let (&method, rest) = rest.split_first()?;
    let rest = &rest[rest.iter().position(|&b| b == 0)? + 1..];
    let text = &rest[rest.iter().position(|&b| b == 0)? + 1..];
    match (flag, method) {
        (0, _) => Some(text.to_vec()),
        (1, 0) => inflate(text, MAX_XMP),
        _ => None,
    }
}

/// Output formats that can hold some of the metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Jpeg,
    Png,
    WebP,
    Tiff,
    Avif,
    Jxl,
}

impl Target {
    pub fn of(ext: &str) -> Option<Self> {
        Some(match ext {
            "jpg" | "jpeg" | "jpe" | "jfif" => Self::Jpeg,
            "png" => Self::Png,
            "webp" => Self::WebP,
            "tif" | "tiff" => Self::Tiff,
            "avif" => Self::Avif,
            "jxl" => Self::Jxl,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Jpeg => "JPEG",
            Self::Png => "PNG",
            Self::WebP => "WebP",
            Self::Tiff => "TIFF",
            Self::Avif => "AVIF",
            Self::Jxl => "JPEG XL",
        }
    }

    /// The ICC profile can be embedded.
    fn holds_icc(self) -> bool {
        !matches!(self, Self::Avif | Self::Jxl)
    }

    /// The XMP packet can be embedded.
    fn holds_xmp(self) -> bool {
        self != Self::Avif
    }
}

/// Metadata ready to be written next to the pixels of one output.
#[derive(Debug, Default, PartialEq)]
pub struct Outgoing {
    /// Normalised EXIF block (Orientation 1, dimensions of the written pixels).
    pub exif: Option<Vec<u8>>,
    pub icc: Option<Vec<u8>>,
    pub xmp: Option<Vec<u8>>,
    /// What was dropped, and why.
    pub notes: Vec<String>,
}

/// Fits `meta` to `target` for the pixels of `img` (the pixels as they will be written).
pub fn prepare(meta: &Metadata, target: Target, img: &DynamicImage) -> Outgoing {
    let dims = (img.width(), img.height());
    let mut out = Outgoing::default();
    if let Some(e) = &meta.exif {
        let mut fixed = exif::normalise(e, dims);
        if fixed.is_none() && target == Target::Tiff {
            // The TIFF writer copies only descriptive tags and recomputes the dimensions.
            fixed = Some(e.clone());
        }
        match fixed {
            Some(f) if target == Target::Jpeg && f.len() > MAX_JPEG_EXIF => {
                out.notes.push(format!(
                    "EXIF block of {} bytes does not fit a JPEG segment; not carried over",
                    f.len()
                ))
            }
            Some(f) => out.exif = Some(f),
            None => out.notes.push(
                "EXIF block has a layout that cannot be carried safely; not carried over".into(),
            ),
        }
    }
    if let Some(x) = &meta.xmp {
        let fixed = normalise_xmp(x, dims);
        if !target.holds_xmp() {
            out.notes.push(format!(
                "XMP not carried: the {} writer cannot embed it",
                target.name()
            ));
        } else if target == Target::Jpeg && fixed.len() > MAX_JPEG_XMP {
            out.notes.push(format!(
                "XMP packet of {} bytes does not fit a JPEG segment; not carried over",
                fixed.len()
            ));
        } else {
            out.xmp = Some(fixed);
        }
    }
    if let Some(p) = &meta.icc {
        let grey_pixels = matches!(target, Target::Png | Target::Tiff) && !img.color().has_color();
        let space = icc_space(p);
        let fits = if grey_pixels {
            &space == b"GRAY"
        } else {
            &space == b"RGB "
        };
        if !target.holds_icc() {
            out.notes.push(format!(
                "ICC profile not carried: the {} writer cannot embed one, so the colours are read as sRGB",
                target.name()
            ));
        } else if !fits {
            out.notes.push(format!(
                "ICC profile ({}) not carried: the pixels are written as {}, which it does not describe",
                String::from_utf8_lossy(&space).trim(),
                if grey_pixels { "grey" } else { "RGB" }
            ));
        } else if target == Target::Jpeg && p.len() > ICC_CHUNK * ICC_CHUNKS_MAX {
            out.notes
                .push("ICC profile does not fit a JPEG; not carried over".into());
        } else {
            out.icc = Some(p.clone());
        }
    }
    out
}

/// Rewrites the XMP fields that describe the pixel layout for pixels that were already
/// oriented and possibly resized to `dims`: `tiff:Orientation` becomes 1 and
/// `exif:PixelXDimension`, `exif:PixelYDimension`, `tiff:ImageWidth` and `tiff:ImageLength`
/// follow `dims`, in both attribute (`tiff:Orientation="6"`) and element
/// (`<tiff:Orientation>6</tiff:Orientation>`) form. Dimensions are only replaced when they
/// are plain numbers. XMP that uses other namespace prefixes for these properties is left
/// as is.
pub fn normalise_xmp(xmp: &[u8], dims: (u32, u32)) -> Vec<u8> {
    let (w, h) = (dims.0.to_string(), dims.1.to_string());
    let mut out = xmp.to_vec();
    for (key, value, force) in [
        ("tiff:Orientation", "1", true),
        ("exif:PixelXDimension", w.as_str(), false),
        ("exif:PixelYDimension", h.as_str(), false),
        ("tiff:ImageWidth", w.as_str(), false),
        ("tiff:ImageLength", h.as_str(), false),
    ] {
        out = rewrite_field(&out, key.as_bytes(), value.as_bytes(), force);
    }
    out
}

/// Replaces the value of every `key="v"`, `key='v'` and `<key>v</key>` in `xmp`; unless
/// `force`, only when `v` is a plain number.
fn rewrite_field(xmp: &[u8], key: &[u8], new: &[u8], force: bool) -> Vec<u8> {
    /// Longest value that is considered (a number, or a stray odd Orientation).
    const MAX_VALUE: usize = 32;
    let mut out = Vec::with_capacity(xmp.len());
    let mut i = 0;
    while i < xmp.len() {
        let rest = &xmp[i..];
        let span = if i > 0 && xmp[i - 1].is_ascii_whitespace() && rest.starts_with(key) {
            // key="value"
            match (rest.get(key.len()), rest.get(key.len() + 1)) {
                (Some(b'='), Some(&q @ (b'"' | b'\''))) => {
                    let start = key.len() + 2;
                    rest[start..]
                        .iter()
                        .take(MAX_VALUE + 1)
                        .position(|&b| b == q)
                        .map(|n| (start, start + n))
                }
                _ => None,
            }
        } else if rest.first() == Some(&b'<')
            && rest[1..].starts_with(key)
            && rest.get(key.len() + 1) == Some(&b'>')
        {
            // <key>value</key>
            let start = key.len() + 2;
            rest[start..]
                .iter()
                .take(MAX_VALUE + 1)
                .position(|&b| b == b'<')
                .map(|n| (start, start + n))
        } else {
            None
        };
        match span {
            Some((start, end))
                if force || (end > start && rest[start..end].iter().all(u8::is_ascii_digit)) =>
            {
                out.extend_from_slice(&rest[..start]);
                out.extend_from_slice(new);
                i += end;
            }
            _ => {
                out.push(xmp[i]);
                i += 1;
            }
        }
    }
    out
}

/// Inserts `xmp` as an `APP1` segment after the last leading `APPn` segment of `jpeg` (as
/// written by the `image` crate: JFIF, Exif, ICC, then the frame header).
pub fn jpeg_add_xmp(jpeg: &[u8], xmp: &[u8]) -> Option<Vec<u8>> {
    if !jpeg.starts_with(&[0xFF, 0xD8]) || xmp.len() > MAX_JPEG_XMP {
        return None;
    }
    let mut at = 2;
    while jpeg.get(at) == Some(&0xFF) && matches!(jpeg.get(at + 1), Some(0xE0..=0xEF)) {
        let len = usize::from(u16::from_be_bytes([*jpeg.get(at + 2)?, *jpeg.get(at + 3)?]));
        at = at.checked_add(2 + len).filter(|&e| e <= jpeg.len())?;
    }
    let len = u16::try_from(2 + JPEG_XMP_NS.len() + xmp.len()).ok()?;
    let mut out = Vec::with_capacity(jpeg.len() + usize::from(len) + 2);
    out.extend_from_slice(&jpeg[..at]);
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(JPEG_XMP_NS);
    out.extend_from_slice(xmp);
    out.extend_from_slice(&jpeg[at..]);
    Some(out)
}

fn png_chunk(out: &mut Vec<u8>, typ: &[u8; 4], body: &[u8]) -> Option<()> {
    out.extend_from_slice(&u32::try_from(body.len()).ok()?.to_be_bytes());
    out.extend_from_slice(typ);
    out.extend_from_slice(body);
    let mut crc = flate2::Crc::new();
    crc.update(typ);
    crc.update(body);
    out.extend_from_slice(&crc.sum().to_be_bytes());
    Some(())
}

/// Inserts `xmp` as an uncompressed `iTXt` chunk (`XML:com.adobe.xmp`) before the first
/// `IDAT` of `png`.
pub fn png_add_xmp(png: &[u8], xmp: &[u8]) -> Option<Vec<u8>> {
    if !exif::is_png(png) {
        return None;
    }
    let mut at = 8;
    loop {
        let len = u32::from_be_bytes(png.get(at..at + 4)?.try_into().ok()?) as usize;
        if png.get(at + 4..at + 8)? == b"IDAT" {
            break;
        }
        at = at.checked_add(12 + len).filter(|&e| e <= png.len())?;
    }
    let mut body = PNG_XMP_KEY.to_vec();
    // Not compressed, compression method 0, empty language tag, empty translated keyword.
    body.extend_from_slice(&[0, 0, 0, 0]);
    body.extend_from_slice(xmp);
    let mut out = Vec::with_capacity(png.len() + body.len() + 12);
    out.extend_from_slice(&png[..at]);
    png_chunk(&mut out, b"iTXt", &body)?;
    out.extend_from_slice(&png[at..]);
    Some(out)
}

/// Adds `xmp` as the last chunk of a lossless WebP written by the `image` crate (a plain
/// `VP8L` file gets a `VP8X` header first; one that already has it gets the XMP flag set).
pub fn webp_add_xmp(webp: &[u8], xmp: &[u8]) -> Option<Vec<u8>> {
    if !exif::is_webp(webp) {
        return None;
    }
    let chunks: Vec<([u8; 4], &[u8])> = exif::webp_chunks(webp).collect();
    let (first_typ, first) = *chunks.first()?;
    if chunks.iter().any(|(t, _)| t == b"XMP ") {
        return None;
    }
    let (mut vp8x, rest) = if &first_typ == b"VP8X" && first.len() == 10 {
        (first.to_vec(), &chunks[1..])
    } else if &first_typ == b"VP8L" && first.len() >= 5 && first[0] == 0x2F {
        // Signature, then 14 bits width-1, 14 bits height-1, 1 bit alpha-in-use.
        let bits = u32::from_le_bytes(first[1..5].try_into().ok()?);
        let (w, h) = ((bits & 0x3FFF), (bits >> 14) & 0x3FFF);
        let mut v = vec![0u8; 10];
        v[0] = if bits >> 28 & 1 == 1 { 0x10 } else { 0 };
        v[4..7].copy_from_slice(&w.to_le_bytes()[..3]);
        v[7..10].copy_from_slice(&h.to_le_bytes()[..3]);
        (v, &chunks[..])
    } else {
        return None;
    };
    vp8x[0] |= 0x04;
    let mut body = Vec::with_capacity(webp.len() + xmp.len() + 40);
    let mut chunk = |typ: &[u8; 4], data: &[u8]| -> Option<()> {
        body.extend_from_slice(typ);
        body.extend_from_slice(&u32::try_from(data.len()).ok()?.to_le_bytes());
        body.extend_from_slice(data);
        if data.len() % 2 == 1 {
            body.push(0);
        }
        Some(())
    };
    chunk(b"VP8X", &vp8x)?;
    for (typ, data) in rest {
        chunk(typ, data)?;
    }
    chunk(b"XMP ", xmp)?;
    let mut out = Vec::with_capacity(body.len() + 12);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&u32::try_from(body.len() + 4).ok()?.to_le_bytes());
    out.extend_from_slice(b"WEBP");
    out.extend_from_slice(&body);
    Some(out)
}

#[cfg(test)]
pub mod testutil {
    use super::*;

    /// A syntactically valid ICC profile of `len` bytes (header, then filler) whose data
    /// colour space is `space`.
    pub fn icc_profile(space: &[u8; 4], len: usize) -> Vec<u8> {
        let mut p = vec![0u8; len.max(132)];
        let size = (p.len() as u32).to_be_bytes();
        p[0..4].copy_from_slice(&size);
        p[8] = 4; // version 4
        p[12..16].copy_from_slice(b"mntr");
        p[16..20].copy_from_slice(space);
        p[20..24].copy_from_slice(b"XYZ ");
        p[36..40].copy_from_slice(b"acsp");
        for (i, b) in p.iter_mut().enumerate().skip(128) {
            *b = (i * 31 % 251) as u8;
        }
        p
    }

    /// An XMP packet with orientation 6 and dimensions 640x480 in both syntaxes.
    pub fn xmp_packet() -> Vec<u8> {
        br#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?><x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:tiff="http://ns.adobe.com/tiff/1.0/" xmlns:exif="http://ns.adobe.com/exif/1.0/" xmlns:dc="http://purl.org/dc/elements/1.1/" tiff:Orientation="6" exif:PixelXDimension="640"><exif:PixelYDimension>480</exif:PixelYDimension><dc:title>Thumbwright test</dc:title></rdf:Description></rdf:RDF></x:xmpmeta><?xpacket end="w"?>"#.to_vec()
    }

    /// Inserts `APP2 ICC_PROFILE` chunks and an `APP1` XMP segment after SOI.
    pub fn inject_icc_xmp_into_jpeg(
        jpeg: &[u8],
        icc: Option<&[u8]>,
        xmp: Option<&[u8]>,
    ) -> Vec<u8> {
        let mut seg = Vec::new();
        if let Some(icc) = icc {
            let n = icc.len().div_ceil(ICC_CHUNK);
            for (i, c) in icc.chunks(ICC_CHUNK).enumerate() {
                seg.extend_from_slice(&icc_segment(i as u8 + 1, n as u8, c));
            }
        }
        if let Some(x) = xmp {
            let mut payload = JPEG_XMP_NS.to_vec();
            payload.extend_from_slice(x);
            seg.extend_from_slice(&raw_segment(0xE1, &payload));
        }
        let mut out = jpeg[..2].to_vec();
        out.extend_from_slice(&seg);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    pub fn raw_segment(marker: u8, payload: &[u8]) -> Vec<u8> {
        let mut s = vec![0xFF, marker];
        s.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        s.extend_from_slice(payload);
        s
    }

    pub fn icc_segment(seq: u8, count: u8, chunk: &[u8]) -> Vec<u8> {
        let mut payload = b"ICC_PROFILE\0".to_vec();
        payload.extend_from_slice(&[seq, count]);
        payload.extend_from_slice(chunk);
        raw_segment(0xE2, &payload)
    }

    /// A PNG chunk with a correct CRC.
    pub fn png_chunk_bytes(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        png_chunk(&mut out, typ, body).unwrap();
        out
    }

    /// Inserts raw chunk bytes right after the IHDR of `png`.
    pub fn insert_after_ihdr(png: &[u8], chunks: &[u8]) -> Vec<u8> {
        let mut out = png[..33].to_vec();
        out.extend_from_slice(chunks);
        out.extend_from_slice(&png[33..]);
        out
    }

    pub fn iccp_chunk(icc: &[u8]) -> Vec<u8> {
        let mut body = b"test\0\0".to_vec();
        body.extend_from_slice(&deflate(icc).unwrap());
        png_chunk_bytes(b"iCCP", &body)
    }

    pub fn itxt_xmp_chunk(xmp: &[u8], compressed: bool) -> Vec<u8> {
        let mut body = PNG_XMP_KEY.to_vec();
        body.extend_from_slice(&[u8::from(compressed), 0, 0, 0]);
        if compressed {
            body.extend_from_slice(&deflate(xmp).unwrap());
        } else {
            body.extend_from_slice(xmp);
        }
        png_chunk_bytes(b"iTXt", &body)
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;
    use image::{ImageEncoder, RgbImage};

    fn rgb_jpeg() -> Vec<u8> {
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut out)
            .write_image(&[200u8; 8 * 8 * 3], 8, 8, image::ExtendedColorType::Rgb8)
            .unwrap();
        out
    }

    fn rgb_png() -> Vec<u8> {
        let mut out = Vec::new();
        image::codecs::png::PngEncoder::new(&mut out)
            .write_image(&[90u8; 6 * 4 * 3], 6, 4, image::ExtendedColorType::Rgb8)
            .unwrap();
        out
    }

    #[test]
    fn jpeg_icc_chunks_reassemble_in_any_order() {
        let icc = icc_profile(b"RGB ", 150_000);
        let jpeg = inject_icc_xmp_into_jpeg(&rgb_jpeg(), Some(&icc), None);
        let m = Metadata::read("jpg", &jpeg);
        assert_eq!(m.icc.as_deref(), Some(&icc[..]));
        assert!(m.notes.is_empty());
        // Three chunks, shuffled: sequence numbers decide, not file order.
        let chunks: Vec<&[u8]> = icc.chunks(ICC_CHUNK).collect();
        assert_eq!(chunks.len(), 3);
        let mut shuffled = rgb_jpeg()[..2].to_vec();
        for i in [2usize, 0, 1] {
            shuffled.extend(icc_segment(i as u8 + 1, 3, chunks[i]));
        }
        shuffled.extend_from_slice(&rgb_jpeg()[2..]);
        assert_eq!(
            Metadata::read("jpg", &shuffled).icc.as_deref(),
            Some(&icc[..])
        );
    }

    #[test]
    fn hostile_jpeg_icc_sequences_are_dropped_with_a_note() {
        let icc = icc_profile(b"RGB ", 150_000);
        let chunks: Vec<&[u8]> = icc.chunks(ICC_CHUNK).collect();
        let base = rgb_jpeg();
        let build = |segs: Vec<Vec<u8>>| {
            let mut v = base[..2].to_vec();
            segs.into_iter().for_each(|s| v.extend(s));
            v.extend_from_slice(&base[2..]);
            v
        };
        let cases = [
            // Missing the middle chunk.
            vec![icc_segment(1, 3, chunks[0]), icc_segment(3, 3, chunks[2])],
            // Duplicate sequence number.
            vec![
                icc_segment(1, 3, chunks[0]),
                icc_segment(1, 3, chunks[0]),
                icc_segment(3, 3, chunks[2]),
            ],
            // Disagreeing counts.
            vec![icc_segment(1, 2, chunks[0]), icc_segment(2, 3, chunks[1])],
            // Zero count, zero sequence.
            vec![icc_segment(0, 0, chunks[0])],
            // Only the first of three.
            vec![icc_segment(1, 3, chunks[0])],
        ];
        for segs in cases {
            let m = Metadata::read("jpg", &build(segs));
            assert!(m.icc.is_none());
            assert_eq!(m.notes.len(), 1, "{:?}", m.notes);
        }
        // A complete sequence whose bytes are not a profile is dropped by the header check.
        let junk = vec![0x55u8; 300];
        let m = Metadata::read("jpg", &build(vec![icc_segment(1, 1, &junk)]));
        assert!(m.icc.is_none() && m.notes.len() == 1);
        // A chunk header cut off after the signature.
        let m = Metadata::read("jpg", &build(vec![raw_segment(0xE2, b"ICC_PROFILE\0\x01")]));
        assert!(m.icc.is_none());
        // A segment that claims more bytes than the file has: the walk stops, nothing panics.
        let mut cut = base[..2].to_vec();
        cut.extend_from_slice(&[0xFF, 0xE2, 0xFF, 0xFF]);
        cut.extend_from_slice(b"ICC_PROFILE\0\x01\x01short");
        let m = Metadata::read("jpg", &cut);
        assert!(m.icc.is_none() && m.exif.is_none() && m.xmp.is_none());
    }

    #[test]
    fn declared_profile_size_beyond_the_data_is_rejected() {
        let mut p = icc_profile(b"RGB ", 4096);
        assert!(valid_icc(&p));
        p[0..4].copy_from_slice(&(5000u32).to_be_bytes());
        assert!(!valid_icc(&p));
        let mut p = icc_profile(b"RGB ", 4096);
        p[36] = b'x';
        assert!(!valid_icc(&p));
        assert!(!valid_icc(&[0u8; 127]));
    }

    #[test]
    fn jpeg_xmp_is_read_and_malformed_lengths_do_not_panic() {
        let xmp = xmp_packet();
        let jpeg = inject_icc_xmp_into_jpeg(&rgb_jpeg(), None, Some(&xmp));
        assert_eq!(Metadata::read("jpg", &jpeg).xmp.as_deref(), Some(&xmp[..]));
        // Length field smaller than the namespace: not XMP, and no slice panic.
        let mut bad = rgb_jpeg()[..2].to_vec();
        bad.extend_from_slice(&[0xFF, 0xE1, 0x00, 0x02]);
        bad.extend_from_slice(&rgb_jpeg()[2..]);
        assert!(Metadata::read("jpg", &bad).xmp.is_none());
        // Length of 0 and 1 (below the minimum of 2) end the walk.
        for len in [0u8, 1] {
            let mut bad = rgb_jpeg()[..2].to_vec();
            bad.extend_from_slice(&[0xFF, 0xE1, 0x00, len]);
            bad.extend_from_slice(JPEG_XMP_NS);
            assert!(Metadata::read("jpg", &bad).xmp.is_none());
        }
        // Declared length runs past the end of the file.
        let mut cut = rgb_jpeg()[..2].to_vec();
        cut.extend_from_slice(&[0xFF, 0xE1, 0xFF, 0xF0]);
        cut.extend_from_slice(JPEG_XMP_NS);
        cut.extend_from_slice(b"<x:xmpmeta/>");
        assert!(Metadata::read("jpg", &cut).xmp.is_none());
        // Extended XMP is announced, not silently lost.
        let mut ext = rgb_jpeg()[..2].to_vec();
        ext.extend(raw_segment(
            0xE1,
            &[JPEG_XMP_EXT_NS, &b"guidguid0123"[..]].concat(),
        ));
        ext.extend_from_slice(&rgb_jpeg()[2..]);
        assert!(Metadata::read("jpg", &ext).notes[0].contains("Extended XMP"));
    }

    #[test]
    fn png_icc_and_xmp_round_trip_and_hostile_chunks_are_dropped() {
        let icc = icc_profile(b"RGB ", 5000);
        let xmp = xmp_packet();
        for compressed in [false, true] {
            let png = insert_after_ihdr(
                &rgb_png(),
                &[iccp_chunk(&icc), itxt_xmp_chunk(&xmp, compressed)].concat(),
            );
            let m = Metadata::read("png", &png);
            assert_eq!(m.icc.as_deref(), Some(&icc[..]));
            assert_eq!(m.xmp.as_deref(), Some(&xmp[..]), "compressed={compressed}");
        }
        let bad = |chunk: Vec<u8>| Metadata::read("png", &insert_after_ihdr(&rgb_png(), &chunk));
        // Wrong compression method, no name, garbage zlib stream.
        let mut body = b"n\0\x07".to_vec();
        body.extend_from_slice(&deflate(&icc).unwrap());
        for body in [
            body,
            b"\0\0abc".to_vec(),
            b"n\0\0notzlib".to_vec(),
            b"n".to_vec(),
        ] {
            let m = bad(png_chunk_bytes(b"iCCP", &body));
            assert!(m.icc.is_none() && m.notes.len() == 1, "{body:?}");
        }
        // A deflate bomb inflating past the cap is refused, not allocated.
        let bomb = deflate(&vec![0u8; MAX_ICC + 1]).unwrap();
        let mut body = b"n\0\0".to_vec();
        body.extend_from_slice(&bomb);
        assert!(bad(png_chunk_bytes(b"iCCP", &body)).icc.is_none());
        // Truncated iTXt fields and unknown compression.
        for body in [
            [PNG_XMP_KEY, &[0u8][..]].concat(),
            [PNG_XMP_KEY, &[0u8, 0, b'e', b'n'][..]].concat(),
            [PNG_XMP_KEY, &[1u8, 5, 0, 0][..], &b"x"[..]].concat(),
            [PNG_XMP_KEY, &[1u8, 0, 0, 0][..], &b"notzlib"[..]].concat(),
        ] {
            let m = bad(png_chunk_bytes(b"iTXt", &body));
            assert!(m.xmp.is_none() && m.notes.len() == 1, "{body:?}");
        }
        // A chunk length beyond the file: the chunk walk ends.
        let mut cut = rgb_png()[..33].to_vec();
        cut.extend_from_slice(&[0x7F, 0xFF, 0xFF, 0xFF]);
        cut.extend_from_slice(b"iCCPxx");
        let m = Metadata::read("png", &cut);
        assert!(m.icc.is_none() && m.xmp.is_none());
    }

    #[test]
    fn webp_chunks_are_read() {
        let icc = icc_profile(b"RGB ", 700);
        let xmp = xmp_packet();
        let img = DynamicImage::ImageRgba8(image::RgbaImage::new(5, 3));
        let mut webp = Vec::new();
        let mut enc = image::codecs::webp::WebPEncoder::new_lossless(&mut webp);
        enc.set_icc_profile(icc.clone()).unwrap();
        enc.write_image(&img.to_rgba8(), 5, 3, image::ExtendedColorType::Rgba8)
            .unwrap();
        let webp = webp_add_xmp(&webp, &xmp).unwrap();
        let m = Metadata::read("webp", &webp);
        assert_eq!(m.icc.as_deref(), Some(&icc[..]));
        assert_eq!(m.xmp.as_deref(), Some(&xmp[..]));
        // A chunk whose length runs past the file ends the walk.
        let mut cut = webp[..12].to_vec();
        cut.extend_from_slice(b"ICCP\xFF\xFF\xFF\x7Fabc");
        assert!(Metadata::read("webp", &cut).icc.is_none());
    }

    #[test]
    fn preparing_for_an_output_drops_what_cannot_describe_the_pixels() {
        let rgb = DynamicImage::ImageRgb8(RgbImage::new(10, 8));
        let grey = DynamicImage::ImageLuma8(image::GrayImage::new(10, 8));
        let meta = |space: &[u8; 4]| Metadata {
            icc: Some(icc_profile(space, 2000)),
            xmp: Some(xmp_packet()),
            ..Metadata::default()
        };
        // RGB profile on RGB pixels is kept everywhere an ICC chunk exists.
        for t in [Target::Jpeg, Target::Png, Target::WebP, Target::Tiff] {
            let o = prepare(&meta(b"RGB "), t, &rgb);
            assert!(o.icc.is_some() && o.notes.is_empty(), "{t:?}");
        }
        // CMYK profile: the decoder converted to RGB, so the profile no longer applies.
        let o = prepare(&meta(b"CMYK"), Target::Jpeg, &rgb);
        assert!(o.icc.is_none());
        assert!(
            o.notes[0].contains("CMYK") && o.notes[0].contains("RGB"),
            "{:?}",
            o.notes
        );
        // Grey profile: kept for grey PNG/TIFF, dropped for JPEG/WebP which write RGB.
        assert!(prepare(&meta(b"GRAY"), Target::Png, &grey).icc.is_some());
        assert!(prepare(&meta(b"GRAY"), Target::Tiff, &grey).icc.is_some());
        assert!(prepare(&meta(b"GRAY"), Target::Jpeg, &grey).icc.is_none());
        assert!(prepare(&meta(b"GRAY"), Target::WebP, &grey).icc.is_none());
        assert!(prepare(&meta(b"GRAY"), Target::Png, &rgb).icc.is_none());
        assert!(prepare(&meta(b"RGB "), Target::Png, &grey).icc.is_none());
        // AVIF and JPEG XL say so.
        let o = prepare(&meta(b"RGB "), Target::Avif, &rgb);
        assert!(
            o.icc.is_none() && o.xmp.is_none() && o.notes.len() == 2,
            "{:?}",
            o.notes
        );
        let o = prepare(&meta(b"RGB "), Target::Jxl, &rgb);
        assert!(o.icc.is_none() && o.xmp.is_some() && o.notes.len() == 1);
        // XMP is rewritten for the written pixels.
        let o = prepare(&meta(b"RGB "), Target::Png, &rgb);
        let x = String::from_utf8(o.xmp.unwrap()).unwrap();
        assert!(x.contains(r#"tiff:Orientation="1""#) && x.contains(r#"PixelXDimension="10""#));
        assert!(x.contains("<exif:PixelYDimension>8</exif:PixelYDimension>"));
        // A profile or packet too large for one JPEG is dropped, not truncated.
        let big = Metadata {
            icc: Some(icc_profile(b"RGB ", ICC_CHUNK * ICC_CHUNKS_MAX + 1)),
            xmp: Some(vec![b'x'; MAX_JPEG_XMP + 1]),
            ..Metadata::default()
        };
        let o = prepare(&big, Target::Jpeg, &rgb);
        assert!(o.icc.is_none() && o.xmp.is_none() && o.notes.len() == 2);
    }

    #[test]
    fn xmp_orientation_and_dimensions_rewrite_in_both_syntaxes() {
        let out = String::from_utf8(normalise_xmp(&xmp_packet(), (1234, 56))).unwrap();
        assert!(out.contains(r#" tiff:Orientation="1""#));
        assert!(out.contains(r#" exif:PixelXDimension="1234""#));
        assert!(out.contains("<exif:PixelYDimension>56</exif:PixelYDimension>"));
        assert!(
            out.contains("<dc:title>Thumbwright test</dc:title>"),
            "other fields stay"
        );
        let element =
            b"<tiff:Orientation>8</tiff:Orientation><tiff:ImageWidth>9999</tiff:ImageWidth>";
        let out = normalise_xmp(element, (7, 3));
        assert_eq!(
            out,
            b"<tiff:Orientation>1</tiff:Orientation><tiff:ImageWidth>7</tiff:ImageWidth>"
        );
        // Single quotes, and an odd Orientation value is still forced to 1.
        assert_eq!(
            normalise_xmp(b"a tiff:Orientation='3 ' b", (1, 1)),
            b"a tiff:Orientation='1' b"
        );
        // A dimension that is not a plain number, a longer name, and an unterminated value
        // are left alone.
        let odd = br#" exif:PixelXDimension="12px" xtiff:Orientation="6" tiff:Orientation="6"#;
        assert_eq!(
            normalise_xmp(odd, (5, 5)),
            br#" exif:PixelXDimension="12px" xtiff:Orientation="6" tiff:Orientation="6"#
        );
        assert_eq!(normalise_xmp(b"", (1, 1)), b"");
        assert_eq!(
            normalise_xmp(b"tiff:Orientation=\"", (1, 1)),
            b"tiff:Orientation=\""
        );
    }

    #[test]
    fn writers_place_xmp_and_reject_foreign_input() {
        let xmp = xmp_packet();
        let jpeg = jpeg_add_xmp(&rgb_jpeg(), &xmp).unwrap();
        // After JFIF APP0, before the frame header.
        let segs = exif::jpeg_segments(&jpeg);
        assert_eq!(segs[0].0, 0xE0);
        assert_eq!(segs[1].0, 0xE1);
        assert_eq!(segs[2].0, 0xC0);
        assert_eq!(Metadata::read("jpg", &jpeg).xmp.as_deref(), Some(&xmp[..]));
        assert!(jpeg_add_xmp(&rgb_jpeg(), &vec![0; MAX_JPEG_XMP + 1]).is_none());
        assert!(jpeg_add_xmp(b"not a jpeg", &xmp).is_none());

        let png = png_add_xmp(&rgb_png(), &xmp).unwrap();
        let types: Vec<[u8; 4]> = exif::png_chunks(&png).map(|(t, _)| t).collect();
        let itxt = types.iter().position(|t| t == b"iTXt").unwrap();
        assert_eq!(&types[0], b"IHDR");
        assert_eq!(&types[itxt + 1], b"IDAT");
        assert_eq!(Metadata::read("png", &png).xmp.as_deref(), Some(&xmp[..]));
        // The decoder must still read the picture.
        assert_eq!(image::load_from_memory(&png).unwrap().width(), 6);
        assert!(png_add_xmp(b"not a png", &xmp).is_none());
        // No IDAT: refuses instead of looping or writing at a wrong offset.
        assert!(png_add_xmp(&rgb_png()[..33], &xmp).is_none());

        assert!(webp_add_xmp(b"RIFF\0\0\0\0WEBPVP8 \x04\0\0\0abcd", &xmp).is_none());
        assert!(webp_add_xmp(b"nope", &xmp).is_none());
    }
}
