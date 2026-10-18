//! Kodak Photo CD Image Pac (`.pcd`, `.ph`): a fixed layout of 2048-byte sectors. The
//! largest of Base/16 (192x128), Base/4 (384x256) and Base (768x512) that is completely
//! present is decoded from Photo YCC and rotated according to the header.
//!
//! Each resolution is stored as groups of two scan lines: luma of line 2k, luma of line
//! 2k+1, then the C1 and C2 chroma rows at half horizontal resolution.

use image::DynamicImage;

use super::{DecodeError, Result, pixel_buffer, rgb_image};

const SIGNATURE_AT: usize = 0x800;
const SIGNATURE: &[u8] = b"PCD_IPI";
const ROTATION_AT: usize = 0x0E02;
const MIN_LEN: usize = 0x2000;

/// `(data offset, width, height)` from largest to smallest.
const RESOLUTIONS: [(usize, usize, usize); 3] =
    [(0x30000, 768, 512), (0xB800, 384, 256), (0x2000, 192, 128)];

fn plane_len(w: usize, h: usize) -> usize {
    h / 2 * (w * 2 + w)
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if data.get(SIGNATURE_AT..SIGNATURE_AT + SIGNATURE.len()) != Some(SIGNATURE) {
        return Err(DecodeError::Unsupported);
    }
    if data.len() < MIN_LEN {
        return Err(DecodeError::malformed("Photo CD header truncated"));
    }
    let (offset, w, h) = RESOLUTIONS
        .into_iter()
        .find(|&(off, w, h)| data.len() >= off + plane_len(w, h))
        .ok_or_else(|| DecodeError::malformed("Photo CD image data truncated"))?;
    let rotation = data[ROTATION_AT] & 3;

    let mut out = pixel_buffer(w as u32, h as u32, 3)?;
    let group = w * 3; // 2 * w luma + 2 * (w / 2) chroma bytes
    for (g, chunk) in data[offset..offset + plane_len(w, h)]
        .chunks_exact(group)
        .enumerate()
    {
        let (luma, chroma) = chunk.split_at(2 * w);
        let (c1, c2) = chroma.split_at(w / 2);
        for row in 0..2 {
            let y = g * 2 + row;
            for x in 0..w {
                let luma = f32::from(luma[row * w + x]) * 1.402 / 255.0;
                let r = luma + (f32::from(c2[x / 2]) - 137.0) / 135.64;
                let b = luma + (f32::from(c1[x / 2]) - 156.0) / 111.40;
                let g_ = (luma - 0.299 * r - 0.114 * b) / 0.587;
                let at = (y * w + x) * 3;
                for (o, v) in out[at..at + 3].iter_mut().zip([r, g_, b]) {
                    *o = (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                }
            }
        }
    }
    let img = rgb_image(w as u32, h as u32, out)?;
    Ok(match rotation {
        1 => img.rotate90(),
        2 => img.rotate180(),
        3 => img.rotate270(),
        _ => img,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base16(rotation: u8) -> Vec<u8> {
        let mut f = vec![0u8; 0x2000 + plane_len(192, 128)];
        f[SIGNATURE_AT..SIGNATURE_AT + SIGNATURE.len()].copy_from_slice(SIGNATURE);
        f[ROTATION_AT] = rotation;
        // Group 0 (lines 0 and 1): reference white. Group 1 (lines 2 and 3): saturated red.
        // Other groups: black (luma 0, neutral chroma).
        for g in 0..64 {
            let (y, c1, c2) = match g {
                0 => (182u8, 156u8, 137u8),
                1 => (54, 123, 232),
                _ => (0, 156, 137),
            };
            let at = 0x2000 + g * 576;
            f[at..at + 384].fill(y);
            f[at + 384..at + 480].fill(c1);
            f[at + 480..at + 576].fill(c2);
        }
        f
    }

    #[test]
    fn base_over_16_decodes_ycc() {
        let img = decode(&base16(0)).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (192, 128));
        let white = img.get_pixel(10, 1).0;
        assert!(white.iter().all(|c| *c >= 253), "{white:?}");
        let red = img.get_pixel(100, 2).0;
        assert!(red[0] > 240 && red[1] < 20 && red[2] < 20, "{red:?}");
        assert_eq!(img.get_pixel(5, 100).0, [0, 0, 0]);
    }

    #[test]
    fn rotation_flag_turns_the_image() {
        let img = decode(&base16(1)).unwrap();
        assert_eq!((img.width(), img.height()), (128, 192));
        // Rotating 90 degrees clockwise moves the top-left white pixel to the top-right.
        let rgb = img.to_rgb8();
        assert!(rgb.get_pixel(127, 5).0.iter().all(|c| *c >= 253));
    }

    #[test]
    fn largest_complete_resolution_is_chosen() {
        let mut f = base16(0);
        f[SIGNATURE_AT..SIGNATURE_AT + SIGNATURE.len()].copy_from_slice(SIGNATURE);
        f.resize(0xB800 + plane_len(384, 256), 0);
        let img = decode(&f).unwrap();
        assert_eq!((img.width(), img.height()), (384, 256));
    }

    #[test]
    fn hostile_inputs_are_rejected() {
        assert_eq!(decode(b"not a photo cd"), Err(DecodeError::Unsupported));
        let mut header = vec![0u8; 0x900];
        header[SIGNATURE_AT..SIGNATURE_AT + SIGNATURE.len()].copy_from_slice(SIGNATURE);
        assert!(matches!(decode(&header), Err(DecodeError::Malformed(_))));
        // Header present, image data one byte short.
        let mut f = base16(0);
        f.pop();
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
    }
}
