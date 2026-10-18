//! PGX (`.pgx`), the uncompressed JPEG 2000 verification format: one ASCII header line
//! `PG <ML|LM> <+|-> <depth> <width> <height>`, then one component of raw samples (1, 2 or
//! 4 bytes each by depth). Shown as 8-bit greyscale.

use image::DynamicImage;

use super::{DecodeError, Result, gray_image, pixel_bytes};

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if data.get(..2) != Some(b"PG") {
        return Err(DecodeError::Unsupported);
    }
    let nl = data
        .iter()
        .take(128)
        .position(|&b| b == b'\n')
        .ok_or_else(|| DecodeError::malformed("PGX header line missing"))?;
    let line = std::str::from_utf8(&data[..nl])
        .map_err(|_| DecodeError::malformed("PGX header is not text"))?;
    let samples = &data[nl + 1..];

    let mut tok = line.split_ascii_whitespace();
    let bad = || DecodeError::malformed("bad PGX header");
    tok.next(); // "PG"
    let big_endian = match tok.next().ok_or_else(bad)? {
        "ML" => true,
        "LM" => false,
        _ => return Err(bad()),
    };
    let signed = match tok.next().ok_or_else(bad)? {
        "+" => false,
        "-" => true,
        _ => return Err(bad()),
    };
    let mut next_num = || -> Result<u32> { tok.next().ok_or_else(bad)?.parse().map_err(|_| bad()) };
    let depth = next_num()?;
    let width = next_num()?;
    let height = next_num()?;
    if !(1..=32).contains(&depth) {
        return Err(DecodeError::malformed("unsupported PGX bit depth"));
    }
    let bps = match depth {
        1..=8 => 1usize,
        9..=16 => 2,
        _ => 4,
    };
    pixel_bytes(width, height, bps as u64)?;
    let count = width as usize * height as usize;
    let body = samples
        .get(..count * bps)
        .ok_or_else(|| DecodeError::malformed("PGX data truncated"))?;

    let max = (1u64 << depth) - 1;
    let sign_bit = 1u64 << (depth - 1);
    let out: Vec<u8> = body
        .chunks_exact(bps)
        .map(|c| {
            let mut v = 0u64;
            for i in 0..bps {
                let b = if big_endian { c[i] } else { c[bps - 1 - i] };
                v = v << 8 | u64::from(b);
            }
            v &= max;
            if signed {
                // Two's complement of `depth` bits, re-centred to an unsigned range.
                v ^= sign_bit;
            }
            (v * 255 / max) as u8
        })
        .collect();
    gray_image(width, height, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eight_bit_unsigned() {
        let mut f = b"PG ML + 8 3 1\n".to_vec();
        f.extend_from_slice(&[0, 128, 255]);
        let img = decode(&f).unwrap().to_luma8();
        assert_eq!(img.dimensions(), (3, 1));
        assert_eq!(img.get_pixel(0, 0).0, [0]);
        assert_eq!(img.get_pixel(1, 0).0, [128]);
        assert_eq!(img.get_pixel(2, 0).0, [255]);
    }

    #[test]
    fn sixteen_bit_little_endian_signed_is_recentred() {
        // 12-bit signed samples: -2048 -> 0, 0 -> 127 (2048*255/4095), 2047 -> 255.
        let mut f = b"PG LM - 12 3 1\r\n".to_vec();
        for v in [-2048i16, 0, 2047] {
            f.extend_from_slice(&v.to_le_bytes());
        }
        let img = decode(&f).unwrap().to_luma8();
        assert_eq!(img.get_pixel(0, 0).0, [0]);
        assert_eq!(img.get_pixel(1, 0).0, [127]);
        assert_eq!(img.get_pixel(2, 0).0, [255]);
    }

    #[test]
    fn hostile_inputs() {
        assert_eq!(
            decode(b"P5\n1 1\n255\n\0").unwrap_err(),
            DecodeError::Unsupported
        );
        assert!(matches!(
            decode(b"PG ML + 8 2 2"),
            Err(DecodeError::Malformed(_))
        ));
        assert!(matches!(
            decode(b"PG ML + 8 2 2\n\0\0"),
            Err(DecodeError::Malformed(_))
        ));
        assert_eq!(
            decode(b"PG ML + 8 4294967295 4294967295\n").unwrap_err(),
            DecodeError::TooLarge
        );
        assert_eq!(
            decode(b"PG ML + 16 65535 65535\n").unwrap_err(),
            DecodeError::TooLarge
        );
        assert!(matches!(
            decode(b"PG ML + 99 1 1\n\0"),
            Err(DecodeError::Malformed(_))
        ));
    }
}
