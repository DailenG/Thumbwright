//! Embedded cover art from audio tags (ID3v2, Vorbis comments / FLAC pictures, MP4
//! `covr`, APE), read with `lofty`, plus the two containers `lofty` cannot read: WMA (ASF
//! `WM/Picture` descriptors, parsed here with every size bounded by the bytes present) and
//! DSF (the ID3v2 tag the `DSD ` chunk points at, handed to `lofty`). The front cover
//! wins; otherwise the first picture.

use std::io::Cursor;

use image::DynamicImage;
use lofty::config::ParseOptions;
use lofty::file::{FileType, TaggedFileExt};
use lofty::picture::{Picture, PictureType};
use lofty::probe::Probe;

use super::{DecodeError, Result, decode_bytes};

/// Formats `lofty` parses (by content, with the extension as a hint), plus WMA and DSF,
/// which the format matrix lists under audio.
pub const EXTENSIONS: &[&str] = &[
    "mp3", "flac", "ogg", "oga", "opus", "spx", "m4a", "m4b", "aac", "ape", "wv", "mpc", "wav",
    "aiff", "aif", "aifc", "wma", "dsf",
];

/// ASF Header Object GUID (3026B275-8E66-CF11-A6D9-00AA0062CE6C, wire byte order).
const ASF_HEADER: [u8; 16] = [
    0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C,
];
/// Extended Content Description Object (D2D0A440-E307-11D2-97F0-00A0C95EA850).
const ASF_EXTENDED_CONTENT: [u8; 16] = [
    0x40, 0xA4, 0xD0, 0xD2, 0x07, 0xE3, 0xD2, 0x11, 0x97, 0xF0, 0x00, 0xA0, 0xC9, 0x5E, 0xA8, 0x50,
];
/// Header Extension Object (5FBF03B5-A92E-11CF-8EE3-00C00C205365).
const ASF_HEADER_EXTENSION: [u8; 16] = [
    0xB5, 0x03, 0xBF, 0x5F, 0x2E, 0xA9, 0xCF, 0x11, 0x8E, 0xE3, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65,
];
/// Metadata Library Object (44231C94-9498-49D1-A141-1D134E457054); pictures larger than
/// 64 KiB live here because its value length field is 32 bits wide.
const ASF_METADATA_LIBRARY: [u8; 16] = [
    0x94, 0x1C, 0x23, 0x44, 0x98, 0x94, 0xD1, 0x49, 0xA1, 0x41, 0x1D, 0x13, 0x4E, 0x45, 0x70, 0x54,
];
/// Objects scanned per object list; a hostile header cannot make the walk unbounded.
const ASF_MAX_OBJECTS: usize = 4096;

pub fn extract(data: &[u8], ext: &str) -> Result<DynamicImage> {
    if data.starts_with(&ASF_HEADER) {
        return extract_asf(data);
    }
    if data.starts_with(b"DSD ") {
        return extract_dsf(data);
    }
    let options = ParseOptions::new().read_properties(false);
    let probe = Probe::new(Cursor::new(data))
        .options(options)
        .guess_file_type()
        .map_err(DecodeError::malformed)?;
    let probe = match probe.file_type() {
        Some(_) => probe,
        None => match FileType::from_ext(ext) {
            Some(hint) => probe.set_file_type(hint),
            None => return Err(DecodeError::Unsupported),
        },
    };
    let tagged = probe.read().map_err(DecodeError::malformed)?;
    let pictures: Vec<&Picture> = tagged.tags().iter().flat_map(|t| t.pictures()).collect();
    decode_cover(&pictures)
}

/// Front cover first, then the remaining pictures in tag order; the first that decodes
/// wins.
fn decode_cover(pictures: &[&Picture]) -> Result<DynamicImage> {
    let is_front = |p: &&&Picture| p.pic_type() == PictureType::CoverFront;
    let ordered = pictures
        .iter()
        .filter(is_front)
        .chain(pictures.iter().filter(|p| !is_front(p)));
    decode_first(ordered.map(|p| p.data()))
}

/// Decodes candidates in order; the first that decodes wins.
fn decode_first<'a>(images: impl Iterator<Item = &'a [u8]>) -> Result<DynamicImage> {
    let mut last = DecodeError::Unsupported;
    for data in images {
        match decode_bytes(data) {
            Ok(img) => return Ok(img),
            Err(e) if e.falls_through() => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

fn le_u16(d: &[u8], off: usize) -> Option<u16> {
    let b = d.get(off..off.checked_add(2)?)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

fn le_u32(d: &[u8], off: usize) -> Option<u32> {
    let b = d.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn le_u64(d: &[u8], off: usize) -> Option<u64> {
    let b = d.get(off..off.checked_add(8)?)?;
    let mut a = [0u8; 8];
    a.copy_from_slice(b);
    Some(u64::from_le_bytes(a))
}

/// DSF: the `DSD ` chunk (28 bytes) ends in a pointer to the ID3v2 tag at the end of
/// the file; zero means no metadata.
fn extract_dsf(data: &[u8]) -> Result<DynamicImage> {
    let pointer = le_u64(data, 20).ok_or_else(|| DecodeError::malformed("truncated DSD chunk"))?;
    if pointer == 0 {
        return Err(DecodeError::Unsupported);
    }
    let tag = usize::try_from(pointer)
        .ok()
        .filter(|&p| p >= 28)
        .and_then(|p| data.get(p..))
        .ok_or_else(|| DecodeError::malformed("DSF metadata pointer outside the file"))?;
    if !tag.starts_with(b"ID3") {
        return Err(DecodeError::Unsupported);
    }
    let options = ParseOptions::new().read_properties(false);
    let tagged = Probe::new(Cursor::new(tag))
        .options(options)
        .set_file_type(FileType::Mpeg)
        .read()
        .map_err(DecodeError::malformed)?;
    let pictures: Vec<&Picture> = tagged.tags().iter().flat_map(|t| t.pictures()).collect();
    decode_cover(&pictures)
}

/// UTF-16LE (NUL-terminated) string at the start of `d`: `(code units, bytes consumed)`.
fn utf16_cstr(d: &[u8]) -> Option<(&[u8], usize)> {
    let mut i = 0;
    while i + 2 <= d.len() {
        if d[i] == 0 && d[i + 1] == 0 {
            return Some((&d[..i], i + 2));
        }
        i += 2;
    }
    None
}

fn utf16_eq(raw: &[u8], s: &str) -> bool {
    let mut units = raw
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c));
    let mut expect = s.encode_utf16();
    loop {
        match (units.next(), expect.next()) {
            (None, None) => return true,
            (Some(a), Some(b)) if a == b => {}
            _ => return false,
        }
    }
}

/// A `WM/Picture` value: picture type, data size, MIME, description, picture bytes.
/// Returns `(picture type, picture bytes)`.
fn asf_picture(value: &[u8]) -> Option<(u8, &[u8])> {
    let kind = *value.first()?;
    let size = le_u32(value, 1)? as usize;
    let rest = value.get(5..)?;
    let (_mime, used) = utf16_cstr(rest)?;
    let rest = rest.get(used..)?;
    let (_desc, used) = utf16_cstr(rest)?;
    let image = rest.get(used..)?;
    Some((kind, image.get(..size)?))
}

/// Collects `WM/Picture` values from one object list. `depth` bounds the nesting through
/// the Header Extension Object (header -> extension -> metadata library).
fn asf_collect<'a>(mut objects: &'a [u8], depth: u32, out: &mut Vec<(u8, &'a [u8])>) -> Result<()> {
    for _ in 0..ASF_MAX_OBJECTS {
        if objects.len() < 24 {
            return Ok(());
        }
        let size = le_u64(objects, 16).ok_or_else(|| DecodeError::malformed("ASF object"))?;
        let size = usize::try_from(size)
            .ok()
            .filter(|&s| (24..=objects.len()).contains(&s))
            .ok_or_else(|| DecodeError::malformed("ASF object size exceeds the file"))?;
        let guid = &objects[..16];
        let body = &objects[24..size];
        if guid == ASF_EXTENDED_CONTENT {
            asf_extended_content(body, out)?;
        } else if guid == ASF_METADATA_LIBRARY {
            asf_metadata_library(body, out)?;
        } else if guid == ASF_HEADER_EXTENSION && depth > 0 {
            // Reserved GUID (16), reserved u16, data size u32, then the objects.
            let inner = body
                .get(22..)
                .and_then(|b| {
                    let n = le_u32(body, 18)? as usize;
                    b.get(..n)
                })
                .ok_or_else(|| DecodeError::malformed("ASF header extension size"))?;
            asf_collect(inner, depth - 1, out)?;
        }
        objects = &objects[size..];
    }
    Err(DecodeError::malformed("too many ASF objects"))
}

/// Extended Content Description: u16 count, then (name length u16, name, type u16,
/// value length u16, value) per descriptor. Type 1 is a byte array.
fn asf_extended_content<'a>(body: &'a [u8], out: &mut Vec<(u8, &'a [u8])>) -> Result<()> {
    let bad = || DecodeError::malformed("ASF content description exceeds its object");
    let count = le_u16(body, 0).ok_or_else(bad)?;
    let mut at = 2usize;
    for _ in 0..count {
        let name_len = usize::from(le_u16(body, at).ok_or_else(bad)?);
        let name = body.get(at + 2..at + 2 + name_len).ok_or_else(bad)?;
        at += 2 + name_len;
        let kind = le_u16(body, at).ok_or_else(bad)?;
        let value_len = usize::from(le_u16(body, at + 2).ok_or_else(bad)?);
        let value = body.get(at + 4..at + 4 + value_len).ok_or_else(bad)?;
        at += 4 + value_len;
        if kind == 1 && utf16_eq(trim_nul(name), "WM/Picture") {
            out.extend(asf_picture(value));
        }
    }
    Ok(())
}

/// Metadata Library: u16 count, then (reserved u16, stream u16, name length u16, type
/// u16, value length u32, name, value) per record.
fn asf_metadata_library<'a>(body: &'a [u8], out: &mut Vec<(u8, &'a [u8])>) -> Result<()> {
    let bad = || DecodeError::malformed("ASF metadata library exceeds its object");
    let count = le_u16(body, 0).ok_or_else(bad)?;
    let mut at = 2usize;
    for _ in 0..count {
        let name_len = usize::from(le_u16(body, at + 4).ok_or_else(bad)?);
        let kind = le_u16(body, at + 6).ok_or_else(bad)?;
        let value_len = le_u32(body, at + 8).ok_or_else(bad)? as usize;
        let name = body.get(at + 12..at + 12 + name_len).ok_or_else(bad)?;
        let value_at = at + 12 + name_len;
        let value = body
            .get(value_at..value_at.checked_add(value_len).ok_or_else(bad)?)
            .ok_or_else(bad)?;
        at = value_at + value_len;
        if kind == 1 && utf16_eq(trim_nul(name), "WM/Picture") {
            out.extend(asf_picture(value));
        }
    }
    Ok(())
}

/// Strips one trailing UTF-16 NUL.
fn trim_nul(name: &[u8]) -> &[u8] {
    name.strip_suffix(&[0, 0]).unwrap_or(name)
}

/// WMA: the ASF Header Object's objects (after its 30-byte fixed part), then the
/// `WM/Picture` descriptors; the front cover (picture type 3) first.
fn extract_asf(data: &[u8]) -> Result<DynamicImage> {
    let bad = || DecodeError::malformed("truncated ASF header");
    let size = le_u64(data, 16).ok_or_else(bad)?;
    let size = usize::try_from(size)
        .ok()
        .filter(|&s| (30..=data.len()).contains(&s))
        .ok_or_else(|| DecodeError::malformed("ASF header size exceeds the file"))?;
    let mut pictures = Vec::new();
    asf_collect(&data[30..size], 2, &mut pictures)?;
    let front = pictures.iter().filter(|(k, _)| *k == 3);
    let others = pictures.iter().filter(|(k, _)| *k != 3);
    decode_first(front.chain(others).map(|(_, d)| *d))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgba, RgbaImage};

    fn png(color: [u8; 4], w: u32, h: u32) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        RgbaImage::from_pixel(w, h, Rgba(color))
            .write_to(&mut out, ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    fn apic(pic_type: u8, data: &[u8]) -> Vec<u8> {
        let mut body = vec![0u8]; // ISO-8859-1
        body.extend_from_slice(b"image/png\0");
        body.push(pic_type);
        body.push(0); // empty description
        body.extend_from_slice(data);
        let mut frame = b"APIC".to_vec();
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frame.extend_from_slice(&[0, 0]);
        frame.extend_from_slice(&body);
        frame
    }

    fn synchsafe(n: u32) -> [u8; 4] {
        [
            ((n >> 21) & 0x7F) as u8,
            ((n >> 14) & 0x7F) as u8,
            ((n >> 7) & 0x7F) as u8,
            (n & 0x7F) as u8,
        ]
    }

    /// ID3v2.3 tag with the given frames, followed by silent MPEG-1 layer III frames.
    fn mp3(frames: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = frames.concat();
        let mut f = b"ID3\x03\x00\x00".to_vec();
        f.extend_from_slice(&synchsafe(body.len() as u32));
        f.extend_from_slice(&body);
        for _ in 0..4 {
            // 128 kbit/s, 44.1 kHz, no padding: 417-byte frames.
            f.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
            f.extend_from_slice(&[0u8; 413]);
        }
        f
    }

    #[test]
    fn front_cover_is_preferred() {
        let other = apic(0, &png([255, 0, 0, 255], 4, 4));
        let front = apic(3, &png([0, 0, 255, 255], 6, 3));
        let img = extract(&mp3(&[other, front]), "mp3").unwrap();
        assert_eq!((img.width(), img.height()), (6, 3));
        assert_eq!(img.to_rgba8().get_pixel(0, 0).0, [0, 0, 255, 255]);
    }

    #[test]
    fn first_picture_when_no_front_cover() {
        let back = apic(4, &png([0, 255, 0, 255], 2, 5));
        let img = extract(&mp3(&[back]), "mp3").unwrap();
        assert_eq!((img.width(), img.height()), (2, 5));
        assert_eq!(img.to_rgba8().get_pixel(1, 4).0, [0, 255, 0, 255]);
    }

    #[test]
    fn undecodable_front_cover_falls_back_to_next_picture() {
        let broken = apic(3, b"\x89PNG\r\n\x1a\nbroken");
        let good = apic(0, &png([9, 9, 9, 255], 3, 3));
        let img = extract(&mp3(&[broken, good]), "mp3").unwrap();
        assert_eq!(img.to_rgba8().get_pixel(0, 0).0, [9, 9, 9, 255]);
    }

    #[test]
    fn no_art_or_foreign_data_is_rejected() {
        assert_eq!(extract(&mp3(&[]), "mp3"), Err(DecodeError::Unsupported));
        assert_eq!(
            extract(b"definitely not audio", "xyz"),
            Err(DecodeError::Unsupported)
        );
        // Tag declaring far more bytes than the file holds.
        let mut lying = b"ID3\x03\x00\x00".to_vec();
        lying.extend_from_slice(&synchsafe(0x0FFF_FFFF));
        lying.extend_from_slice(&apic(3, &png([1, 2, 3, 255], 2, 2))[..20]);
        assert!(matches!(
            extract(&lying, "mp3"),
            Err(DecodeError::Malformed(_))
        ));
    }

    fn utf16z(s: &str) -> Vec<u8> {
        s.encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    /// `WM/Picture` value: type, data size, MIME, description, bytes.
    fn wm_picture(kind: u8, data: &[u8]) -> Vec<u8> {
        let mut v = vec![kind];
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend(utf16z("image/png"));
        v.extend(utf16z(""));
        v.extend_from_slice(data);
        v
    }

    fn asf_object(guid: [u8; 16], body: &[u8]) -> Vec<u8> {
        let mut o = guid.to_vec();
        o.extend_from_slice(&((24 + body.len()) as u64).to_le_bytes());
        o.extend_from_slice(body);
        o
    }

    /// Extended Content Description with (name, type, value) descriptors.
    fn ecd(descs: &[(&str, u16, Vec<u8>)]) -> Vec<u8> {
        let mut b = (descs.len() as u16).to_le_bytes().to_vec();
        for (name, kind, value) in descs {
            let n = utf16z(name);
            b.extend_from_slice(&(n.len() as u16).to_le_bytes());
            b.extend(n);
            b.extend_from_slice(&kind.to_le_bytes());
            b.extend_from_slice(&(value.len() as u16).to_le_bytes());
            b.extend_from_slice(value);
        }
        asf_object(ASF_EXTENDED_CONTENT, &b)
    }

    /// Metadata Library with `WM/Picture` records (32-bit value lengths).
    fn metadata_library(pictures: &[Vec<u8>]) -> Vec<u8> {
        let mut b = (pictures.len() as u16).to_le_bytes().to_vec();
        for value in pictures {
            let n = utf16z("WM/Picture");
            b.extend_from_slice(&[0, 0, 0, 0]);
            b.extend_from_slice(&(n.len() as u16).to_le_bytes());
            b.extend_from_slice(&1u16.to_le_bytes());
            b.extend_from_slice(&(value.len() as u32).to_le_bytes());
            b.extend(n);
            b.extend_from_slice(value);
        }
        asf_object(ASF_METADATA_LIBRARY, &b)
    }

    fn header_extension(inner: &[Vec<u8>]) -> Vec<u8> {
        let inner: Vec<u8> = inner.concat();
        let mut b = vec![0u8; 16];
        b.extend_from_slice(&6u16.to_le_bytes());
        b.extend_from_slice(&(inner.len() as u32).to_le_bytes());
        b.extend(inner);
        asf_object(ASF_HEADER_EXTENSION, &b)
    }

    fn wma(objects: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = objects.concat();
        let mut f = ASF_HEADER.to_vec();
        f.extend_from_slice(&((30 + body.len()) as u64).to_le_bytes());
        f.extend_from_slice(&(objects.len() as u32).to_le_bytes());
        f.extend_from_slice(&[1, 2]);
        f.extend(body);
        f.extend_from_slice(&[0x36, 0x26, 0xB2, 0x75, 0, 0, 0, 0]); // start of the data object
        f
    }

    #[test]
    fn wma_front_cover_is_preferred() {
        let other = wm_picture(0, &png([255, 0, 0, 255], 4, 4));
        let front = wm_picture(3, &png([0, 0, 255, 255], 6, 3));
        let title = utf16z("Title");
        let file = wma(&[ecd(&[
            ("Title", 0, title),
            ("WM/Picture", 1, other),
            ("WM/Picture", 1, front),
        ])]);
        let img = extract(&file, "wma").unwrap();
        assert_eq!((img.width(), img.height()), (6, 3));
        assert_eq!(img.to_rgba8().get_pixel(0, 0).0, [0, 0, 255, 255]);
    }

    #[test]
    fn wma_picture_in_the_metadata_library_of_the_header_extension() {
        let pic = wm_picture(3, &png([0, 255, 0, 255], 2, 7));
        let file = wma(&[header_extension(&[metadata_library(&[pic])])]);
        let img = extract(&file, "wma").unwrap();
        assert_eq!((img.width(), img.height()), (2, 7));
        assert_eq!(img.to_rgba8().get_pixel(1, 6).0, [0, 255, 0, 255]);
    }

    #[test]
    fn wma_without_art_is_unsupported() {
        let file = wma(&[ecd(&[("Title", 0, utf16z("x"))])]);
        assert_eq!(extract(&file, "wma"), Err(DecodeError::Unsupported));
    }

    #[test]
    fn wma_lying_sizes_are_malformed_or_unsupported() {
        let pic = wm_picture(3, &png([1, 2, 3, 255], 2, 2));
        let good = wma(&[ecd(&[("WM/Picture", 1, pic.clone())])]);
        // Header object size beyond the file.
        let mut f = good.clone();
        f[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(matches!(extract(&f, "wma"), Err(DecodeError::Malformed(_))));
        // Child object size beyond the header.
        let mut f = good.clone();
        f[30 + 16..30 + 24].copy_from_slice(&0x7FFF_FFFFu64.to_le_bytes());
        assert!(matches!(extract(&f, "wma"), Err(DecodeError::Malformed(_))));
        // Child object smaller than its own header.
        let mut f = good.clone();
        f[30 + 16..30 + 24].copy_from_slice(&3u64.to_le_bytes());
        assert!(matches!(extract(&f, "wma"), Err(DecodeError::Malformed(_))));
        // Descriptor count far beyond the object.
        let mut f = good.clone();
        f[30 + 24..30 + 26].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(matches!(extract(&f, "wma"), Err(DecodeError::Malformed(_))));
        // Value length beyond the object (name is "WM/Picture\0" = 22 bytes).
        let mut f = good.clone();
        let value_len_at = 30 + 24 + 2 + 2 + 22 + 2;
        f[value_len_at..value_len_at + 2].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(matches!(extract(&f, "wma"), Err(DecodeError::Malformed(_))));
        // Picture data size larger than the value: no usable picture.
        let mut bad_pic = pic;
        bad_pic[1..5].copy_from_slice(&u32::MAX.to_le_bytes());
        let f = wma(&[ecd(&[("WM/Picture", 1, bad_pic)])]);
        assert_eq!(extract(&f, "wma"), Err(DecodeError::Unsupported));
        // Header extension claiming more data than it holds.
        let mut f = wma(&[header_extension(&[metadata_library(&[wm_picture(
            3,
            &[1],
        )])])]);
        let ext_size_at = 30 + 24 + 16 + 2;
        f[ext_size_at..ext_size_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(extract(&f, "wma"), Err(DecodeError::Malformed(_))));
        // Truncated header.
        assert!(matches!(
            extract(&good[..20], "wma"),
            Err(DecodeError::Malformed(_))
        ));
    }

    /// A bare ID3v2.3 tag (no audio frames).
    fn id3_tag(frames: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = frames.concat();
        let mut t = b"ID3\x03\x00\x00".to_vec();
        t.extend_from_slice(&synchsafe(body.len() as u32));
        t.extend_from_slice(&body);
        t
    }

    /// DSF: 28-byte `DSD ` chunk, filler standing in for fmt/data chunks, then the tag.
    fn dsf(tag: &[u8], pointer: Option<u64>) -> Vec<u8> {
        let filler = 200usize;
        let mut f = b"DSD ".to_vec();
        f.extend_from_slice(&28u64.to_le_bytes());
        f.extend_from_slice(&((28 + filler + tag.len()) as u64).to_le_bytes());
        f.extend_from_slice(&pointer.unwrap_or((28 + filler) as u64).to_le_bytes());
        f.extend(std::iter::repeat_n(0u8, filler));
        f.extend_from_slice(tag);
        f
    }

    #[test]
    fn dsf_cover_art_via_the_metadata_pointer() {
        let tag = id3_tag(&[
            apic(0, &png([255, 0, 0, 255], 4, 4)),
            apic(3, &png([0, 0, 255, 255], 5, 3)),
        ]);
        let img = extract(&dsf(&tag, None), "dsf").unwrap();
        assert_eq!((img.width(), img.height()), (5, 3));
        assert_eq!(img.to_rgba8().get_pixel(4, 2).0, [0, 0, 255, 255]);
    }

    #[test]
    fn dsf_without_metadata_or_with_bad_pointer_is_rejected() {
        let tag = id3_tag(&[apic(3, &png([1, 2, 3, 255], 2, 2))]);
        assert_eq!(
            extract(&dsf(&tag, Some(0)), "dsf"),
            Err(DecodeError::Unsupported)
        );
        for pointer in [u64::MAX, 100_000, 5] {
            assert!(
                matches!(
                    extract(&dsf(&tag, Some(pointer)), "dsf"),
                    Err(DecodeError::Malformed(_))
                ),
                "pointer {pointer}"
            );
        }
        assert!(matches!(
            extract(b"DSD \x1c\0\0\0\0\0\0\0", "dsf"),
            Err(DecodeError::Malformed(_))
        ));
        // Pointer into the middle of the filler: not an ID3 tag.
        assert_eq!(
            extract(&dsf(&tag, Some(60)), "dsf"),
            Err(DecodeError::Unsupported)
        );
        // ID3 tag declaring far more bytes than the file holds, cut inside its frame.
        let mut lying = tag[..30].to_vec();
        lying[6..10].copy_from_slice(&synchsafe(0x0FFF_FFFF));
        assert!(matches!(
            extract(&dsf(&lying, None), "dsf"),
            Err(DecodeError::Malformed(_))
        ));
    }
}
