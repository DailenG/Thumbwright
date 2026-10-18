//! Simple, well-documented legacy bitmap formats, each decoded by a small pure-Rust
//! decoder: X bitmap/pixmap, Sun raster, MacPaint, Amiga IFF ILBM, Alias PIX, DR Halo CUT,
//! PlayStation TIM, Valve VTF, portable float maps, PGX, ZX-Spectrum SCREEN$, animated
//! cursors and CALS raster.
//!
//! Every decoder validates the declared dimensions with [`check_dimensions`] and every
//! pixel / decompression buffer with [`check_alloc`] before allocating it, and bounds
//! RLE output by the declared image size.

// Pixel and row walks pair `chunks_exact` with other iterators and index by position;
// the `as_chunks` rewrite offers no safety gain over the already length-checked slices.
#![allow(clippy::chunks_exact_to_as_chunks)]

use image::{DynamicImage, GrayImage, RgbaImage};

use super::{DecodeError, Result, check_alloc, check_dimensions};

mod ani;
mod cals;
mod cut;
mod ilbm;
mod macpaint;
mod pfm;
mod pgx;
mod pix;
mod scr;
mod sunras;
mod tim;
mod vtf;
mod xbm;
mod xpm;

pub const EXTENSIONS: &[&str] = &[
    "xbm", "xpm", "ras", "sun", "mac", "iff", "ilbm", "lbm", "pix", "cut", "tim", "vtf", "pfm",
    "phm", "pgx", "scr", "ani", "cal", "cals",
];

/// Decodes `data` as the format named by the lower-case extension `ext`.
pub fn decode(data: &[u8], ext: &str) -> Result<DynamicImage> {
    match ext {
        "xbm" => xbm::decode(data),
        "xpm" => xpm::decode(data),
        "ras" | "sun" => sunras::decode(data),
        "mac" => macpaint::decode(data),
        "iff" | "ilbm" | "lbm" => ilbm::decode(data),
        "pix" => pix::decode(data),
        "cut" => cut::decode(data),
        "tim" => tim::decode(data),
        "vtf" => vtf::decode(data),
        "pfm" | "phm" => pfm::decode(data),
        "pgx" => pgx::decode(data),
        "scr" => scr::decode(data),
        "ani" => ani::decode(data),
        "cal" | "cals" => cals::decode(data),
        _ => Err(DecodeError::Unsupported),
    }
}

pub(super) fn le16(d: &[u8], at: usize) -> Option<u16> {
    let b = d.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

pub(super) fn le32(d: &[u8], at: usize) -> Option<u32> {
    let b = d.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

pub(super) fn be16(d: &[u8], at: usize) -> Option<u16> {
    let b = d.get(at..at.checked_add(2)?)?;
    Some(u16::from_be_bytes([b[0], b[1]]))
}

pub(super) fn be32(d: &[u8], at: usize) -> Option<u32> {
    let b = d.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// Validates `width x height` and the byte size of a `width * height * bytes_per_pixel`
/// buffer against the budgets, returning that size.
pub(super) fn pixel_bytes(width: u32, height: u32, bytes_per_pixel: u64) -> Result<usize> {
    check_dimensions(width, height)?;
    let bytes = u64::from(width) * u64::from(height) * bytes_per_pixel;
    check_alloc(bytes)?;
    usize::try_from(bytes).map_err(|_| DecodeError::TooLarge)
}

pub(super) fn gray_image(width: u32, height: u32, buf: Vec<u8>) -> Result<DynamicImage> {
    GrayImage::from_raw(width, height, buf)
        .map(DynamicImage::ImageLuma8)
        .ok_or_else(|| DecodeError::malformed("pixel buffer size mismatch"))
}

pub(super) fn rgba_image(width: u32, height: u32, buf: Vec<u8>) -> Result<DynamicImage> {
    RgbaImage::from_raw(width, height, buf)
        .map(DynamicImage::ImageRgba8)
        .ok_or_else(|| DecodeError::malformed("pixel buffer size mismatch"))
}

/// Unpacks a PackBits / IFF ByteRun1 stream from `src` into exactly `out.len()` bytes,
/// returning the number of source bytes consumed. Output is bounded by `out`, so a
/// decompression bomb cannot exceed the declared image.
pub(super) fn unpack_bits(src: &[u8], out: &mut [u8]) -> Result<usize> {
    let truncated = || DecodeError::malformed("RLE data truncated");
    let mut s = 0usize;
    let mut d = 0usize;
    while d < out.len() {
        let n = *src.get(s).ok_or_else(truncated)? as i8;
        s += 1;
        if n >= 0 {
            let len = n as usize + 1;
            let lit = src.get(s..s + len).ok_or_else(truncated)?;
            let dst = out
                .get_mut(d..d + len)
                .ok_or_else(|| DecodeError::malformed("RLE run overruns the row"))?;
            dst.copy_from_slice(lit);
            s += len;
            d += len;
        } else if n != -128 {
            let len = (1 - i16::from(n)) as usize;
            let b = *src.get(s).ok_or_else(truncated)?;
            s += 1;
            out.get_mut(d..d + len)
                .ok_or_else(|| DecodeError::malformed("RLE run overruns the row"))?
                .fill(b);
            d += len;
        }
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packbits_literal_repeat_and_nop() {
        let mut out = [0u8; 6];
        // literal 2 bytes, nop, repeat 'x' 4 times (-3 => 4 copies)
        let used = unpack_bits(&[1, 7, 8, 0x80, 0xFD, b'x'], &mut out).unwrap();
        assert_eq!(used, 6);
        assert_eq!(out, [7, 8, b'x', b'x', b'x', b'x']);
    }

    #[test]
    fn packbits_run_past_the_row_is_malformed() {
        let mut out = [0u8; 2];
        assert!(matches!(
            unpack_bits(&[0xF0, 1], &mut out),
            Err(DecodeError::Malformed(_))
        ));
        assert!(unpack_bits(&[5, 1], &mut out).is_err());
    }

    #[test]
    fn unknown_extension_is_unsupported() {
        assert_eq!(decode(b"", "zzz").unwrap_err(), DecodeError::Unsupported);
    }
}
