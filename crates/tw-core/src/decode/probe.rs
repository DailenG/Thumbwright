//! Header-only image facts for the property handler: dimensions, bit depth and EXIF values,
//! read without decoding a single pixel. Covers the `image` crate raster formats only
//! (`raster::format_for_ext`); other routes (vector, container, OS-codec) have no cheap,
//! memory-safe header read here and expose no properties.

use std::io::Cursor;

use image::ImageDecoder;

use super::exif::{self, Exif};
use super::{DecodeError, Result, image_limits, raster};

#[derive(Clone, Debug, PartialEq)]
pub struct ImageInfo {
    pub width: u32,
    pub height: u32,
    /// Bits per pixel of the file's own colour model (24 for an RGB JPEG).
    pub bits_per_pixel: u32,
    pub exif: Exif,
}

/// Reads the facts of `data`; the format comes from the content, with `ext` as the fallback.
pub fn probe(data: &[u8], ext: Option<&str>) -> Result<ImageInfo> {
    let format = image::guess_format(data)
        .ok()
        .or_else(|| ext.and_then(raster::format_for_ext))
        .ok_or(DecodeError::Unsupported)?;
    let mut reader = image::ImageReader::with_format(Cursor::new(data), format);
    reader.limits(image_limits());
    let mut decoder = reader.into_decoder()?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 {
        return Err(DecodeError::malformed("zero dimension"));
    }
    let bits_per_pixel = u32::from(decoder.original_color_type().bits_per_pixel());
    let exif = decoder
        .exif_metadata()
        .ok()
        .flatten()
        .map(|block| exif::parse(&block))
        .unwrap_or_default();
    Ok(ImageInfo {
        width,
        height,
        bits_per_pixel,
        exif,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32, color: image::ExtendedColorType) -> Vec<u8> {
        let mut v = Vec::new();
        let px = vec![0u8; (w * h) as usize * color.bits_per_pixel() as usize / 8];
        image::codecs::png::PngEncoder::new(&mut v)
            .write_image(&px, w, h, color)
            .unwrap();
        v
    }

    use image::ImageEncoder;

    #[test]
    fn png_dimensions_and_depth() {
        let info = probe(&png(7, 3, image::ExtendedColorType::Rgba8), Some("png")).unwrap();
        assert_eq!((info.width, info.height, info.bits_per_pixel), (7, 3, 32));
        let info = probe(&png(2, 5, image::ExtendedColorType::L8), None).unwrap();
        assert_eq!((info.width, info.height, info.bits_per_pixel), (2, 5, 8));
    }

    #[test]
    fn declared_bomb_is_rejected_quickly_without_pixels() {
        // 33-byte PNG declaring 65535 x 65535 and no pixel data (its IHDR CRC is wrong, so
        // the PNG layer refuses it): the probe fails fast and never materialises pixels.
        let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        v.extend_from_slice(&65535u32.to_be_bytes());
        v.extend_from_slice(&65535u32.to_be_bytes());
        v.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        let started = std::time::Instant::now();
        assert!(probe(&v, Some("png")).is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn garbage_is_unsupported() {
        assert!(probe(b"definitely not an image", Some("png")).is_err());
        assert!(probe(&[], None).is_err());
    }
}
