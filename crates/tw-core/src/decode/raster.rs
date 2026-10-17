//! Tier 1: memory-safe raster decoders from the `image` crate.

use image::{DynamicImage, ImageFormat};

use super::{DecodeError, Result, check_dimensions, decode_limited};

/// Extensions decoded here, keyed to the `image` format that handles them.
pub fn format_for_ext(ext: &str) -> Option<ImageFormat> {
    Some(match ext {
        "png" | "apng" => ImageFormat::Png,
        "jpg" | "jpeg" | "jpe" | "jfif" | "jps" | "mpo" => ImageFormat::Jpeg,
        "gif" => ImageFormat::Gif,
        "bmp" | "dib" => ImageFormat::Bmp,
        "ico" | "icon" | "cur" => ImageFormat::Ico,
        "tga" | "icb" | "vda" | "vst" | "tpic" => ImageFormat::Tga,
        "webp" => ImageFormat::WebP,
        "qoi" => ImageFormat::Qoi,
        "dds" | "dxt1" | "dxt5" => ImageFormat::Dds,
        "exr" | "cxr" => ImageFormat::OpenExr,
        "hdr" | "rgbe" | "hdri" => ImageFormat::Hdr,
        "pbm" | "pgm" | "ppm" | "pnm" | "pam" => ImageFormat::Pnm,
        "ff" | "farbfeld" => ImageFormat::Farbfeld,
        "tif" | "tiff" | "ptif" | "tiff64" => ImageFormat::Tiff,
        _ => return None,
    })
}

fn be16(d: &[u8], i: usize) -> Option<u32> {
    Some(u32::from(u16::from_be_bytes(
        d.get(i..i + 2)?.try_into().ok()?,
    )))
}

fn le16(d: &[u8], i: usize) -> Option<u32> {
    Some(u32::from(u16::from_le_bytes(
        d.get(i..i + 2)?.try_into().ok()?,
    )))
}

fn be32(d: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(i..i + 4)?.try_into().ok()?))
}

fn le24(d: &[u8], i: usize) -> Option<u32> {
    let b = d.get(i..i + 3)?;
    Some(u32::from(b[0]) | u32::from(b[1]) << 8 | u32::from(b[2]) << 16)
}

/// Dimensions declared by the file header, read without constructing a decoder, so a
/// decompression bomb is rejected even when the rest of the file is missing.
pub fn header_dimensions(d: &[u8], format: ImageFormat) -> Option<(u32, u32)> {
    match format {
        ImageFormat::Png if d.get(12..16) == Some(b"IHDR") => Some((be32(d, 16)?, be32(d, 20)?)),
        ImageFormat::Gif => Some((le16(d, 6)?, le16(d, 8)?)),
        ImageFormat::Qoi => Some((be32(d, 4)?, be32(d, 8)?)),
        ImageFormat::Bmp => {
            let header = u32::from_le_bytes(d.get(14..18)?.try_into().ok()?);
            if header >= 40 {
                let w = i32::from_le_bytes(d.get(18..22)?.try_into().ok()?);
                let h = i32::from_le_bytes(d.get(22..26)?.try_into().ok()?);
                Some((w.unsigned_abs(), h.unsigned_abs()))
            } else {
                Some((le16(d, 18)?, le16(d, 20)?))
            }
        }
        ImageFormat::WebP => match d.get(12..16)? {
            b"VP8X" => Some((le24(d, 24)? + 1, le24(d, 27)? + 1)),
            b"VP8L" => {
                let bits = u32::from_le_bytes(d.get(21..25)?.try_into().ok()?);
                Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))
            }
            b"VP8 " => Some((le16(d, 26)? & 0x3FFF, le16(d, 28)? & 0x3FFF)),
            _ => None,
        },
        ImageFormat::Jpeg => {
            let mut i = 2;
            while i + 9 < d.len() {
                if d[i] != 0xFF {
                    return None;
                }
                let m = d[i + 1];
                if m == 0xFF {
                    i += 1;
                    continue;
                }
                if (0xC0..=0xCF).contains(&m) && !matches!(m, 0xC4 | 0xC8 | 0xCC) {
                    return Some((be16(d, i + 7)?, be16(d, i + 5)?));
                }
                i += 2 + be16(d, i + 2)? as usize;
            }
            None
        }
        _ => None,
    }
}

/// Decodes `data` as `format`, honouring EXIF orientation, after checking the
/// declared dimensions against the pixel budget.
pub fn decode_as(data: &[u8], format: ImageFormat) -> Result<DynamicImage> {
    if let Some((w, h)) = header_dimensions(data, format) {
        check_dimensions(w, h)?;
    }
    let (mut img, orientation) = decode_limited(data, format)?;
    if let Some(o) = orientation {
        img.apply_orientation(o);
    }
    Ok(img)
}

/// Content sniffing first (an extension can lie), extension as the fallback hint.
pub fn decode(data: &[u8], ext: Option<&str>) -> Result<DynamicImage> {
    let format = image::guess_format(data)
        .ok()
        .or_else(|| ext.and_then(format_for_ext))
        .ok_or(DecodeError::Unsupported)?;
    decode_as(data, format)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    /// 33-byte PNG: signature + IHDR declaring 65535 x 65535, no image data.
    fn png_bomb() -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&13u32.to_be_bytes());
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&65535u32.to_be_bytes());
        v.extend_from_slice(&65535u32.to_be_bytes());
        v.extend_from_slice(&[8, 6, 0, 0, 0]);
        let crc = crc32(&v[12..29]);
        v.extend_from_slice(&crc.to_be_bytes());
        v
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &b in data {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
        }
        !c
    }

    #[test]
    fn decompression_bomb_rejected_before_allocation() {
        let bomb = png_bomb();
        assert_eq!(bomb.len(), 33);
        let start = std::time::Instant::now();
        assert_eq!(decode(&bomb, Some("png")), Err(DecodeError::TooLarge));
        assert!(start.elapsed().as_secs() < 1);
    }

    #[test]
    fn content_wins_over_extension() {
        let mut png = Vec::new();
        image::RgbImage::new(3, 2)
            .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .unwrap();
        let img = decode(&png, Some("jpg")).unwrap();
        assert_eq!((img.width(), img.height()), (3, 2));
    }

    #[test]
    fn garbage_is_not_accepted() {
        assert!(decode(&[0x42; 64], Some("xyz")).is_err());
        assert!(decode(&[], Some("png")).is_err());
    }

    /// 16-byte farbfeld header: 20000 x 20000 is under the pixel-area budget, but the
    /// RGBA16 buffer (3.2 GB) is not; it must be refused before allocation.
    fn farbfeld_bomb() -> Vec<u8> {
        let mut v = b"farbfeld".to_vec();
        v.extend_from_slice(&20000u32.to_be_bytes());
        v.extend_from_slice(&20000u32.to_be_bytes());
        v
    }

    #[test]
    fn farbfeld_header_bomb_is_too_large() {
        let bomb = farbfeld_bomb();
        assert_eq!(bomb.len(), 16);
        assert_eq!(decode(&bomb, Some("ff")), Err(DecodeError::TooLarge));
        assert_eq!(
            super::super::decode_bytes(&bomb),
            Err(DecodeError::TooLarge)
        );
    }
}
