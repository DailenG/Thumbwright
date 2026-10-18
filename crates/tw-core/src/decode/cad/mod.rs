//! Previews embedded in CAD, slicer and drawing files.
//!
//! One module per family; each takes the whole file and returns the embedded preview:
//! - [`gcode`]: `; thumbnail begin ...` comment blocks of text G-code (`.gcode`, `.gco`),
//! - [`bgcode`]: thumbnail blocks of PrusaSlicer binary G-code (`.bgcode`),
//! - [`dwg`]: the preview image section of AutoCAD drawings (`.dwg`),
//! - [`dxf`]: the `THUMBNAILIMAGE` section of ASCII DXF files (`.dxf`),
//! - [`clip`]: the `CanvasPreview` table of the SQLite database inside Clip Studio
//!   Paint files (`.clip`), read with the bounded page walker in [`sqlite`].
//!
//! Every parser reads through bounds-checked slices only, caps what it copies or
//! inflates, and hands the preview bytes to the memory-safe `image` decoders (which
//! check the declared dimensions against the pixel budget before allocating).

pub mod bgcode;
pub mod clip;
pub mod dwg;
pub mod dxf;
pub mod gcode;
pub mod sqlite;

use image::DynamicImage;

use super::{DecodeError, Result, check_alloc, check_dimensions, decode_bytes};

pub const EXTENSIONS: &[&str] = &["gcode", "gco", "bgcode", "dwg", "dxf", "clip"];

/// Largest embedded preview image (encoded bytes) any family will copy out of a file.
pub(crate) const MAX_PREVIEW_BYTES: usize = 64 << 20;

pub fn decode(data: &[u8], ext: &str) -> Result<DynamicImage> {
    match ext.to_ascii_lowercase().as_str() {
        "gcode" | "gco" => gcode::decode(data),
        "bgcode" => bgcode::decode(data),
        "dwg" => dwg::decode(data),
        "dxf" => dxf::decode(data),
        "clip" => clip::decode(data),
        _ => Err(DecodeError::Unsupported),
    }
}

pub(crate) fn le16(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        d.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

pub(crate) fn le32(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        d.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

pub(crate) fn be64(d: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(
        d.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
}

/// Decodes standard base64 (RFC 4648 alphabet), ignoring ASCII whitespace and stopping
/// at the first `=`. The output never exceeds `max_out` bytes (`TooLarge` otherwise).
pub(crate) fn base64_decode(text: &[u8], max_out: usize) -> Result<Vec<u8>> {
    fn sextet(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        })
    }
    // Every 4 characters yield at most 3 bytes, so the bound is known up front.
    if text.len() / 4 * 3 > max_out.saturating_add(3) {
        return Err(DecodeError::TooLarge);
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3 + 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in text {
        if c.is_ascii_whitespace() {
            continue;
        }
        if c == b'=' {
            break;
        }
        let v = sextet(c).ok_or_else(|| DecodeError::malformed("invalid base64 character"))?;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    if out.len() > max_out {
        return Err(DecodeError::TooLarge);
    }
    Ok(out)
}

/// Wraps a headerless Windows DIB (BITMAPINFOHEADER or BITMAPCOREHEADER, optional
/// palette, pixels) in a BMP file header so the `image` BMP decoder can read it.
/// Declared dimensions are validated and, for uncompressed pixels, the payload must be
/// complete.
pub(crate) fn dib_to_bmp(dib: &[u8]) -> Result<Vec<u8>> {
    let size = le32(dib, 0).ok_or(DecodeError::Unsupported)? as usize;
    let (width, height, bpp, compression, colors_used, entry) = match size {
        12 => (
            u32::from(le16(dib, 4).ok_or(DecodeError::Unsupported)?),
            u32::from(le16(dib, 6).ok_or(DecodeError::Unsupported)?),
            le16(dib, 10).ok_or(DecodeError::Unsupported)?,
            0,
            0,
            3usize,
        ),
        40..=124 => {
            let w = le32(dib, 4).ok_or(DecodeError::Unsupported)? as i32;
            let h = le32(dib, 8).ok_or(DecodeError::Unsupported)? as i32;
            (
                w.unsigned_abs(),
                h.unsigned_abs(),
                le16(dib, 14).ok_or(DecodeError::Unsupported)?,
                le32(dib, 16).ok_or(DecodeError::Unsupported)?,
                le32(dib, 32).ok_or(DecodeError::Unsupported)?,
                4usize,
            )
        }
        _ => return Err(DecodeError::Unsupported),
    };
    if !matches!(bpp, 1 | 4 | 8 | 16 | 24 | 32) {
        return Err(DecodeError::Unsupported);
    }
    check_dimensions(width, height)?;
    let palette = if bpp <= 8 {
        let full = 1usize << bpp;
        if colors_used == 0 {
            full
        } else {
            (colors_used as usize).min(full)
        }
    } else {
        0
    };
    let masks = match (size, compression) {
        (40, 3) => 12,
        (40, 6) => 16,
        _ => 0,
    };
    let pixels_at = size + masks + palette * entry;
    if matches!(compression, 0 | 3 | 6) {
        let stride = (u64::from(width) * u64::from(bpp)).div_ceil(32) * 4;
        let need = stride
            .checked_mul(u64::from(height))
            .ok_or(DecodeError::TooLarge)?;
        check_alloc(need)?;
        let have = (dib.len().saturating_sub(pixels_at)) as u64;
        if have < need {
            return Err(DecodeError::malformed("bitmap pixel data truncated"));
        }
    }
    if pixels_at > dib.len() {
        return Err(DecodeError::malformed("bitmap header truncated"));
    }
    let total = u32::try_from(dib.len() + 14).map_err(|_| DecodeError::TooLarge)?;
    let mut out = Vec::with_capacity(dib.len() + 14);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&total.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&((pixels_at + 14) as u32).to_le_bytes());
    out.extend_from_slice(dib);
    Ok(out)
}

/// Decodes an embedded preview that is either a complete image file (PNG, JPEG, BMP,
/// QOI...) or a headerless DIB.
pub(crate) fn decode_preview(bytes: &[u8]) -> Result<DynamicImage> {
    if image::guess_format(bytes).is_ok() {
        return decode_bytes(bytes);
    }
    decode_bytes(&dib_to_bmp(bytes)?)
}

#[cfg(test)]
pub(crate) mod fixtures {
    use image::{ImageFormat, Rgb, RgbImage};
    use std::io::Cursor;

    /// PNG of `w` x `h` filled with `color`.
    pub fn png(w: u32, h: u32, color: [u8; 3]) -> Vec<u8> {
        let img = RgbImage::from_pixel(w, h, Rgb(color));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    pub fn jpeg(w: u32, h: u32, color: [u8; 3]) -> Vec<u8> {
        let img = RgbImage::from_pixel(w, h, Rgb(color));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, ImageFormat::Jpeg).unwrap();
        out.into_inner()
    }

    /// 24-bit bottom-up DIB (no file header): top row red, bottom row blue.
    pub fn dib24(w: u32, h: u32) -> Vec<u8> {
        let stride = (w * 3).div_ceil(4) * 4;
        let mut d = Vec::new();
        d.extend_from_slice(&40u32.to_le_bytes());
        d.extend_from_slice(&(w as i32).to_le_bytes());
        d.extend_from_slice(&(h as i32).to_le_bytes());
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&24u16.to_le_bytes());
        d.extend_from_slice(&[0; 24]);
        for row in (0..h).rev() {
            let bgr: [u8; 3] = if row == 0 { [0, 0, 255] } else { [255, 0, 0] };
            for _ in 0..w {
                d.extend_from_slice(&bgr);
            }
            d.extend(std::iter::repeat_n(0, (stride - w * 3) as usize));
        }
        d
    }

    pub fn base64(data: &[u8]) -> String {
        const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut s = String::new();
        for c in data.chunks(3) {
            let n = u32::from(c[0]) << 16
                | u32::from(*c.get(1).unwrap_or(&0)) << 8
                | u32::from(*c.get(2).unwrap_or(&0));
            s.push(A[(n >> 18) as usize & 63] as char);
            s.push(A[(n >> 12) as usize & 63] as char);
            s.push(if c.len() > 1 {
                A[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            s.push(if c.len() > 2 {
                A[n as usize & 63] as char
            } else {
                '='
            });
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn base64_round_trip_and_limits() {
        for n in 0..10usize {
            let data: Vec<u8> = (0..n as u8).map(|b| b.wrapping_mul(37)).collect();
            assert_eq!(base64_decode(base64(&data).as_bytes(), 64).unwrap(), data);
        }
        assert_eq!(
            base64_decode(b"AAAAAAAAAAAAAAAA", 4),
            Err(DecodeError::TooLarge)
        );
        assert!(matches!(
            base64_decode(b"AA*A", 8),
            Err(DecodeError::Malformed(_))
        ));
    }

    #[test]
    fn dib_is_wrapped_and_decoded() {
        let img = decode_preview(&dib24(3, 2)).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (3, 2));
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0]);
        assert_eq!(img.get_pixel(2, 1).0, [0, 0, 255]);
    }

    #[test]
    fn hostile_dibs_are_rejected_without_allocating() {
        let mut huge = dib24(2, 2);
        huge[4..8].copy_from_slice(&65535u32.to_le_bytes());
        huge[8..12].copy_from_slice(&65535u32.to_le_bytes());
        assert_eq!(dib_to_bmp(&huge), Err(DecodeError::TooLarge));
        // Passes the pixel budget, but the payload is missing.
        let mut lying = dib24(2, 2);
        lying[4..8].copy_from_slice(&8000u32.to_le_bytes());
        lying[8..12].copy_from_slice(&8000u32.to_le_bytes());
        assert!(matches!(dib_to_bmp(&lying), Err(DecodeError::Malformed(_))));
        assert_eq!(
            dib_to_bmp(&dib24(2, 2)[..20]),
            Err(DecodeError::Unsupported)
        );
        assert!(dib_to_bmp(&dib24(2, 2)[..45]).is_err());
    }
}
