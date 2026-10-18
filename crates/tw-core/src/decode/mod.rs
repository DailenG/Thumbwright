//! Decoding pipeline: format routing (`route`), the individual decoders, and the
//! conversion of a decoded image into the premultiplied BGRA thumbnail the shell needs.
//!
//! Every decoder takes the whole file as a byte slice (the shell only has an `IStream`)
//! and returns an [`image::DynamicImage`]. Decoders MUST:
//! - check declared dimensions against [`check_dimensions`] before allocating pixels,
//! - cap decompressed sizes of embedded entries ([`crate::limits`]),
//! - return [`DecodeError::Unsupported`] when the input is simply not theirs, so the
//!   next tier can try.

pub mod audio;
pub mod j2k;
pub mod jxl;
pub mod pcx;
pub mod psd;
pub mod raster;
pub mod raw;
pub mod route;
pub mod sci;
pub mod sgi;
pub mod svg;
#[cfg(windows)]
pub mod wic;
pub mod zip_preview;

use image::DynamicImage;

use crate::limits;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// No decoder in this tier accepts the input; the next tier may try.
    Unsupported,
    /// The input is damaged or uses an unsupported variant; the next tier may try.
    Malformed(String),
    /// Declared dimensions or expansion exceed the budgets. Never retried (hostile input).
    TooLarge,
    /// A time budget expired. Never retried (hostile input).
    Timeout,
}

impl DecodeError {
    /// REQ-TMO-1: only plain decode failures fall through to the next tier.
    pub fn falls_through(&self) -> bool {
        matches!(self, Self::Unsupported | Self::Malformed(_))
    }

    pub fn malformed(msg: impl std::fmt::Display) -> Self {
        Self::Malformed(msg.to_string())
    }
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => f.write_str("unsupported input"),
            Self::Malformed(m) => write!(f, "malformed input: {m}"),
            Self::TooLarge => f.write_str("input exceeds resource budget"),
            Self::Timeout => f.write_str("decode time budget exceeded"),
        }
    }
}

impl std::error::Error for DecodeError {}

pub type Result<T, E = DecodeError> = std::result::Result<T, E>;

impl From<image::ImageError> for DecodeError {
    fn from(e: image::ImageError) -> Self {
        match e {
            image::ImageError::Limits(_) => Self::TooLarge,
            image::ImageError::Unsupported(_) => Self::Unsupported,
            other => Self::Malformed(other.to_string()),
        }
    }
}

/// Rejects zero or over-budget dimensions before any pixel allocation.
pub fn check_dimensions(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        return Err(DecodeError::malformed("zero dimension"));
    }
    if u64::from(width) * u64::from(height) > limits::MAX_PIXEL_AREA {
        return Err(DecodeError::TooLarge);
    }
    Ok(())
}

/// Allocation limits applied to every `image` decoder.
pub fn image_limits() -> image::Limits {
    let mut l = image::Limits::default();
    l.max_alloc = Some(limits::DECODE_ALLOC_BYTES);
    l
}

/// Rejects a single decode buffer larger than the decode byte budget, before allocating.
pub fn check_alloc(bytes: u64) -> Result<()> {
    if bytes > limits::DECODE_ALLOC_BYTES {
        return Err(DecodeError::TooLarge);
    }
    Ok(())
}

/// Decodes `data` as `format` with [`image_limits`], reserving the whole output buffer
/// against the budget before it is allocated (as `ImageReader::decode` does, which
/// `DynamicImage::from_decoder` alone does not). Returns the decoder's EXIF orientation.
pub fn decode_limited(
    data: &[u8],
    format: image::ImageFormat,
) -> Result<(DynamicImage, Option<image::metadata::Orientation>)> {
    use image::ImageDecoder;

    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(data), format);
    reader.limits(image_limits());
    let mut decoder = reader.into_decoder()?;
    let (w, h) = decoder.dimensions();
    check_dimensions(w, h)?;
    let orientation = decoder.orientation().ok();
    let mut budget = image_limits();
    budget.reserve(decoder.total_bytes())?;
    decoder.set_limits(budget)?;
    Ok((DynamicImage::from_decoder(decoder)?, orientation))
}

/// Decodes any raster format the `image` crate recognises by content (used both for
/// standalone files and for previews embedded in containers).
pub fn decode_bytes(data: &[u8]) -> Result<DynamicImage> {
    let format = image::guess_format(data).map_err(|_| DecodeError::Unsupported)?;
    raster::decode_as(data, format)
}

/// Thumbnail pixels: premultiplied BGRA8, top-down rows, stride `width * 4`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// Shrinks (never enlarges) so the longer edge is at most `edge`, then converts to
/// premultiplied BGRA.
pub fn to_thumbnail(img: &DynamicImage, edge: u32) -> Thumbnail {
    let edge = edge.max(1);
    let resized;
    let img = if img.width() > edge || img.height() > edge {
        resized = img.thumbnail(edge, edge);
        &resized
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    let mut bgra = rgba.into_raw();
    let (pixels, _) = bgra.as_chunks_mut::<4>();
    for px in pixels {
        let a = u16::from(px[3]);
        let premul = |c: u8| ((u16::from(c) * a + 127) / 255) as u8;
        *px = [premul(px[2]), premul(px[1]), premul(px[0]), px[3]];
    }
    Thumbnail {
        width,
        height,
        bgra,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    #[test]
    fn thumbnail_premultiplies_and_swaps_channels() {
        let img = DynamicImage::ImageRgba8(RgbaImage::from_pixel(2, 1, Rgba([255, 128, 0, 128])));
        let t = to_thumbnail(&img, 256);
        assert_eq!((t.width, t.height), (2, 1));
        assert_eq!(&t.bgra[..4], &[0, 64, 128, 128]);
    }

    #[test]
    fn thumbnail_shrinks_long_edge_and_keeps_aspect() {
        let img = DynamicImage::ImageRgba8(RgbaImage::new(1000, 250));
        let t = to_thumbnail(&img, 100);
        assert_eq!((t.width, t.height), (100, 25));
        assert_eq!(t.bgra.len(), 100 * 25 * 4);
    }

    #[test]
    fn dimension_guard() {
        assert_eq!(check_dimensions(65535, 65535), Err(DecodeError::TooLarge));
        assert!(matches!(
            check_dimensions(0, 5),
            Err(DecodeError::Malformed(_))
        ));
        assert!(check_dimensions(20000, 20000).is_ok());
    }
}
