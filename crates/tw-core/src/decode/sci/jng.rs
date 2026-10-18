//! JNG (`.jng`): a baseline JPEG colour image (`JDAT`) with an optional alpha channel, held
//! either as a grayscale PNG (`IDAT`) or a grayscale JPEG (`JDAA`). 8-bit image data only.

use image::DynamicImage;

use super::mng::chunk_at;
use super::{DecodeError, Result, be32, check_alloc, check_dimensions};

const JNG_SIGNATURE: [u8; 8] = [0x8B, b'J', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if !data.starts_with(&JNG_SIGNATURE) {
        return Err(DecodeError::Unsupported);
    }
    decode_from(data, JNG_SIGNATURE.len())
}

/// Decodes the JNG object whose `JHDR` chunk starts at `pos` (also used for MNG streams).
pub(super) fn decode_from(data: &[u8], pos: usize) -> Result<DynamicImage> {
    let (jhdr, mut next) = chunk_at(data, pos)?
        .filter(|(c, _)| &c.ty == b"JHDR")
        .ok_or_else(|| DecodeError::malformed("JNG JHDR missing"))?;
    let h = jhdr.data;
    if h.len() < 16 {
        return Err(DecodeError::malformed("JNG JHDR truncated"));
    }
    let (width, height) = (be32(h, 0).unwrap_or(0), be32(h, 4).unwrap_or(0));
    let (color_type, sample_depth, compression) = (h[8], h[9], h[10]);
    let (alpha_depth, alpha_compression, alpha_interlace) = (h[12], h[13], h[15]);
    check_dimensions(width, height)?;
    check_alloc(u64::from(width) * u64::from(height) * 4)?;
    if sample_depth != 8 || compression != 8 || !matches!(color_type, 8 | 10 | 12 | 14) {
        return Err(DecodeError::Unsupported);
    }
    let has_alpha = matches!(color_type, 12 | 14) && alpha_depth != 0;

    let mut jdat: Vec<u8> = Vec::new();
    let mut jdaa: Vec<u8> = Vec::new();
    let mut idat: Vec<u8> = Vec::new();
    while let Some((chunk, after)) = chunk_at(data, next)? {
        match &chunk.ty {
            b"JDAT" => jdat.extend_from_slice(chunk.data),
            b"JDAA" => jdaa.extend_from_slice(chunk.data),
            b"IDAT" => idat.extend_from_slice(chunk.raw),
            b"IEND" | b"MEND" => break,
            _ => {}
        }
        next = after;
    }
    if jdat.is_empty() {
        return Err(DecodeError::malformed("JNG has no JDAT chunk"));
    }
    let color = super::super::decode_bytes(&jdat)?;
    if (color.width(), color.height()) != (width, height) {
        return Err(DecodeError::malformed("JNG JPEG size differs from JHDR"));
    }

    if !has_alpha {
        return Ok(match color_type {
            8 => DynamicImage::ImageLuma8(color.to_luma8()),
            _ => DynamicImage::ImageRgb8(color.to_rgb8()),
        });
    }
    let alpha = match alpha_compression {
        0 => {
            if idat.is_empty() {
                return Err(DecodeError::malformed("JNG alpha has no IDAT chunk"));
            }
            let mut ihdr = Vec::with_capacity(13);
            ihdr.extend(width.to_be_bytes());
            ihdr.extend(height.to_be_bytes());
            ihdr.extend([alpha_depth, 0, 0, 0, alpha_interlace]);
            let mut png = PNG_SIGNATURE.to_vec();
            png.extend(super::mng::make_chunk(b"IHDR", &ihdr));
            png.extend(idat);
            png.extend(super::mng::make_chunk(b"IEND", &[]));
            super::super::decode_limited(&png, image::ImageFormat::Png)?.0
        }
        8 => {
            if jdaa.is_empty() {
                return Err(DecodeError::malformed("JNG alpha has no JDAA chunk"));
            }
            super::super::decode_bytes(&jdaa)?
        }
        _ => return Err(DecodeError::Unsupported),
    };
    let alpha = alpha.to_luma8();
    if alpha.dimensions() != (width, height) {
        return Err(DecodeError::malformed("JNG alpha size differs from JHDR"));
    }
    let mut out = color.to_rgba8();
    for (px, a) in out.pixels_mut().zip(alpha.pixels()) {
        px.0[3] = a.0[0];
    }
    Ok(DynamicImage::ImageRgba8(out))
}

#[cfg(test)]
mod tests {
    use super::super::mng::make_chunk;
    use super::*;

    fn jpeg(img: &image::DynamicImage) -> Vec<u8> {
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 95)
            .encode_image(img)
            .unwrap();
        out
    }

    fn png_idat(img: &image::DynamicImage) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        let png = out.into_inner();
        let mut pos = 8;
        let mut idat = Vec::new();
        while let Some((c, next)) = chunk_at(&png, pos).unwrap() {
            if &c.ty == b"IDAT" {
                idat.extend_from_slice(c.raw);
            }
            pos = next;
        }
        idat
    }

    fn jhdr(w: u32, h: u32, color_type: u8, alpha_depth: u8, alpha_compression: u8) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend(w.to_be_bytes());
        d.extend(h.to_be_bytes());
        d.extend([color_type, 8, 8, 0, alpha_depth, alpha_compression, 0, 0]);
        make_chunk(b"JHDR", &d)
    }

    #[test]
    fn colour_with_png_alpha() {
        let color = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            8,
            8,
            image::Rgb([240, 20, 20]),
        ));
        let alpha = image::DynamicImage::ImageLuma8(image::GrayImage::from_fn(8, 8, |x, _| {
            image::Luma([if x < 4 { 255 } else { 0 }])
        }));
        let jpg = jpeg(&color);
        let (a, b) = jpg.split_at(jpg.len() / 2);
        let mut f = JNG_SIGNATURE.to_vec();
        f.extend(jhdr(8, 8, 14, 8, 0));
        f.extend(make_chunk(b"JDAT", a));
        f.extend(make_chunk(b"JDAT", b));
        f.extend(png_idat(&alpha));
        f.extend(make_chunk(b"IEND", &[]));
        let img = decode(&f).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (8, 8));
        let left = img.get_pixel(1, 4).0;
        assert!(left[0] > 220 && left[1] < 40 && left[3] == 255, "{left:?}");
        assert_eq!(img.get_pixel(6, 4).0[3], 0);
    }

    #[test]
    fn gray_with_jpeg_alpha() {
        let color = image::DynamicImage::ImageLuma8(image::GrayImage::from_pixel(
            16,
            16,
            image::Luma([90]),
        ));
        let alpha = image::DynamicImage::ImageLuma8(image::GrayImage::from_pixel(
            16,
            16,
            image::Luma([200]),
        ));
        let mut f = JNG_SIGNATURE.to_vec();
        f.extend(jhdr(16, 16, 12, 8, 8));
        f.extend(make_chunk(b"JDAT", &jpeg(&color)));
        f.extend(make_chunk(b"JDAA", &jpeg(&alpha)));
        f.extend(make_chunk(b"IEND", &[]));
        let img = decode(&f).unwrap().to_rgba8();
        let p = img.get_pixel(8, 8).0;
        assert!(
            (85..=95).contains(&p[0]) && p[0] == p[1] && (195..=205).contains(&p[3]),
            "{p:?}"
        );
    }

    #[test]
    fn opaque_colour_without_alpha() {
        let color = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            8,
            8,
            image::Rgb([10, 200, 10]),
        ));
        let mut f = JNG_SIGNATURE.to_vec();
        f.extend(jhdr(8, 8, 10, 0, 0));
        f.extend(make_chunk(b"JDAT", &jpeg(&color)));
        f.extend(make_chunk(b"IEND", &[]));
        let img = decode(&f).unwrap().to_rgb8();
        let p = img.get_pixel(3, 3).0;
        assert!(p[1] > 180 && p[0] < 40, "{p:?}");
    }

    #[test]
    fn hostile_inputs_are_rejected() {
        assert_eq!(decode(b"not jng"), Err(DecodeError::Unsupported));
        let mut f = JNG_SIGNATURE.to_vec();
        f.extend(jhdr(65535, 65535, 10, 0, 0));
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
        // Truncated JHDR chunk.
        let mut f = JNG_SIGNATURE.to_vec();
        f.extend(&jhdr(8, 8, 10, 0, 0)[..14]);
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        // Valid header, no image data.
        let mut f = JNG_SIGNATURE.to_vec();
        f.extend(jhdr(8, 8, 10, 0, 0));
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        // JPEG whose size disagrees with JHDR.
        let color = image::DynamicImage::ImageRgb8(image::RgbImage::new(8, 8));
        let mut f = JNG_SIGNATURE.to_vec();
        f.extend(jhdr(16, 16, 10, 0, 0));
        f.extend(make_chunk(b"JDAT", &jpeg(&color)));
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
    }
}
