//! Portable Float Map (`.pfm`: `PF` colour / `Pf` grey, 32-bit floats) and Portable Half Map
//! (`.phm`: `PH` / `Ph`, 16-bit floats). Rows are stored bottom to top; a negative scale
//! means little-endian samples. HDR values are clamped to `[0, 1]` and sRGB-encoded.

use image::{DynamicImage, RgbImage};

use super::{DecodeError, Result, gray_image, pixel_bytes};

/// IEEE 754 binary16 to binary32.
fn half_to_f32(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0f32 } else { 1.0 };
    let exp = i32::from(h >> 10 & 0x1F);
    let frac = f32::from(h & 0x3FF);
    match exp {
        0 => sign * frac * 2f32.powi(-24),
        31 if frac == 0.0 => sign * f32::INFINITY,
        31 => f32::NAN,
        _ => sign * (1.0 + frac / 1024.0) * 2f32.powi(exp - 15),
    }
}

/// Clamp to `[0, 1]` (NaN -> 0) and encode with the sRGB transfer curve.
fn tone_map(v: f32) -> u8 {
    let v = if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) };
    let e = if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    };
    (e * 255.0 + 0.5) as u8
}

/// Next whitespace-delimited header token (bounded length) and the cursor after it.
fn token(data: &[u8], mut at: usize) -> Result<(&str, usize)> {
    let bad = || DecodeError::malformed("bad PFM header");
    while data.get(at).is_some_and(u8::is_ascii_whitespace) {
        at += 1;
    }
    let start = at;
    while data.get(at).is_some_and(|b| !b.is_ascii_whitespace()) {
        at += 1;
        if at - start > 32 {
            return Err(bad());
        }
    }
    let tok = std::str::from_utf8(&data[start..at]).map_err(|_| bad())?;
    if tok.is_empty() {
        return Err(bad());
    }
    Ok((tok, at))
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    let (channels, half) = match data.get(..3) {
        Some([b'P', b'F', c]) if c.is_ascii_whitespace() => (3usize, false),
        Some([b'P', b'f', c]) if c.is_ascii_whitespace() => (1, false),
        Some([b'P', b'H', c]) if c.is_ascii_whitespace() => (3, true),
        Some([b'P', b'h', c]) if c.is_ascii_whitespace() => (1, true),
        _ => return Err(DecodeError::Unsupported),
    };
    let bad = || DecodeError::malformed("bad PFM header");
    let (w_tok, at) = token(data, 2)?;
    let (h_tok, at) = token(data, at)?;
    let (s_tok, at) = token(data, at)?;
    let width: u32 = w_tok.parse().map_err(|_| bad())?;
    let height: u32 = h_tok.parse().map_err(|_| bad())?;
    let scale: f32 = s_tok.parse().map_err(|_| bad())?;
    let little = scale <= 0.0;
    // Exactly one whitespace byte separates the header from the samples.
    let samples = data.get(at + 1..).ok_or_else(bad)?;

    let sample_size = if half { 2usize } else { 4 };
    pixel_bytes(width, height, (channels * sample_size) as u64)?;
    let (w, h) = (width as usize, height as usize);
    let row_len = w * channels;
    let body = samples
        .get(..row_len * h * sample_size)
        .ok_or_else(|| DecodeError::malformed("PFM data truncated"))?;

    let mut out = vec![0u8; row_len * h];
    for (file_row, src) in body.chunks_exact(row_len * sample_size).enumerate() {
        let y = h - 1 - file_row;
        let dst = &mut out[y * row_len..(y + 1) * row_len];
        for (d, s) in dst.iter_mut().zip(src.chunks_exact(sample_size)) {
            let v = if half {
                let raw = [s[0], s[1]];
                half_to_f32(if little {
                    u16::from_le_bytes(raw)
                } else {
                    u16::from_be_bytes(raw)
                })
            } else {
                let raw = [s[0], s[1], s[2], s[3]];
                if little {
                    f32::from_le_bytes(raw)
                } else {
                    f32::from_be_bytes(raw)
                }
            };
            *d = tone_map(v);
        }
    }
    if channels == 1 {
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

    #[test]
    fn half_floats_convert() {
        assert_eq!(half_to_f32(0x3C00), 1.0);
        assert_eq!(half_to_f32(0xC000), -2.0);
        assert_eq!(half_to_f32(0x3800), 0.5);
        assert_eq!(half_to_f32(0), 0.0);
        assert!(half_to_f32(0x7E00).is_nan());
        assert_eq!(half_to_f32(0x7C00), f32::INFINITY);
    }

    #[test]
    fn colour_pfm_is_flipped_clamped_and_encoded() {
        // 2x2, little-endian (-1). File rows are bottom-up.
        let mut f = b"PF\n2 2\n-1.0\n".to_vec();
        let px = |r: f32, g: f32, b: f32| {
            [r, g, b]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<u8>>()
        };
        f.extend(px(1.0, 0.0, 0.0)); // bottom-left red
        f.extend(px(0.0, 4.0, 0.0)); // bottom-right: over-range green clamps
        f.extend(px(0.0, 0.0, 1.0)); // top-left blue
        f.extend(px(0.5, 0.5, 0.5)); // top-right mid grey -> sRGB 188
        let img = decode(&f).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (2, 2));
        assert_eq!(img.get_pixel(0, 1).0, [255, 0, 0]);
        assert_eq!(img.get_pixel(1, 1).0, [0, 255, 0]);
        assert_eq!(img.get_pixel(0, 0).0, [0, 0, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [188, 188, 188]);
    }

    #[test]
    fn big_endian_half_grey() {
        let mut f = b"Ph 2 1 1.0\n".to_vec();
        f.extend_from_slice(&0x3C00u16.to_be_bytes());
        f.extend_from_slice(&0x0000u16.to_be_bytes());
        let img = decode(&f).unwrap().to_luma8();
        assert_eq!(img.get_pixel(0, 0).0, [255]);
        assert_eq!(img.get_pixel(1, 0).0, [0]);
    }

    #[test]
    fn hostile_inputs() {
        assert_eq!(
            decode(b"P5\n1 1\n255\n").unwrap_err(),
            DecodeError::Unsupported
        );
        assert!(matches!(
            decode(b"PF\n2 2\n"),
            Err(DecodeError::Malformed(_))
        ));
        assert!(matches!(
            decode(b"PF\n2 2\n-1\n\0\0\0\0"),
            Err(DecodeError::Malformed(_))
        ));
        assert_eq!(
            decode(b"Pf\n4294967295 4294967295\n-1\n").unwrap_err(),
            DecodeError::TooLarge
        );
        assert_eq!(
            decode(b"PF\n65535 65535\n-1\n").unwrap_err(),
            DecodeError::TooLarge
        );
        // Within the pixel budget but not within the file.
        assert!(matches!(
            decode(b"Pf\n10000 10000\n-1\n\0"),
            Err(DecodeError::Malformed(_))
        ));
    }
}
