//! Sun rasterfile (`.ras`, `.sun`): 1, 8, 24 and 32 bits per pixel, old / standard /
//! byte-encoded (RLE, `0x80` escape) / RGB-ordered variants, optional RGB colour map.

use image::{DynamicImage, RgbImage};

use super::{DecodeError, Result, be32, check_alloc, check_dimensions, gray_image, pixel_bytes};

const MAGIC: u32 = 0x59A6_6A95;
const HEADER_LEN: usize = 32;
const RT_OLD: u32 = 0;
const RT_STANDARD: u32 = 1;
const RT_BYTE_ENCODED: u32 = 2;
const RT_FORMAT_RGB: u32 = 3;
const MAP_NONE: u32 = 0;
const MAP_RGB: u32 = 1;
const MAP_RAW: u32 = 2;

/// Expands the byte-encoded stream into exactly `out.len()` bytes.
fn unrle(src: &[u8], out: &mut [u8]) -> Result<()> {
    let truncated = || DecodeError::malformed("Sun raster RLE data truncated");
    let mut s = 0usize;
    let mut d = 0usize;
    while d < out.len() {
        let b = *src.get(s).ok_or_else(truncated)?;
        s += 1;
        if b != 0x80 {
            out[d] = b;
            d += 1;
            continue;
        }
        let count = *src.get(s).ok_or_else(truncated)?;
        s += 1;
        if count == 0 {
            out[d] = 0x80;
            d += 1;
        } else {
            let value = *src.get(s).ok_or_else(truncated)?;
            s += 1;
            let end = (d + usize::from(count) + 1).min(out.len());
            out[d..end].fill(value);
            d = end;
        }
    }
    Ok(())
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if be32(data, 0) != Some(MAGIC) {
        return Err(DecodeError::Unsupported);
    }
    let field = |i: usize| be32(data, 4 + i * 4);
    let (Some(width), Some(height), Some(depth), Some(_length)) =
        (field(0), field(1), field(2), field(3))
    else {
        return Err(DecodeError::malformed("Sun raster header truncated"));
    };
    let (Some(rtype), Some(maptype), Some(maplength)) = (field(4), field(5), field(6)) else {
        return Err(DecodeError::malformed("Sun raster header truncated"));
    };
    if !matches!(depth, 1 | 8 | 24 | 32) {
        return Err(DecodeError::malformed("unsupported Sun raster depth"));
    }
    if !matches!(
        rtype,
        RT_OLD | RT_STANDARD | RT_BYTE_ENCODED | RT_FORMAT_RGB
    ) {
        return Err(DecodeError::malformed("unsupported Sun raster type"));
    }
    check_dimensions(width, height)?;
    pixel_bytes(width, height, 3)?;

    // Row stride: scan lines are padded to 16 bits.
    let stride = u64::from(width) * u64::from(depth);
    let stride = usize::try_from(stride.div_ceil(16) * 2).map_err(|_| DecodeError::TooLarge)?;
    let raw_len = stride
        .checked_mul(height as usize)
        .ok_or(DecodeError::TooLarge)?;
    check_alloc(raw_len as u64)?;

    let map_len = maplength as usize;
    let body_at = HEADER_LEN
        .checked_add(map_len)
        .filter(|&at| at <= data.len())
        .ok_or_else(|| DecodeError::malformed("Sun raster colour map truncated"))?;
    let map_bytes = &data[HEADER_LEN..body_at];
    let palette: Option<(&[u8], &[u8], &[u8])> = match maptype {
        MAP_NONE => None,
        MAP_RGB => {
            let n = map_len / 3;
            if !map_len.is_multiple_of(3) || n > 256 {
                return Err(DecodeError::malformed("bad Sun raster colour map"));
            }
            Some((&map_bytes[..n], &map_bytes[n..2 * n], &map_bytes[2 * n..]))
        }
        MAP_RAW => None,
        _ => return Err(DecodeError::malformed("bad Sun raster map type")),
    };
    let body = &data[body_at..];

    let mut rows_buf;
    let raw: &[u8] = if rtype == RT_BYTE_ENCODED {
        // A three-byte run expands to at most 256 bytes: a larger claim is a lie.
        if raw_len as u64 > body.len() as u64 * 86 + 256 {
            return Err(DecodeError::malformed("Sun raster RLE data too short"));
        }
        rows_buf = vec![0u8; raw_len];
        unrle(body, &mut rows_buf)?;
        &rows_buf
    } else {
        body.get(..raw_len)
            .ok_or_else(|| DecodeError::malformed("Sun raster data truncated"))?
    };

    let (w, h) = (width as usize, height as usize);
    let lookup = |i: u8| -> [u8; 3] {
        match palette {
            Some((r, g, b)) => {
                let i = usize::from(i);
                [
                    r.get(i).copied().unwrap_or(0),
                    g.get(i).copied().unwrap_or(0),
                    b.get(i).copied().unwrap_or(0),
                ]
            }
            None => [i, i, i],
        }
    };

    // No palette on 1/8 bit: plain greyscale (1-bit: set bit is black).
    if palette.is_none() && depth <= 8 {
        let mut out = vec![0u8; w * h];
        for y in 0..h {
            let row = &raw[y * stride..(y + 1) * stride];
            let dst = &mut out[y * w..(y + 1) * w];
            if depth == 1 {
                for (x, px) in dst.iter_mut().enumerate() {
                    *px = if row[x / 8] >> (7 - x % 8) & 1 == 1 {
                        0
                    } else {
                        255
                    };
                }
            } else {
                dst.copy_from_slice(&row[..w]);
            }
        }
        return gray_image(width, height, out);
    }

    let mut out = vec![0u8; w * h * 3];
    for y in 0..h {
        let row = &raw[y * stride..(y + 1) * stride];
        let dst = &mut out[y * w * 3..(y + 1) * w * 3];
        for (x, px) in dst.chunks_exact_mut(3).enumerate() {
            let rgb = match depth {
                1 => lookup(row[x / 8] >> (7 - x % 8) & 1),
                8 => lookup(row[x]),
                24 => {
                    let p = &row[x * 3..x * 3 + 3];
                    if rtype == RT_FORMAT_RGB {
                        [p[0], p[1], p[2]]
                    } else {
                        [p[2], p[1], p[0]]
                    }
                }
                _ => {
                    // 32 bit: one pad / alpha byte first, then the three colour bytes.
                    let p = &row[x * 4 + 1..x * 4 + 4];
                    if rtype == RT_FORMAT_RGB {
                        [p[0], p[1], p[2]]
                    } else {
                        [p[2], p[1], p[0]]
                    }
                }
            };
            px.copy_from_slice(&rgb);
        }
    }
    RgbImage::from_raw(width, height, out)
        .map(DynamicImage::ImageRgb8)
        .ok_or_else(|| DecodeError::malformed("pixel buffer size mismatch"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(w: u32, h: u32, depth: u32, rtype: u32, maptype: u32, maplen: u32) -> Vec<u8> {
        let mut v = Vec::new();
        for x in [MAGIC, w, h, depth, 0, rtype, maptype, maplen] {
            v.extend_from_slice(&x.to_be_bytes());
        }
        v
    }

    #[test]
    fn bgr_24_standard_pads_rows_to_16_bits() {
        // 1x2: stride = ceil(24/16)*2 = 4 bytes.
        let mut f = header(1, 2, 24, RT_STANDARD, 0, 0);
        f.extend_from_slice(&[0, 0, 255, 0]); // BGR: red
        f.extend_from_slice(&[255, 0, 0, 0]); // blue
        let img = decode(&f).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (1, 2));
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0]);
        assert_eq!(img.get_pixel(0, 1).0, [0, 0, 255]);
    }

    #[test]
    fn rgb_ordered_32_bit() {
        let mut f = header(1, 1, 32, RT_FORMAT_RGB, 0, 0);
        f.extend_from_slice(&[0, 10, 20, 30]);
        assert_eq!(
            decode(&f).unwrap().to_rgb8().get_pixel(0, 0).0,
            [10, 20, 30]
        );
    }

    #[test]
    fn rle_8_bit_with_colour_map() {
        // 4x1, stride 4: map of 2 entries (r,r,g,g,b,b); data: 0x80 run of 3 x index 1, then 0x80 0 (literal 0x80 -> index 128 -> black).
        let mut f = header(4, 1, 8, RT_BYTE_ENCODED, MAP_RGB, 6);
        f.extend_from_slice(&[10, 200, 20, 100, 30, 50]);
        f.extend_from_slice(&[0x80, 2, 1, 0x80, 0]);
        let img = decode(&f).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (4, 1));
        assert_eq!(img.get_pixel(0, 0).0, [200, 100, 50]);
        assert_eq!(img.get_pixel(2, 0).0, [200, 100, 50]);
        assert_eq!(img.get_pixel(3, 0).0, [0, 0, 0]);
    }

    #[test]
    fn one_bit_without_map_is_black_on_set() {
        let mut f = header(3, 1, 1, RT_OLD, 0, 0);
        f.extend_from_slice(&[0b1010_0000, 0]);
        let img = decode(&f).unwrap().to_luma8();
        assert_eq!(img.get_pixel(0, 0).0, [0]);
        assert_eq!(img.get_pixel(1, 0).0, [255]);
        assert_eq!(img.get_pixel(2, 0).0, [0]);
    }

    #[test]
    fn hostile_inputs() {
        assert_eq!(decode(b"nope").unwrap_err(), DecodeError::Unsupported);
        assert!(matches!(
            decode(&header(1, 1, 24, 1, 0, 0)[..20]),
            Err(DecodeError::Malformed(_))
        ));
        assert_eq!(
            decode(&header(u32::MAX, u32::MAX, 24, 1, 0, 0)).unwrap_err(),
            DecodeError::TooLarge
        );
        assert_eq!(
            decode(&header(65535, 65535, 8, 2, 0, 0)).unwrap_err(),
            DecodeError::TooLarge
        );
        // Plausible size, RLE stream that ends early.
        let mut f = header(8000, 8000, 8, RT_BYTE_ENCODED, 0, 0);
        f.extend_from_slice(&[0x80, 255, 7]);
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        // Colour map longer than the file.
        assert!(matches!(
            decode(&header(1, 1, 8, 1, 1, 0xFFFF_FFF0)),
            Err(DecodeError::Malformed(_))
        ));
    }
}
