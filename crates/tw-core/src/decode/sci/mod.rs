//! Scientific, medical, film, texture and network image formats: FITS, DPX, Cineon, DICOM,
//! MIFF, RLA, Utah RLE, VIFF, Photo CD, KTX, Sixel, XYZE, MATLAB, VICAR, MNG, JNG and GIMP
//! XCF. Every decoder is memory-safe Rust that validates declared sizes against the budgets
//! before allocating and uses checked arithmetic on file-controlled values.

use image::{DynamicImage, GrayImage, RgbImage, RgbaImage};

use super::{DecodeError, Result, check_alloc, check_dimensions};

mod cineon;
mod dicom;
mod dpx;
mod fits;
mod jng;
mod ktx;
mod mat;
mod miff;
mod mng;
mod pcd;
mod rla;
mod sixel;
mod utah_rle;
mod vicar;
mod viff;
mod vips;
mod xcf;
mod xyze;

pub const EXTENSIONS: &[&str] = &[
    "fits", "fts", "dpx", "cin", "dcm", "dicom", "miff", "rla", "rle", "viff", "xv", "pcd", "ph",
    "ktx", "six", "sixel", "xyze", "mat", "vicar", "mng", "jng", "xcf", "vips",
];

/// Decodes `data` as the format named by `ext` (lower-case, no dot).
pub fn decode(data: &[u8], ext: &str) -> Result<DynamicImage> {
    match ext {
        "fits" | "fts" => fits::decode(data),
        "dpx" => dpx::decode(data),
        "cin" => cineon::decode(data),
        "dcm" | "dicom" => dicom::decode(data),
        "miff" => miff::decode(data),
        "rla" => rla::decode(data),
        "rle" => utah_rle::decode(data),
        "viff" | "xv" => viff::decode(data),
        "pcd" | "ph" => pcd::decode(data),
        "ktx" => ktx::decode(data),
        "six" | "sixel" => sixel::decode(data),
        "xyze" => xyze::decode(data),
        "mat" => mat::decode(data),
        "vicar" => vicar::decode(data),
        "mng" => mng::decode(data),
        "jng" => jng::decode(data),
        "xcf" => xcf::decode(data),
        "vips" => vips::decode(data),
        _ => Err(DecodeError::Unsupported),
    }
}

/// Byte order of a multi-byte field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Order {
    Big,
    Little,
}

impl Order {
    pub(super) fn u16(self, d: &[u8], at: usize) -> Option<u16> {
        let b: [u8; 2] = d.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(match self {
            Self::Big => u16::from_be_bytes(b),
            Self::Little => u16::from_le_bytes(b),
        })
    }

    pub(super) fn u32(self, d: &[u8], at: usize) -> Option<u32> {
        let b: [u8; 4] = d.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(match self {
            Self::Big => u32::from_be_bytes(b),
            Self::Little => u32::from_le_bytes(b),
        })
    }

    pub(super) fn f32(self, d: &[u8], at: usize) -> Option<f32> {
        self.u32(d, at).map(f32::from_bits)
    }
}

pub(super) fn be16(d: &[u8], at: usize) -> Option<u16> {
    Order::Big.u16(d, at)
}

pub(super) fn be32(d: &[u8], at: usize) -> Option<u32> {
    Order::Big.u32(d, at)
}

pub(super) fn le16(d: &[u8], at: usize) -> Option<u16> {
    Order::Little.u16(d, at)
}

/// `d[at..at + len]` without overflow or panic.
pub(super) fn slice(d: &[u8], at: usize, len: usize) -> Option<&[u8]> {
    d.get(at..at.checked_add(len)?)
}

/// Validates the dimensions and the `width * height * channels` buffer size against the
/// budgets, then returns the zeroed buffer.
pub(super) fn pixel_buffer(width: u32, height: u32, channels: u64) -> Result<Vec<u8>> {
    check_dimensions(width, height)?;
    let bytes = u64::from(width) * u64::from(height) * channels;
    check_alloc(bytes)?;
    let bytes = usize::try_from(bytes).map_err(|_| DecodeError::TooLarge)?;
    Ok(vec![0u8; bytes])
}

fn size_mismatch() -> DecodeError {
    DecodeError::malformed("pixel buffer size mismatch")
}

pub(super) fn gray_image(w: u32, h: u32, buf: Vec<u8>) -> Result<DynamicImage> {
    GrayImage::from_raw(w, h, buf)
        .map(DynamicImage::ImageLuma8)
        .ok_or_else(size_mismatch)
}

pub(super) fn rgb_image(w: u32, h: u32, buf: Vec<u8>) -> Result<DynamicImage> {
    RgbImage::from_raw(w, h, buf)
        .map(DynamicImage::ImageRgb8)
        .ok_or_else(size_mismatch)
}

pub(super) fn rgba_image(w: u32, h: u32, buf: Vec<u8>) -> Result<DynamicImage> {
    RgbaImage::from_raw(w, h, buf)
        .map(DynamicImage::ImageRgba8)
        .ok_or_else(size_mismatch)
}

/// Builds an 8-bit gray, RGB or RGBA image from interleaved samples.
pub(super) fn interleaved_image(
    w: u32,
    h: u32,
    channels: usize,
    buf: Vec<u8>,
) -> Result<DynamicImage> {
    match channels {
        1 => gray_image(w, h, buf),
        3 => rgb_image(w, h, buf),
        4 => rgba_image(w, h, buf),
        _ => Err(DecodeError::malformed("unsupported channel count")),
    }
}

/// Reverses the row order of a `row_len`-byte-per-row buffer in place.
pub(super) fn flip_rows(buf: &mut [u8], row_len: usize) {
    if row_len == 0 {
        return;
    }
    let rows = buf.len() / row_len;
    for y in 0..rows / 2 {
        let (top, bottom) = buf.split_at_mut((rows - 1 - y) * row_len);
        top[y * row_len..(y + 1) * row_len].swap_with_slice(&mut bottom[..row_len]);
    }
}

/// Linear to sRGB transfer for a value in `[0, 1]`, returned as an 8-bit code.
pub(super) fn srgb_encode(linear: f32) -> u8 {
    let v = linear.clamp(0.0, 1.0);
    let s = if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0 + 0.5) as u8
}
