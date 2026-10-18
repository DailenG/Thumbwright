//! PlayStation TIM (`.tim`): magic `0x10`, a flags word (pixel mode in bits 0-1, CLUT present
//! in bit 3), an optional CLUT block and the image block (`size, x, y, width-in-16-bit-words,
//! height`). 4/8-bit indexed (first CLUT row), 16-bit ABGR1555 and 24-bit RGB.
//! The 16-bit value `0x0000` is the format's transparent colour.

use image::DynamicImage;

use super::{DecodeError, Result, le16, le32, pixel_bytes, rgba_image};

const MAGIC: u32 = 0x10;
const FLAG_CLUT: u32 = 0x08;

/// Block header: size (u32), x, y, width, height (u16 each); returns the block's pixel
/// data (`width * 2 * height` bytes), width in words, height, and the offset after the block.
fn block(data: &[u8], at: usize) -> Result<(&[u8], usize, usize, usize)> {
    let bad = || DecodeError::malformed("TIM block truncated");
    let size = le32(data, at).ok_or_else(bad)? as usize;
    let words = usize::from(le16(data, at + 8).ok_or_else(bad)?);
    let rows = usize::from(le16(data, at + 10).ok_or_else(bad)?);
    let len = words * 2 * rows;
    let body = data.get(at + 12..at + 12 + len).ok_or_else(bad)?;
    let next = at
        .checked_add(size)
        .filter(|&n| n >= at + 12)
        .ok_or_else(bad)?;
    Ok((body, words, rows, next))
}

fn rgb555(v: u16) -> [u8; 4] {
    let ex = |c: u16| {
        let c = (c & 0x1F) as u8;
        c << 3 | c >> 2
    };
    [ex(v), ex(v >> 5), ex(v >> 10), if v == 0 { 0 } else { 255 }]
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if le32(data, 0) != Some(MAGIC) {
        return Err(DecodeError::Unsupported);
    }
    let flags = le32(data, 4).ok_or_else(|| DecodeError::malformed("TIM header truncated"))?;
    let mode = flags & 7;
    if mode > 3 {
        return Err(DecodeError::malformed("unsupported TIM pixel mode"));
    }
    let mut at = 8usize;
    let mut clut: &[u8] = &[];
    if flags & FLAG_CLUT != 0 {
        let (body, _, _, next) = block(data, at)?;
        clut = body;
        at = next;
    }
    let (pix, words, rows, _) = block(data, at)?;

    let width = match mode {
        0 => words * 4,
        1 => words * 2,
        2 => words,
        _ => words * 2 / 3,
    };
    let (w32, h32) = (
        u32::try_from(width).map_err(|_| DecodeError::TooLarge)?,
        u32::try_from(rows).map_err(|_| DecodeError::TooLarge)?,
    );
    pixel_bytes(w32, h32, 4)?;
    if mode < 2 && clut.len() < 2 {
        return Err(DecodeError::malformed("indexed TIM without a CLUT"));
    }
    let entry = |i: usize| rgb555(le16(clut, i * 2).unwrap_or(0));

    let row_bytes = words * 2;
    let mut out = vec![0u8; width * rows * 4];
    for (y, src) in pix.chunks_exact(row_bytes).enumerate() {
        let dst = &mut out[y * width * 4..(y + 1) * width * 4];
        for (x, px) in dst.chunks_exact_mut(4).enumerate() {
            let c = match mode {
                0 => entry(usize::from(src[x / 2] >> (4 * (x % 2)) & 0xF)),
                1 => entry(usize::from(src[x])),
                2 => rgb555(u16::from_le_bytes([src[x * 2], src[x * 2 + 1]])),
                _ => [src[x * 3], src[x * 3 + 1], src[x * 3 + 2], 255],
            };
            px.copy_from_slice(&c);
        }
    }
    rgba_image(w32, h32, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tim(flags: u32, clut: Option<&[u16]>, words: u16, rows: u16, pix: &[u8]) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(&MAGIC.to_le_bytes());
        f.extend_from_slice(&flags.to_le_bytes());
        if let Some(c) = clut {
            f.extend_from_slice(&(12 + c.len() as u32 * 2).to_le_bytes());
            f.extend_from_slice(&[0, 0, 0, 0]);
            f.extend_from_slice(&(c.len() as u16).to_le_bytes());
            f.extend_from_slice(&1u16.to_le_bytes());
            for v in c {
                f.extend_from_slice(&v.to_le_bytes());
            }
        }
        f.extend_from_slice(&(12 + pix.len() as u32).to_le_bytes());
        f.extend_from_slice(&[0, 0, 0, 0]);
        f.extend_from_slice(&words.to_le_bytes());
        f.extend_from_slice(&rows.to_le_bytes());
        f.extend_from_slice(pix);
        f
    }

    #[test]
    fn four_bit_indexed_uses_low_nibble_first() {
        // CLUT: 0 = transparent, 1 = red (0x001F), 2 = blue (0x7C00). 4 pixels: 1 2 0 1.
        let f = tim(0x08, Some(&[0, 0x001F, 0x7C00]), 1, 1, &[0x21, 0x10]);
        let img = decode(&f).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (4, 1));
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 0, 255, 255]);
        assert_eq!(img.get_pixel(2, 0).0[3], 0);
        assert_eq!(img.get_pixel(3, 0).0, [255, 0, 0, 255]);
    }

    #[test]
    fn sixteen_bit_direct_and_eight_bit_indexed() {
        let f = tim(0x02, None, 2, 1, &[0x1F, 0x00, 0x00, 0x80]);
        let img = decode(&f).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (2, 1));
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 0, 0, 255]); // 0x8000: opaque black

        let f = tim(0x09, Some(&[0x03E0, 0x7FFF]), 1, 1, &[1, 0]);
        let img = decode(&f).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (2, 1));
        assert_eq!(img.get_pixel(0, 0).0, [255, 255, 255, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 255, 0, 255]);
    }

    #[test]
    fn twenty_four_bit_rgb() {
        // 3 words = 6 bytes = 2 pixels.
        let f = tim(0x03, None, 3, 1, &[1, 2, 3, 4, 5, 6]);
        let img = decode(&f).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (2, 1));
        assert_eq!(img.get_pixel(1, 0).0, [4, 5, 6, 255]);
    }

    #[test]
    fn hostile_inputs() {
        assert_eq!(decode(b"\x11\0\0\0").unwrap_err(), DecodeError::Unsupported);
        assert!(matches!(
            decode(&[0x10, 0, 0, 0, 2]),
            Err(DecodeError::Malformed(_))
        ));
        let f = tim(0x02, None, 2, 1, &[0; 4]);
        assert!(matches!(
            decode(&f[..f.len() - 2]),
            Err(DecodeError::Malformed(_))
        ));
        // 65535 words x 65535 rows declared with no data: the payload check fails first.
        let mut f = tim(0x00, Some(&[0]), 0, 0, &[]);
        let n = f.len();
        f[n - 4..n - 2].copy_from_slice(&0xFFFFu16.to_le_bytes());
        f[n - 2..].copy_from_slice(&0xFFFFu16.to_le_bytes());
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        // Indexed image without a CLUT.
        let f = tim(0x01, None, 1, 1, &[0, 0]);
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
    }
}
