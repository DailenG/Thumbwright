//! Valve Texture Format (`.vtf`, versions 7.0 to 7.5): the largest mip of the first frame
//! and face is decoded for DXT1/DXT3/DXT5 and the common uncompressed layouts. Formats not
//! handled here (palettised, floating point, bump maps, 16-bit alpha variants) fall back
//! to the file's embedded low-resolution thumbnail.
//!
//! Layout: header, low-res thumbnail, then the mip chain smallest-first; each mip holds
//! `frames x faces x depth` images (7.3+ locates both images through a resource table).

use image::DynamicImage;

use super::{DecodeError, Result, le16, le32, pixel_bytes, rgba_image};

const FLAG_ENVMAP: u32 = 0x4000;
/// Upper bound on the mip count (a 65535-pixel edge has 16 levels).
const MAX_MIPS: u8 = 20;
const MAX_RESOURCES: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Rgba8888,
    Abgr8888,
    Rgb888,
    Bgr888,
    Rgb565,
    I8,
    Ia88,
    Argb8888,
    Bgra8888,
    Bgrx8888,
    Dxt1 { one_bit_alpha: bool },
    Dxt3,
    Dxt5,
}

fn format_from_code(code: i32) -> Option<Format> {
    Some(match code {
        0 => Format::Rgba8888,
        1 => Format::Abgr8888,
        2 => Format::Rgb888,
        3 => Format::Bgr888,
        4 => Format::Rgb565,
        5 => Format::I8,
        6 => Format::Ia88,
        11 => Format::Argb8888,
        12 => Format::Bgra8888,
        13 => Format::Dxt1 {
            one_bit_alpha: false,
        },
        14 => Format::Dxt3,
        15 => Format::Dxt5,
        16 => Format::Bgrx8888,
        20 => Format::Dxt1 {
            one_bit_alpha: true,
        },
        _ => return None,
    })
}

impl Format {
    /// Bytes of one `w x h` image in this format.
    fn image_size(self, w: u64, h: u64) -> u64 {
        let blocks = w.max(1).div_ceil(4) * h.max(1).div_ceil(4);
        match self {
            Self::Dxt1 { .. } => blocks * 8,
            Self::Dxt3 | Self::Dxt5 => blocks * 16,
            Self::Rgb888 | Self::Bgr888 => w * h * 3,
            Self::Rgb565 | Self::Ia88 => w * h * 2,
            Self::I8 => w * h,
            _ => w * h * 4,
        }
    }
}

fn expand565(v: u16) -> [u8; 3] {
    let (r, g, b) = ((v >> 11) & 0x1F, (v >> 5) & 0x3F, v & 0x1F);
    [
        (r << 3 | r >> 2) as u8,
        (g << 2 | g >> 4) as u8,
        (b << 3 | b >> 2) as u8,
    ]
}

/// The four colours of a DXT colour block. In three-colour mode (DXT1 only) index 3 is
/// transparent black.
fn dxt_palette(block: &[u8], dxt1: bool) -> [[u8; 4]; 4] {
    let c0v = u16::from_le_bytes([block[0], block[1]]);
    let c1v = u16::from_le_bytes([block[2], block[3]]);
    let (c0, c1) = (expand565(c0v), expand565(c1v));
    let mix = |a: u8, b: u8, wa: u32, wb: u32, div: u32| {
        ((u32::from(a) * wa + u32::from(b) * wb) / div) as u8
    };
    let mut pal = [[0, 0, 0, 255]; 4];
    pal[0] = [c0[0], c0[1], c0[2], 255];
    pal[1] = [c1[0], c1[1], c1[2], 255];
    if c0v > c1v || !dxt1 {
        for i in 0..3 {
            pal[2][i] = mix(c0[i], c1[i], 2, 1, 3);
            pal[3][i] = mix(c0[i], c1[i], 1, 2, 3);
        }
    } else {
        for i in 0..3 {
            pal[2][i] = mix(c0[i], c1[i], 1, 1, 2);
        }
        pal[3] = [0, 0, 0, 0];
    }
    pal
}

fn dxt5_alpha(block: &[u8]) -> [u8; 16] {
    let (a0, a1) = (u32::from(block[0]), u32::from(block[1]));
    let mut table = [0u8; 8];
    table[0] = a0 as u8;
    table[1] = a1 as u8;
    if a0 > a1 {
        for i in 1..7u32 {
            table[1 + i as usize] = (((7 - i) * a0 + i * a1) / 7) as u8;
        }
    } else {
        for i in 1..5u32 {
            table[1 + i as usize] = (((5 - i) * a0 + i * a1) / 5) as u8;
        }
        table[6] = 0;
        table[7] = 255;
    }
    let mut bits = 0u64;
    for (i, &b) in block[2..8].iter().enumerate() {
        bits |= u64::from(b) << (8 * i);
    }
    let mut out = [0u8; 16];
    for (i, a) in out.iter_mut().enumerate() {
        *a = table[(bits >> (3 * i) & 7) as usize];
    }
    out
}

/// Decodes one image to RGBA8; `src` is at least `format.image_size(w, h)` bytes.
fn decode_image(format: Format, w: usize, h: usize, src: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 4];
    match format {
        Format::Dxt1 { .. } | Format::Dxt3 | Format::Dxt5 => {
            let (block_len, color_at) = match format {
                Format::Dxt1 { .. } => (8, 0),
                _ => (16, 8),
            };
            let blocks_x = w.div_ceil(4);
            for (bi, block) in src.chunks_exact(block_len).enumerate() {
                let (bx, by) = (bi % blocks_x, bi / blocks_x);
                let one_bit = matches!(
                    format,
                    Format::Dxt1 {
                        one_bit_alpha: true
                    }
                );
                let pal = dxt_palette(&block[color_at..], matches!(format, Format::Dxt1 { .. }));
                let alpha = (format == Format::Dxt5).then(|| dxt5_alpha(block));
                let indices = u32::from_le_bytes([
                    block[color_at + 4],
                    block[color_at + 5],
                    block[color_at + 6],
                    block[color_at + 7],
                ]);
                for py in 0..4 {
                    for px in 0..4 {
                        let (x, y) = (bx * 4 + px, by * 4 + py);
                        if x >= w || y >= h {
                            continue;
                        }
                        let i = py * 4 + px;
                        let mut c = pal[(indices >> (2 * i) & 3) as usize];
                        match format {
                            Format::Dxt3 => {
                                let byte = block[i / 2];
                                let nib = if i % 2 == 0 { byte & 0xF } else { byte >> 4 };
                                c[3] = nib * 17;
                            }
                            Format::Dxt5 => c[3] = alpha.map_or(255, |a| a[i]),
                            _ => {
                                if !one_bit {
                                    c[3] = 255;
                                }
                            }
                        }
                        out[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&c);
                    }
                }
            }
        }
        _ => {
            let bpp = match format {
                Format::Rgb888 | Format::Bgr888 => 3,
                Format::Rgb565 | Format::Ia88 => 2,
                Format::I8 => 1,
                _ => 4,
            };
            for (p, dst) in src.chunks_exact(bpp).zip(out.chunks_exact_mut(4)) {
                let c = match format {
                    Format::Rgba8888 => [p[0], p[1], p[2], p[3]],
                    Format::Abgr8888 => [p[3], p[2], p[1], p[0]],
                    Format::Argb8888 => [p[1], p[2], p[3], p[0]],
                    Format::Bgra8888 => [p[2], p[1], p[0], p[3]],
                    Format::Bgrx8888 => [p[2], p[1], p[0], 255],
                    Format::Rgb888 => [p[0], p[1], p[2], 255],
                    Format::Bgr888 => [p[2], p[1], p[0], 255],
                    Format::Rgb565 => {
                        let c = expand565(u16::from_le_bytes([p[0], p[1]]));
                        [c[0], c[1], c[2], 255]
                    }
                    Format::I8 => [p[0], p[0], p[0], 255],
                    _ => [p[0], p[0], p[0], p[1]],
                };
                dst.copy_from_slice(&c);
            }
        }
    }
    out
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if data.get(..4) != Some(b"VTF\0") {
        return Err(DecodeError::Unsupported);
    }
    let bad = || DecodeError::malformed("VTF header truncated");
    let major = le32(data, 4).ok_or_else(bad)?;
    let minor = le32(data, 8).ok_or_else(bad)?;
    if major != 7 || minor > 5 {
        return Err(DecodeError::malformed("unsupported VTF version"));
    }
    let header_size = le32(data, 12).ok_or_else(bad)? as usize;
    let width = u32::from(le16(data, 16).ok_or_else(bad)?);
    let height = u32::from(le16(data, 18).ok_or_else(bad)?);
    let flags = le32(data, 20).ok_or_else(bad)?;
    let frames = u64::from(le16(data, 24).ok_or_else(bad)?.max(1));
    let first_frame = le16(data, 26).ok_or_else(bad)?;
    let high_code = le32(data, 52).ok_or_else(bad)? as i32;
    let mips = *data.get(56).ok_or_else(bad)?;
    let low_code = le32(data, 57).ok_or_else(bad)? as i32;
    let low_w = u32::from(*data.get(61).ok_or_else(bad)?);
    let low_h = u32::from(*data.get(62).ok_or_else(bad)?);
    let depth = if minor >= 2 {
        u64::from(le16(data, 63).ok_or_else(bad)?.max(1))
    } else {
        1
    };
    if mips == 0 || mips > MAX_MIPS {
        return Err(DecodeError::malformed("bad VTF mip count"));
    }
    pixel_bytes(width, height, 4)?;

    let has_low = low_code != -1 && low_w > 0 && low_h > 0;
    let low_format = format_from_code(low_code).filter(|_| has_low);
    if minor < 3 && has_low && low_format.is_none() {
        // The high-res image follows the thumbnail, so its size must be known.
        return Err(DecodeError::malformed("unsupported VTF thumbnail format"));
    }
    let low_len = low_format.map_or(0, |f| f.image_size(u64::from(low_w), u64::from(low_h)));

    // Locate the low-res and high-res images.
    let (low_at, high_at) = if minor >= 3 {
        let count = (le32(data, 68).ok_or_else(bad)? as usize).min(MAX_RESOURCES);
        let (mut low, mut high) = (None, None);
        for i in 0..count {
            let at = 80 + i * 8;
            let tag = data.get(at..at + 3).ok_or_else(bad)?;
            let offset = le32(data, at + 4).ok_or_else(bad)? as usize;
            match tag {
                [0x01, 0, 0] => low = Some(offset),
                [0x30, 0, 0] => high = Some(offset),
                _ => {}
            }
        }
        (low, high)
    } else {
        (Some(header_size), header_size.checked_add(low_len as usize))
    };

    let faces: u64 = if flags & FLAG_ENVMAP != 0 {
        if minor < 5 && first_frame != 0xFFFF {
            7
        } else {
            6
        }
    } else {
        1
    };

    // The thumbnail is the fallback whenever the main image cannot be decoded here.
    let thumbnail = || -> Result<DynamicImage> {
        let (Some(format), Some(at)) = (low_format, low_at) else {
            return Err(DecodeError::malformed("VTF format not supported"));
        };
        pixel_bytes(low_w, low_h, 4)?;
        let len = usize::try_from(format.image_size(u64::from(low_w), u64::from(low_h)))
            .map_err(|_| DecodeError::TooLarge)?;
        let src = at
            .checked_add(len)
            .and_then(|end| data.get(at..end))
            .ok_or_else(|| DecodeError::malformed("VTF thumbnail truncated"))?;
        rgba_image(
            low_w,
            low_h,
            decode_image(format, low_w as usize, low_h as usize, src),
        )
    };

    let (Some(format), Some(high_at)) = (format_from_code(high_code), high_at) else {
        return thumbnail();
    };
    // Skip the smaller mips (stored first) to reach level 0.
    let mut skip = 0u64;
    for level in 1..u32::from(mips) {
        let (mw, mh) = ((width >> level).max(1), (height >> level).max(1));
        let one = format.image_size(u64::from(mw), u64::from(mh));
        skip = skip
            .checked_add(
                one.checked_mul(frames * faces * depth)
                    .ok_or(DecodeError::TooLarge)?,
            )
            .ok_or(DecodeError::TooLarge)?;
    }
    let len = usize::try_from(format.image_size(u64::from(width), u64::from(height)))
        .map_err(|_| DecodeError::TooLarge)?;
    let start = usize::try_from(skip)
        .ok()
        .and_then(|s| s.checked_add(high_at))
        .ok_or_else(|| DecodeError::malformed("VTF image offset out of range"))?;
    let src = start
        .checked_add(len)
        .and_then(|end| data.get(start..end))
        .ok_or_else(|| DecodeError::malformed("VTF image data truncated"))?;
    rgba_image(
        width,
        height,
        decode_image(format, width as usize, height as usize, src),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 7.2 header (64 bytes + depth) with the given geometry and formats.
    fn header(w: u16, h: u16, high: i32, mips: u8, low: i32, low_w: u8, low_h: u8) -> Vec<u8> {
        let mut f = vec![0u8; 80];
        f[0..4].copy_from_slice(b"VTF\0");
        f[4..8].copy_from_slice(&7u32.to_le_bytes());
        f[8..12].copy_from_slice(&2u32.to_le_bytes());
        f[12..16].copy_from_slice(&80u32.to_le_bytes());
        f[16..18].copy_from_slice(&w.to_le_bytes());
        f[18..20].copy_from_slice(&h.to_le_bytes());
        f[24..26].copy_from_slice(&1u16.to_le_bytes());
        f[52..56].copy_from_slice(&high.to_le_bytes());
        f[56] = mips;
        f[57..61].copy_from_slice(&low.to_le_bytes());
        f[61] = low_w;
        f[62] = low_h;
        f[63..65].copy_from_slice(&1u16.to_le_bytes());
        f
    }

    #[test]
    fn rgba_largest_mip_is_found_after_the_smaller_ones() {
        // 2x2 RGBA8888, 2 mips: low-res DXT1 1x1 (8 bytes), mip 1 (1x1 = 4 bytes), mip 0.
        let mut f = header(2, 2, 0, 2, 13, 1, 1);
        f.extend_from_slice(&[0; 8]); // thumbnail
        f.extend_from_slice(&[9, 9, 9, 9]); // mip 1
        for p in [
            [255u8, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 128],
            [1, 2, 3, 4],
        ] {
            f.extend_from_slice(&p);
        }
        let img = decode(&f).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (2, 2));
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 255, 0, 255]);
        assert_eq!(img.get_pixel(0, 1).0, [0, 0, 255, 128]);
        assert_eq!(img.get_pixel(1, 1).0, [1, 2, 3, 4]);
    }

    #[test]
    fn dxt1_block_interpolates_four_colours() {
        // c0 = pure red (0xF800) > c1 = pure blue (0x001F): palette r, b, 2/3r+1/3b, 1/3r+2/3b.
        let mut f = header(4, 4, 13, 1, 13, 0, 0);
        let mut block = vec![0x00, 0xF8, 0x1F, 0x00];
        // Row 0 indices: 0, 1, 2, 3 (2 bits each, LSB first) = 0b11_10_01_00.
        block.extend_from_slice(&[0b1110_0100, 0, 0, 0]);
        f.extend(block);
        let img = decode(&f).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (4, 4));
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 0, 255, 255]);
        assert_eq!(img.get_pixel(2, 0).0, [170, 0, 85, 255]);
        assert_eq!(img.get_pixel(3, 0).0, [85, 0, 170, 255]);
        assert_eq!(img.get_pixel(0, 3).0, [255, 0, 0, 255]);
    }

    #[test]
    fn dxt5_alpha_block_and_dxt3_nibbles() {
        let colour = [0x00, 0xF8, 0x00, 0xF8, 0, 0, 0, 0];
        // DXT5: a0 = 255, a1 = 0, all indices 1 -> alpha 0.
        let mut f = header(4, 4, 15, 1, -1, 0, 0);
        f.extend_from_slice(&[
            255,
            0,
            0b0100_1001,
            0b1001_0010,
            0b0010_0100,
            0b0100_1001,
            0b1001_0010,
            0b0010_0100,
        ]);
        f.extend_from_slice(&colour);
        let img = decode(&f).unwrap().to_rgba8();
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 0]);

        // DXT3: first pixel alpha nibble 0xA -> 170.
        let mut f = header(4, 4, 14, 1, -1, 0, 0);
        f.extend_from_slice(&[0x0A, 0xF0, 0, 0, 0, 0, 0, 0]);
        f.extend_from_slice(&colour);
        let img = decode(&f).unwrap().to_rgba8();
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 170]);
        assert_eq!(img.get_pixel(1, 0).0, [255, 0, 0, 0]);
    }

    #[test]
    fn unsupported_format_falls_back_to_the_thumbnail() {
        // High-res format 7 (P8) is not decoded; the 4x4 DXT1 thumbnail is shown instead.
        let mut f = header(64, 64, 7, 1, 13, 4, 4);
        f.extend_from_slice(&[0x00, 0xF8, 0x00, 0xF8, 0, 0, 0, 0]);
        f.extend_from_slice(&[0; 64]);
        let img = decode(&f).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (4, 4));
        assert_eq!(img.get_pixel(2, 2).0, [255, 0, 0, 255]);
    }

    #[test]
    fn hostile_inputs() {
        assert_eq!(decode(b"VTX\0").unwrap_err(), DecodeError::Unsupported);
        assert!(matches!(
            decode(b"VTF\0\x07\0\0\0"),
            Err(DecodeError::Malformed(_))
        ));
        // 65535 x 65535 exceeds the pixel budget.
        let f = header(65535, 65535, 12, 1, -1, 0, 0);
        assert_eq!(decode(&f).unwrap_err(), DecodeError::TooLarge);
        // Plausible size, no pixel data.
        let f = header(4096, 4096, 12, 13, -1, 0, 0);
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        // Zero mip count and an absurd one.
        let f = header(4, 4, 12, 0, -1, 0, 0);
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        let f = header(4, 4, 12, 255, -1, 0, 0);
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
    }
}
