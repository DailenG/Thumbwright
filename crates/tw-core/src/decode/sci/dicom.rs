//! DICOM (`.dcm`, `.dicom`): Part-10 files (or bare data sets), implicit/explicit VR little
//! endian and explicit VR big endian. Decodes the first frame of uncompressed or RLE
//! lossless 8/16-bit monochrome, PALETTE COLOR, RGB and YBR_FULL images, and JPEG
//! baseline/extended encapsulated frames (through the `image` JPEG decoder). Sequences are
//! skipped iteratively with a bounded nesting counter.

use std::borrow::Cow;

use image::DynamicImage;

use super::{
    DecodeError, Order, Result, check_alloc, check_dimensions, gray_image, pixel_buffer, rgb_image,
    slice,
};

const UNDEFINED: u32 = 0xFFFF_FFFF;
const PIXEL_DATA: (u16, u16) = (0x7FE0, 0x0010);
/// Elements walked before the pixel data must have been found.
const MAX_ELEMENTS: usize = 200_000;
/// Open undefined-length sequences tolerated while skipping.
const MAX_DEPTH: usize = 64;
const MAX_FRAGMENTS: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Compression {
    None,
    Jpeg,
    Rle,
}

#[derive(Clone, Copy, Debug)]
struct Syntax {
    explicit: bool,
    order: Order,
    compression: Compression,
}

fn syntax_for(ts: &str) -> Result<Syntax> {
    let le = |explicit, compression| Syntax {
        explicit,
        order: Order::Little,
        compression,
    };
    Ok(match ts {
        "1.2.840.10008.1.2" => le(false, Compression::None),
        "1.2.840.10008.1.2.1" => le(true, Compression::None),
        "1.2.840.10008.1.2.2" => Syntax {
            explicit: true,
            order: Order::Big,
            compression: Compression::None,
        },
        "1.2.840.10008.1.2.4.50" | "1.2.840.10008.1.2.4.51" => le(true, Compression::Jpeg),
        "1.2.840.10008.1.2.5" => le(true, Compression::Rle),
        // Deflated, JPEG lossless, JPEG-LS, JPEG 2000, video and anything unknown.
        _ => return Err(DecodeError::Unsupported),
    })
}

struct Element {
    group: u16,
    elem: u16,
    len: u32,
    header: usize,
}

fn read_element(d: &[u8], pos: usize, explicit: bool, order: Order) -> Option<Element> {
    let group = order.u16(d, pos)?;
    let elem = order.u16(d, pos + 2)?;
    // Items and delimiters never carry a VR.
    if group == 0xFFFE || !explicit {
        return Some(Element {
            group,
            elem,
            len: order.u32(d, pos + 4)?,
            header: 8,
        });
    }
    let vr = [*d.get(pos + 4)?, *d.get(pos + 5)?];
    let long = matches!(
        &vr,
        b"OB"
            | b"OD"
            | b"OF"
            | b"OL"
            | b"OV"
            | b"OW"
            | b"SQ"
            | b"SV"
            | b"UC"
            | b"UN"
            | b"UR"
            | b"UT"
            | b"UV"
    );
    if long {
        Some(Element {
            group,
            elem,
            len: order.u32(d, pos + 8)?,
            header: 12,
        })
    } else {
        Some(Element {
            group,
            elem,
            len: u32::from(order.u16(d, pos + 6)?),
            header: 8,
        })
    }
}

#[derive(Default)]
struct Attrs {
    samples: Option<u16>,
    rows: Option<u16>,
    cols: Option<u16>,
    bits_alloc: Option<u16>,
    bits_stored: Option<u16>,
    pixel_rep: Option<u16>,
    planar: Option<u16>,
    photometric: String,
    window_center: Option<f64>,
    window_width: Option<f64>,
    intercept: f64,
    slope: Option<f64>,
    /// Palette descriptors `[entries, first value, bits]` for red, green, blue.
    palette_desc: [Option<[u16; 3]>; 3],
    /// Palette data ranges for red, green, blue.
    palette_data: [Option<(usize, usize)>; 3],
}

fn text(d: &[u8]) -> String {
    String::from_utf8_lossy(d)
        .trim_matches(|c: char| c == '\0' || c.is_whitespace())
        .to_string()
}

fn first_number(d: &[u8]) -> Option<f64> {
    text(d).split('\\').next()?.trim().parse().ok()
}

struct Located {
    attrs: Attrs,
    syntax: Syntax,
    /// Pixel data value range; for undefined length, the start of the item stream.
    pixel_start: usize,
    pixel_len: Option<usize>,
}

fn locate(data: &[u8]) -> Result<Located> {
    let preamble = data.get(128..132) == Some(b"DICM");
    let mut pos = if preamble { 132 } else { 0 };
    if !preamble {
        // A bare data set must start with a group 2 or group 8 element.
        match Order::Little.u16(data, 0) {
            Some(2 | 8) => {}
            _ => return Err(DecodeError::Unsupported),
        }
    }
    let mut transfer_syntax = String::new();
    let mut syntax: Option<Syntax> = None;
    let mut attrs = Attrs {
        slope: None,
        ..Attrs::default()
    };
    let mut depth = 0usize;

    for _ in 0..MAX_ELEMENTS {
        let meta = syntax.is_none() && Order::Little.u16(data, pos) == Some(2);
        if syntax.is_none() && !meta {
            syntax = Some(if preamble || !transfer_syntax.is_empty() {
                syntax_for(&transfer_syntax_or_default(&transfer_syntax))?
            } else {
                // No meta header: implicit VR unless the first VR looks like letters.
                let explicit = data
                    .get(pos + 4..pos + 6)
                    .is_some_and(|v| v.iter().all(u8::is_ascii_uppercase));
                Syntax {
                    explicit,
                    order: Order::Little,
                    compression: Compression::None,
                }
            });
        }
        let (explicit, order) = match syntax {
            Some(s) => (s.explicit, s.order),
            None => (true, Order::Little),
        };
        let el = read_element(data, pos, explicit, order)
            .ok_or_else(|| DecodeError::malformed("DICOM element truncated"))?;
        let value_at = pos + el.header;

        if el.group == 0xFFFE {
            if el.elem == 0xE0DD {
                depth = depth.saturating_sub(1);
            }
            pos = value_at;
            continue;
        }
        let tag = (el.group, el.elem);
        if el.len == UNDEFINED {
            if tag == PIXEL_DATA && depth == 0 {
                let syntax =
                    syntax.ok_or_else(|| DecodeError::malformed("DICOM data set missing"))?;
                return Ok(Located {
                    attrs,
                    syntax,
                    pixel_start: value_at,
                    pixel_len: None,
                });
            }
            depth += 1;
            if depth > MAX_DEPTH {
                return Err(DecodeError::malformed("DICOM sequences nested too deeply"));
            }
            pos = value_at;
            continue;
        }
        let len = el.len as usize;
        let value = slice(data, value_at, len);
        if tag == PIXEL_DATA && depth == 0 {
            value.ok_or_else(|| DecodeError::malformed("DICOM pixel data truncated"))?;
            let syntax = syntax.ok_or_else(|| DecodeError::malformed("DICOM data set missing"))?;
            return Ok(Located {
                attrs,
                syntax,
                pixel_start: value_at,
                pixel_len: Some(len),
            });
        }
        let value = value.ok_or_else(|| DecodeError::malformed("DICOM element truncated"))?;
        if depth == 0 {
            let us = |v: &[u8]| order.u16(v, 0);
            match tag {
                (0x0002, 0x0010) => transfer_syntax = text(value),
                (0x0028, 0x0002) => attrs.samples = us(value),
                (0x0028, 0x0004) => attrs.photometric = text(value),
                (0x0028, 0x0006) => attrs.planar = us(value),
                (0x0028, 0x0010) => attrs.rows = us(value),
                (0x0028, 0x0011) => attrs.cols = us(value),
                (0x0028, 0x0100) => attrs.bits_alloc = us(value),
                (0x0028, 0x0101) => attrs.bits_stored = us(value),
                (0x0028, 0x0103) => attrs.pixel_rep = us(value),
                (0x0028, 0x1050) => attrs.window_center = first_number(value),
                (0x0028, 0x1051) => attrs.window_width = first_number(value),
                (0x0028, 0x1052) => attrs.intercept = first_number(value).unwrap_or(0.0),
                (0x0028, 0x1053) => attrs.slope = first_number(value),
                (0x0028, e @ 0x1101..=0x1103) => {
                    attrs.palette_desc[usize::from(e - 0x1101)] =
                        match (us(value), order.u16(value, 2), order.u16(value, 4)) {
                            (Some(a), Some(b), Some(c)) => Some([a, b, c]),
                            _ => None,
                        };
                }
                (0x0028, e @ 0x1201..=0x1203) => {
                    attrs.palette_data[usize::from(e - 0x1201)] = Some((value_at, len));
                }
                _ => {}
            }
        }
        // Group 2 is always explicit little endian: `syntax` is unset while it is read.
        pos = value_at + len;
    }
    Err(DecodeError::malformed("DICOM pixel data not found"))
}

fn transfer_syntax_or_default(ts: &str) -> String {
    if ts.is_empty() {
        "1.2.840.10008.1.2.1".to_string()
    } else {
        ts.to_string()
    }
}

/// Fragments of an encapsulated pixel data element (the basic offset table is skipped).
fn fragments(data: &[u8], start: usize) -> Result<Vec<(usize, usize)>> {
    let mut pos = start;
    let mut out = Vec::new();
    let mut first = true;
    for _ in 0..MAX_FRAGMENTS {
        let group = Order::Little.u16(data, pos);
        let elem = Order::Little.u16(data, pos + 2);
        let len = Order::Little.u32(data, pos + 4);
        let (Some(group), Some(elem), Some(len)) = (group, elem, len) else {
            return Err(DecodeError::malformed("DICOM fragment truncated"));
        };
        if group != 0xFFFE || elem == 0xE0DD {
            break;
        }
        if elem != 0xE000 || len == UNDEFINED {
            return Err(DecodeError::malformed("invalid DICOM fragment item"));
        }
        let at = pos + 8;
        slice(data, at, len as usize)
            .ok_or_else(|| DecodeError::malformed("DICOM fragment truncated"))?;
        if !first {
            out.push((at, len as usize));
        }
        first = false;
        pos = at + len as usize;
    }
    if out.is_empty() {
        return Err(DecodeError::malformed("DICOM has no pixel fragments"));
    }
    Ok(out)
}

/// PackBits-style segment decoder producing exactly `size` bytes.
fn unpack_segment(src: &[u8], size: usize, out: &mut [u8]) -> Result<()> {
    let out = &mut out[..size];
    let (mut i, mut o) = (0usize, 0usize);
    while o < size {
        let n = *src
            .get(i)
            .ok_or_else(|| DecodeError::malformed("DICOM RLE segment truncated"))?;
        i += 1;
        match n {
            0..=127 => {
                let count = usize::from(n) + 1;
                let lit = slice(src, i, count)
                    .ok_or_else(|| DecodeError::malformed("DICOM RLE literal truncated"))?;
                let dst = out
                    .get_mut(o..o + count)
                    .ok_or_else(|| DecodeError::malformed("DICOM RLE overruns the frame"))?;
                dst.copy_from_slice(lit);
                i += count;
                o += count;
            }
            129..=255 => {
                let count = 257 - usize::from(n);
                let b = *src
                    .get(i)
                    .ok_or_else(|| DecodeError::malformed("DICOM RLE run truncated"))?;
                i += 1;
                let dst = out
                    .get_mut(o..o + count)
                    .ok_or_else(|| DecodeError::malformed("DICOM RLE overruns the frame"))?;
                dst.fill(b);
                o += count;
            }
            128 => {}
        }
    }
    Ok(())
}

/// RLE lossless frame to planar big-endian bytes (MSB plane first for 16-bit samples).
fn decode_rle(frame: &[u8], pixels: usize, samples: usize, bytes_alloc: usize) -> Result<Vec<u8>> {
    let segments = Order::Little.u32(frame, 0).unwrap_or(0) as usize;
    if segments != samples * bytes_alloc {
        return Err(DecodeError::Unsupported);
    }
    let total = pixels.checked_mul(segments).ok_or(DecodeError::TooLarge)?;
    check_alloc(total as u64)?;
    // Segment `s` holds byte `s % bytes_alloc` (MSB first) of sample plane `s / bytes_alloc`.
    let mut planes = vec![0u8; total];
    for s in 0..segments {
        let offset = Order::Little.u32(frame, 4 + s * 4).unwrap_or(0) as usize;
        let seg = frame
            .get(offset..)
            .ok_or_else(|| DecodeError::malformed("DICOM RLE segment offset out of range"))?;
        unpack_segment(seg, pixels, &mut planes[s * pixels..(s + 1) * pixels])?;
    }
    if bytes_alloc == 1 {
        return Ok(planes);
    }
    // Interleave the MSB/LSB planes into big-endian samples, keeping sample planes apart.
    let mut out = vec![0u8; total];
    for p in 0..samples {
        for i in 0..pixels {
            let at = (p * pixels + i) * 2;
            out[at] = planes[(p * 2) * pixels + i];
            out[at + 1] = planes[(p * 2 + 1) * pixels + i];
        }
    }
    Ok(out)
}

struct Pixels<'a> {
    bytes: &'a [u8],
    order: Order,
    planar: bool,
}

fn decode_native(data: &[u8], attrs: &Attrs, px: &Pixels<'_>) -> Result<DynamicImage> {
    let (w, h) = dimensions(attrs)?;
    let samples = usize::from(attrs.samples.unwrap_or(1));
    if !matches!(samples, 1 | 3) {
        return Err(DecodeError::Unsupported);
    }
    let bits_alloc = attrs.bits_alloc.unwrap_or(0);
    let bytes_alloc = match bits_alloc {
        8 => 1usize,
        16 => 2,
        _ => return Err(DecodeError::Unsupported),
    };
    let (wu, hu) = (w as usize, h as usize);
    let n = wu * hu;
    let photometric = attrs.photometric.as_str();
    let frame = n * samples * bytes_alloc;
    if px.bytes.len() < frame {
        return Err(DecodeError::malformed(
            "DICOM pixel data shorter than one frame",
        ));
    }
    let bytes = &px.bytes[..frame];
    let bits_stored = attrs.bits_stored.unwrap_or(bits_alloc).clamp(1, bits_alloc);
    let mask = (1u32 << bits_stored) - 1;
    let signed = attrs.pixel_rep == Some(1);
    let raw = |i: usize| -> i32 {
        let v = if bytes_alloc == 1 {
            u32::from(bytes[i])
        } else {
            u32::from(px.order.u16(bytes, i * 2).unwrap_or(0))
        } & mask;
        if signed && v >> (bits_stored - 1) & 1 == 1 {
            v as i32 - (1i32 << bits_stored)
        } else {
            v as i32
        }
    };

    match (samples, photometric) {
        (1, "MONOCHROME1" | "MONOCHROME2") => {
            let slope = attrs.slope.unwrap_or(1.0);
            let value = |i: usize| f64::from(raw(i)) * slope + attrs.intercept;
            let (center, width) = match (attrs.window_center, attrs.window_width) {
                (Some(c), Some(ww)) if ww >= 1.0 => (c, ww),
                _ => {
                    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
                    for i in 0..n {
                        let v = value(i);
                        lo = lo.min(v);
                        hi = hi.max(v);
                    }
                    ((lo + hi) / 2.0 + 0.5, (hi - lo + 1.0).max(1.0))
                }
            };
            let mut out = pixel_buffer(w, h, 1)?;
            let invert = photometric == "MONOCHROME1";
            for (i, o) in out.iter_mut().enumerate() {
                // DICOM linear window function (PS3.3 C.11.2.1.2).
                let y = if width <= 1.0 {
                    if value(i) > center - 0.5 { 1.0 } else { 0.0 }
                } else {
                    ((value(i) - (center - 0.5)) / (width - 1.0) + 0.5).clamp(0.0, 1.0)
                };
                let v = (y * 255.0 + 0.5) as u8;
                *o = if invert { 255 - v } else { v };
            }
            gray_image(w, h, out)
        }
        (1, "PALETTE COLOR") => {
            let mut luts: Vec<Vec<u8>> = Vec::with_capacity(3);
            let mut first = 0i32;
            let mut entries = 0usize;
            for (desc, range) in attrs.palette_desc.iter().zip(&attrs.palette_data) {
                let (Some(desc), Some((at, len))) = (desc, range) else {
                    return Err(DecodeError::malformed("DICOM palette missing"));
                };
                entries = if desc[0] == 0 {
                    65536
                } else {
                    usize::from(desc[0])
                };
                first = i32::from(desc[1]);
                let pal = slice(data, *at, *len)
                    .ok_or_else(|| DecodeError::malformed("DICOM palette truncated"))?;
                luts.push(
                    (0..entries)
                        .map(|e| {
                            let v = px.order.u16(pal, e * 2).unwrap_or(0);
                            if desc[2] > 8 { (v >> 8) as u8 } else { v as u8 }
                        })
                        .collect(),
                );
            }
            let mut out = pixel_buffer(w, h, 3)?;
            for (i, o) in out.as_chunks_mut::<3>().0.iter_mut().enumerate() {
                let idx = (raw(i) - first).clamp(0, entries as i32 - 1) as usize;
                for (d, lut) in o.iter_mut().zip(&luts) {
                    *d = lut.get(idx).copied().unwrap_or(0);
                }
            }
            rgb_image(w, h, out)
        }
        (3, "RGB" | "YBR_FULL") if bytes_alloc == 1 => {
            let mut out = pixel_buffer(w, h, 3)?;
            for i in 0..n {
                let at = |c: usize| {
                    if px.planar { c * n + i } else { i * 3 + c }
                };
                let (a, b, c) = (bytes[at(0)], bytes[at(1)], bytes[at(2)]);
                let rgb = if photometric == "RGB" {
                    [a, b, c]
                } else {
                    let (y, cb, cr) = (f32::from(a), f32::from(b) - 128.0, f32::from(c) - 128.0);
                    let q = |v: f32| v.clamp(0.0, 255.0).round() as u8;
                    [
                        q(y + 1.402 * cr),
                        q(y - 0.344_136 * cb - 0.714_136 * cr),
                        q(y + 1.772 * cb),
                    ]
                };
                out[i * 3..i * 3 + 3].copy_from_slice(&rgb);
            }
            rgb_image(w, h, out)
        }
        _ => Err(DecodeError::Unsupported),
    }
}

/// Frame dimensions declared by the data set, validated against the budgets.
fn dimensions(attrs: &Attrs) -> Result<(u32, u32)> {
    let (Some(rows), Some(cols)) = (attrs.rows, attrs.cols) else {
        return Err(DecodeError::malformed("DICOM rows/columns missing"));
    };
    let (w, h) = (u32::from(cols), u32::from(rows));
    check_dimensions(w, h)?;
    Ok((w, h))
}

/// Bytes of the first JPEG frame: fragments up to and including the one ending in EOI.
fn first_jpeg_frame<'a>(data: &'a [u8], frags: &[(usize, usize)]) -> Result<Cow<'a, [u8]>> {
    let mut end = frags.len();
    for (i, &(at, len)) in frags.iter().enumerate() {
        if data[at..at + len].ends_with(&[0xFF, 0xD9]) {
            end = i + 1;
            break;
        }
    }
    if end == 1 {
        let (at, len) = frags[0];
        return Ok(Cow::Borrowed(&data[at..at + len]));
    }
    let total: usize = frags[..end].iter().map(|f| f.1).sum();
    check_alloc(total as u64)?;
    let mut buf = Vec::with_capacity(total);
    for &(at, len) in &frags[..end] {
        buf.extend_from_slice(&data[at..at + len]);
    }
    Ok(Cow::Owned(buf))
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    let loc = locate(data)?;
    let attrs = &loc.attrs;
    match (loc.syntax.compression, loc.pixel_len) {
        (Compression::None, Some(len)) => {
            let px = Pixels {
                bytes: &data[loc.pixel_start..loc.pixel_start + len],
                order: loc.syntax.order,
                planar: attrs.planar == Some(1),
            };
            decode_native(data, attrs, &px)
        }
        (Compression::None, None) => Err(DecodeError::malformed(
            "undefined-length pixel data in a native transfer syntax",
        )),
        (_, Some(_)) => Err(DecodeError::malformed(
            "defined-length pixel data in an encapsulated transfer syntax",
        )),
        (Compression::Jpeg, None) => {
            let frags = fragments(data, loc.pixel_start)?;
            let jpeg = first_jpeg_frame(data, &frags)?;
            if !jpeg.starts_with(&[0xFF, 0xD8]) {
                return Err(DecodeError::Unsupported);
            }
            super::super::decode_bytes(&jpeg)
        }
        (Compression::Rle, None) => {
            let (w, h) = dimensions(attrs)?;
            let samples = usize::from(attrs.samples.unwrap_or(1));
            if !matches!(samples, 1 | 3) {
                return Err(DecodeError::Unsupported);
            }
            let bytes_alloc = match attrs.bits_alloc {
                Some(8) => 1,
                Some(16) => 2,
                _ => return Err(DecodeError::Unsupported),
            };
            let frags = fragments(data, loc.pixel_start)?;
            let (at, len) = frags[0];
            let planes = decode_rle(
                &data[at..at + len],
                w as usize * h as usize,
                samples,
                bytes_alloc,
            )?;
            let px = Pixels {
                bytes: &planes,
                order: Order::Big,
                planar: true,
            };
            decode_native(data, attrs, &px)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPLICIT_LE: &str = "1.2.840.10008.1.2.1";

    fn explicit(group: u16, elem: u16, vr: &[u8; 2], value: &[u8]) -> Vec<u8> {
        let mut v = value.to_vec();
        if !v.len().is_multiple_of(2) {
            v.push(if vr == b"UI" { 0 } else { b' ' });
        }
        let mut out = Vec::new();
        out.extend(group.to_le_bytes());
        out.extend(elem.to_le_bytes());
        out.extend(vr);
        if matches!(vr, b"OB" | b"OW" | b"SQ" | b"UN" | b"UT") {
            out.extend([0, 0]);
            out.extend((v.len() as u32).to_le_bytes());
        } else {
            out.extend((v.len() as u16).to_le_bytes());
        }
        out.extend(v);
        out
    }

    fn us(group: u16, elem: u16, v: u16) -> Vec<u8> {
        explicit(group, elem, b"US", &v.to_le_bytes())
    }

    fn preamble(ts: &str) -> Vec<u8> {
        let mut f = vec![0u8; 128];
        f.extend(b"DICM");
        f.extend(explicit(0x0002, 0x0010, b"UI", ts.as_bytes()));
        f
    }

    fn image_attrs(rows: u16, cols: u16, samples: u16, photometric: &str, bits: u16) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend(us(0x0028, 0x0002, samples));
        f.extend(explicit(0x0028, 0x0004, b"CS", photometric.as_bytes()));
        f.extend(us(0x0028, 0x0010, rows));
        f.extend(us(0x0028, 0x0011, cols));
        f.extend(us(0x0028, 0x0100, bits));
        f.extend(us(0x0028, 0x0101, bits));
        f
    }

    #[test]
    fn eight_bit_monochrome_is_stretched_between_min_and_max() {
        let mut f = preamble(EXPLICIT_LE);
        f.extend(image_attrs(2, 2, 1, "MONOCHROME2", 8));
        f.extend(explicit(0x7FE0, 0x0010, b"OW", &[0, 100, 200, 100]));
        let img = decode(&f).unwrap().to_luma8();
        assert_eq!(img.dimensions(), (2, 2));
        assert_eq!(img.as_raw(), &[0, 128, 255, 128]);
    }

    #[test]
    fn signed_sixteen_bit_uses_the_window() {
        let mut f = preamble(EXPLICIT_LE);
        f.extend(image_attrs(1, 4, 1, "MONOCHROME2", 16));
        f.extend(us(0x0028, 0x0103, 1));
        f.extend(explicit(0x0028, 0x1050, b"DS", b"0"));
        f.extend(explicit(0x0028, 0x1051, b"DS", b"2001"));
        let px: Vec<u8> = [-1000i16, 0, 1000, 3000]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        f.extend(explicit(0x7FE0, 0x0010, b"OW", &px));
        let img = decode(&f).unwrap().to_luma8();
        assert_eq!(img.as_raw(), &[0, 128, 255, 255]);
    }

    #[test]
    fn planar_rgb_and_monochrome1_in_an_implicit_data_set() {
        let mut f = preamble(EXPLICIT_LE);
        f.extend(image_attrs(1, 2, 3, "RGB", 8));
        f.extend(us(0x0028, 0x0006, 1));
        f.extend(explicit(0x7FE0, 0x0010, b"OW", &[255, 0, 0, 255, 0, 0]));
        let img = decode(&f).unwrap().to_rgb8();
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 255, 0]);

        // Bare implicit VR little endian data set, MONOCHROME1 is inverted.
        let imp = |g: u16, e: u16, v: &[u8]| {
            let mut o = Vec::new();
            o.extend(g.to_le_bytes());
            o.extend(e.to_le_bytes());
            o.extend((v.len() as u32).to_le_bytes());
            o.extend(v);
            o
        };
        let mut f = imp(0x0008, 0x0060, b"OT");
        f.extend(imp(0x0028, 0x0002, &1u16.to_le_bytes()));
        f.extend(imp(0x0028, 0x0004, b"MONOCHROME1 "));
        f.extend(imp(0x0028, 0x0010, &1u16.to_le_bytes()));
        f.extend(imp(0x0028, 0x0011, &2u16.to_le_bytes()));
        f.extend(imp(0x0028, 0x0100, &8u16.to_le_bytes()));
        f.extend(imp(0x7FE0, 0x0010, &[0, 255]));
        let img = decode(&f).unwrap().to_luma8();
        assert_eq!(img.as_raw(), &[255, 0]);
    }

    #[test]
    fn palette_color_maps_indices() {
        let mut f = preamble(EXPLICIT_LE);
        f.extend(image_attrs(1, 2, 1, "PALETTE COLOR", 8));
        let desc: Vec<u8> = [2u16, 0, 8].iter().flat_map(|v| v.to_le_bytes()).collect();
        let lut =
            |a: u16, b: u16| -> Vec<u8> { [a, b].iter().flat_map(|v| v.to_le_bytes()).collect() };
        for (i, (a, b)) in [(255, 0), (0, 255), (0, 0)].into_iter().enumerate() {
            f.extend(explicit(0x0028, 0x1101 + i as u16, b"US", &desc));
            f.extend(explicit(0x0028, 0x1201 + i as u16, b"OW", &lut(a, b)));
        }
        f.extend(explicit(0x7FE0, 0x0010, b"OW", &[0, 1]));
        let img = decode(&f).unwrap().to_rgb8();
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 255, 0]);
    }

    fn encapsulated(ts: &str, attrs: Vec<u8>, fragments: &[&[u8]]) -> Vec<u8> {
        let mut f = preamble(ts);
        f.extend(attrs);
        f.extend(0x7FE0u16.to_le_bytes());
        f.extend(0x0010u16.to_le_bytes());
        f.extend(b"OB\0\0");
        f.extend(UNDEFINED.to_le_bytes());
        let item = |f: &mut Vec<u8>, d: &[u8]| {
            f.extend(0xFFFEu16.to_le_bytes());
            f.extend(0xE000u16.to_le_bytes());
            f.extend((d.len() as u32).to_le_bytes());
            f.extend(d);
        };
        item(&mut f, &[]);
        for d in fragments {
            item(&mut f, d);
        }
        f.extend(0xFFFEu16.to_le_bytes());
        f.extend(0xE0DDu16.to_le_bytes());
        f.extend(0u32.to_le_bytes());
        f
    }

    #[test]
    fn rle_lossless_frame() {
        let mut seg = vec![0u8; 64];
        seg[..4].copy_from_slice(&1u32.to_le_bytes());
        seg[4..8].copy_from_slice(&64u32.to_le_bytes());
        seg.extend([3, 10, 20, 30, 40]); // literal run of four bytes
        let f = encapsulated(
            "1.2.840.10008.1.2.5",
            image_attrs(2, 2, 1, "MONOCHROME2", 8),
            &[&seg],
        );
        let img = decode(&f).unwrap().to_luma8();
        assert_eq!(img.as_raw(), &[0, 85, 170, 255]);
    }

    #[test]
    fn jpeg_baseline_frame_split_over_fragments() {
        let src = image::RgbImage::from_pixel(16, 16, image::Rgb([250, 5, 5]));
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 95)
            .encode_image(&src)
            .unwrap();
        let (a, b) = jpeg.split_at(jpeg.len() / 2);
        let f = encapsulated(
            "1.2.840.10008.1.2.4.50",
            image_attrs(16, 16, 3, "YBR_FULL_422", 8),
            &[a, b],
        );
        let img = decode(&f).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (16, 16));
        let p = img.get_pixel(8, 8).0;
        assert!(p[0] > 235 && p[1] < 25 && p[2] < 25, "{p:?}");
    }

    #[test]
    fn hostile_inputs_are_rejected() {
        assert_eq!(
            decode(b"not a dicom file at all"),
            Err(DecodeError::Unsupported)
        );
        // Declared 65535 x 65535.
        let mut f = preamble(EXPLICIT_LE);
        f.extend(image_attrs(65535, 65535, 1, "MONOCHROME2", 16));
        f.extend(explicit(0x7FE0, 0x0010, b"OW", &[0; 8]));
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
        // 20000 x 20000 16-bit but only 8 bytes of pixels.
        let mut f = preamble(EXPLICIT_LE);
        f.extend(image_attrs(20000, 20000, 1, "MONOCHROME2", 16));
        f.extend(explicit(0x7FE0, 0x0010, b"OW", &[0; 8]));
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        // Element running past the end of the file.
        let mut f = preamble(EXPLICIT_LE);
        f.extend(explicit(0x0028, 0x0004, b"CS", b"MONOCHROME2"));
        let cut = f.len() - 4;
        f.truncate(cut);
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        // Truncated right after the preamble.
        assert!(matches!(
            decode(&preamble(EXPLICIT_LE)[..140]),
            Err(DecodeError::Malformed(_))
        ));
        // Deeply nested undefined-length sequences.
        let mut f = preamble(EXPLICIT_LE);
        for _ in 0..200 {
            f.extend([
                0x08, 0x00, 0x40, 0x11, b'S', b'Q', 0, 0, 0xFF, 0xFF, 0xFF, 0xFF,
            ]);
        }
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        // Deflated transfer syntax is not decoded.
        let mut f = preamble("1.2.840.10008.1.2.1.99");
        f.extend(explicit(0x7FE0, 0x0010, b"OW", &[0; 8]));
        assert_eq!(decode(&f), Err(DecodeError::Unsupported));
    }
}
