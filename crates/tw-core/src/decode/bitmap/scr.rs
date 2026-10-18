//! ZX-Spectrum `SCREEN$` dump (`.scr`): exactly 6912 bytes, a 6144-byte bitmap in the
//! machine's interleaved line order followed by 768 colour attributes (one per 8x8 cell).
//! Other `.scr` files (screensavers) are not this format and fall through.

use image::DynamicImage;

use super::{DecodeError, Result, rgba_image};

const SIZE: usize = 6912;
const BITMAP_LEN: usize = 6144;
const WIDTH: u32 = 256;
const HEIGHT: u32 = 192;

/// Palette entry for colour bits `GRB` (bit 0 blue, bit 1 red, bit 2 green).
fn color(index: u8, bright: bool) -> [u8; 4] {
    let level = if bright { 0xFF } else { 0xCD };
    let on = |bit: u8| if index >> bit & 1 == 1 { level } else { 0 };
    [on(1), on(2), on(0), 255]
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if data.len() != SIZE {
        return Err(DecodeError::Unsupported);
    }
    let mut out = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
    for y in 0..HEIGHT as usize {
        // Line address: bits y7 y6 | y2 y1 y0 | y5 y4 y3 | x4..x0.
        let line = (y & 0xC0) << 5 | (y & 0x07) << 8 | (y & 0x38) << 2;
        for xb in 0..32usize {
            let bits = data[line | xb];
            let attr = data[BITMAP_LEN + (y / 8) * 32 + xb];
            let bright = attr & 0x40 != 0;
            let ink = color(attr & 7, bright);
            let paper = color(attr >> 3 & 7, bright);
            for bit in 0..8usize {
                let c = if bits >> (7 - bit) & 1 == 1 {
                    ink
                } else {
                    paper
                };
                let at = (y * WIDTH as usize + xb * 8 + bit) * 4;
                out[at..at + 4].copy_from_slice(&c);
            }
        }
    }
    rgba_image(WIDTH, HEIGHT, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleaved_lines_and_attributes() {
        let mut f = vec![0u8; SIZE];
        // Scanline y = 9 lives at (0<<11) | (1<<8) | (1<<5) = 0x120; set its first pixel.
        f[0x120] = 0x80;
        // Cell (row 1, col 0): bright ink red (010), paper blue (001).
        f[BITMAP_LEN + 32] = 0x40 | 0b001_010;
        let img = decode(&f).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (256, 192));
        assert_eq!(img.get_pixel(0, 9).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(1, 9).0, [0, 0, 255, 255]);
        // Scanline 8 starts at 0x020 and is untouched: paper only.
        assert_eq!(img.get_pixel(0, 8).0, [0, 0, 255, 255]);
        // Outside the attributed cell: black paper.
        assert_eq!(img.get_pixel(255, 191).0, [0, 0, 0, 255]);
    }

    #[test]
    fn only_exact_size_is_accepted() {
        assert_eq!(
            decode(&[0u8; SIZE - 1]).unwrap_err(),
            DecodeError::Unsupported
        );
        assert_eq!(
            decode(&[0u8; SIZE + 1]).unwrap_err(),
            DecodeError::Unsupported
        );
        assert_eq!(decode(&[]).unwrap_err(), DecodeError::Unsupported);
    }

    #[test]
    fn normal_intensity_is_dimmer() {
        assert_eq!(color(0b110, false), [0xCD, 0xCD, 0, 255]);
        assert_eq!(color(0b111, true), [255, 255, 255, 255]);
    }
}
