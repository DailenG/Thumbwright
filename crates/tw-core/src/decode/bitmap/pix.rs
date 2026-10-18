//! Alias/Wavefront PIX (`.pix`): a 10-byte big-endian header (width, height, x offset,
//! y offset, bits per pixel = 8 or 24), then `(count, pixel)` runs; 24-bit pixels are BGR.

use image::{DynamicImage, RgbImage};

use super::{DecodeError, Result, be16, gray_image, pixel_bytes};

const HEADER_LEN: usize = 10;

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    let (Some(width), Some(height), Some(depth)) = (be16(data, 0), be16(data, 2), be16(data, 8))
    else {
        return Err(DecodeError::Unsupported);
    };
    if !matches!(depth, 8 | 24) {
        return Err(DecodeError::Unsupported);
    }
    let (width, height) = (u32::from(width), u32::from(height));
    let bpp = usize::from(depth / 8);
    pixel_bytes(width, height, bpp as u64)?;
    let total = width as usize * height as usize;

    let runs = &data[HEADER_LEN..];
    // Each `(count, pixel)` run covers at most 255 pixels.
    if total as u64 > (runs.len() / (1 + bpp)) as u64 * 255 {
        return Err(DecodeError::malformed("PIX data too short for its size"));
    }
    let mut out = vec![0u8; total * bpp];
    let mut done = 0usize;
    let mut at = 0usize;
    while done < total {
        let (Some(&count), Some(px)) = (runs.get(at), runs.get(at + 1..at + 1 + bpp)) else {
            return Err(DecodeError::malformed("PIX data truncated"));
        };
        at += 1 + bpp;
        if count == 0 {
            return Err(DecodeError::malformed("PIX run of zero length"));
        }
        let n = usize::from(count).min(total - done);
        for dst in out[done * bpp..(done + n) * bpp].chunks_exact_mut(bpp) {
            if bpp == 3 {
                dst.copy_from_slice(&[px[2], px[1], px[0]]);
            } else {
                dst[0] = px[0];
            }
        }
        done += n;
    }
    if bpp == 1 {
        gray_image(width, height, out)
    } else {
        RgbImage::from_raw(width, height, out)
            .map(DynamicImage::ImageRgb8)
            .ok_or_else(|| DecodeError::malformed("pixel buffer size mismatch"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(w: u16, h: u16, depth: u16) -> Vec<u8> {
        let mut v = Vec::new();
        for x in [w, h, 0, 0, depth] {
            v.extend_from_slice(&x.to_be_bytes());
        }
        v
    }

    #[test]
    fn rgb_runs_are_bgr_ordered() {
        let mut f = header(3, 2, 24);
        f.extend_from_slice(&[2, 0, 0, 255]); // 2 red pixels
        f.extend_from_slice(&[4, 255, 0, 0]); // 4 blue pixels
        let img = decode(&f).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (3, 2));
        assert_eq!(img.get_pixel(1, 0).0, [255, 0, 0]);
        assert_eq!(img.get_pixel(2, 0).0, [0, 0, 255]);
        assert_eq!(img.get_pixel(2, 1).0, [0, 0, 255]);
    }

    #[test]
    fn grayscale_8_bit() {
        let mut f = header(2, 1, 8);
        f.extend_from_slice(&[1, 10, 1, 200]);
        let img = decode(&f).unwrap().to_luma8();
        assert_eq!(img.get_pixel(0, 0).0, [10]);
        assert_eq!(img.get_pixel(1, 0).0, [200]);
    }

    #[test]
    fn hostile_inputs() {
        assert_eq!(decode(&[0, 1]).unwrap_err(), DecodeError::Unsupported);
        assert_eq!(
            decode(&header(1, 1, 16)).unwrap_err(),
            DecodeError::Unsupported
        );
        let mut f = header(2, 2, 24);
        f.extend_from_slice(&[2, 0, 0, 255]);
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        assert_eq!(
            decode(&header(65535, 65535, 24)).unwrap_err(),
            DecodeError::TooLarge
        );
        let mut f = header(60000, 5000, 8);
        f.extend_from_slice(&[255, 1]);
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        let mut f = header(1, 1, 8);
        f.extend_from_slice(&[0, 1]);
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
    }
}
