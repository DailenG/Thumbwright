//! Khoros VIFF / XV (`.viff`, `.xv`): 1024-byte header, an optional colour map, then
//! uncompressed band-sequential 8-bit data. One band (gray, or colour mapped), 3 bands
//! (RGB) or 4 bands (RGBA).

use image::DynamicImage;

use super::{
    DecodeError, Order, Result, check_alloc, check_dimensions, gray_image, pixel_buffer, rgb_image,
    rgba_image, slice,
};

const HEADER_LEN: usize = 1024;
const MAGIC: u8 = 0xAB;
const MAX_MAP_ENTRIES: u32 = 1 << 16;

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if data.first() != Some(&MAGIC) || data.get(1) != Some(&0x01) {
        return Err(DecodeError::Unsupported);
    }
    if data.len() < HEADER_LEN {
        return Err(DecodeError::malformed("VIFF header truncated"));
    }
    // Byte order is MSB first only for the IEEE flag; DEC and NS orders are LSB first.
    let order = if data[4] == 0x02 {
        Order::Big
    } else {
        Order::Little
    };
    let field = |at: usize| order.u32(data, at).unwrap_or(0);
    let (width, height) = (field(520), field(524));
    let bands = field(560);
    let storage = field(564);
    let encoding = field(568);
    let map_scheme = field(572);
    let map_storage = field(576);
    let (map_bands, map_entries) = (field(580), field(584));

    if encoding != 0 || storage != 1 || !matches!(bands, 1 | 3 | 4) {
        return Err(DecodeError::Unsupported);
    }
    check_dimensions(width, height)?;
    let plane = u64::from(width) * u64::from(height);
    check_alloc(plane * u64::from(bands))?;

    // Colour map: `map_bands` runs of `map_entries` values, in the file before the pixels.
    let mut pos = HEADER_LEN;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    if map_scheme != 0 && map_storage != 0 {
        let elem = match map_storage {
            1 => 1usize,
            2 => 2,
            _ => return Err(DecodeError::Unsupported),
        };
        if !matches!(map_bands, 1 | 3) || map_entries == 0 || map_entries > MAX_MAP_ENTRIES {
            return Err(DecodeError::Unsupported);
        }
        let entries = map_entries as usize;
        let bytes = elem * entries * map_bands as usize;
        let map = slice(data, pos, bytes)
            .ok_or_else(|| DecodeError::malformed("VIFF colour map truncated"))?;
        pos += bytes;
        let value = |band: usize, i: usize| -> u8 {
            let at = (band * entries + i) * elem;
            if elem == 1 {
                map[at]
            } else {
                (order.u16(map, at).unwrap_or(0) >> 8) as u8
            }
        };
        palette = (0..entries)
            .map(|i| {
                if map_bands == 1 {
                    [value(0, i); 3]
                } else {
                    [value(0, i), value(1, i), value(2, i)]
                }
            })
            .collect();
    }

    let (w, h) = (width as usize, height as usize);
    let n = w * h;
    let needed = n.checked_mul(bands as usize).ok_or(DecodeError::TooLarge)?;
    let pixels = data
        .get(pos..)
        .and_then(|d| d.get(..needed))
        .ok_or_else(|| DecodeError::malformed("VIFF pixel data truncated"))?;

    match (bands, palette.is_empty()) {
        (1, true) => gray_image(width, height, pixels.to_vec()),
        (1, false) => {
            let mut out = pixel_buffer(width, height, 3)?;
            for (o, &i) in out.as_chunks_mut::<3>().0.iter_mut().zip(pixels) {
                o.copy_from_slice(&palette.get(usize::from(i)).copied().unwrap_or([0, 0, 0]));
            }
            rgb_image(width, height, out)
        }
        (3 | 4, _) => {
            let channels = bands as usize;
            let mut out = pixel_buffer(width, height, channels as u64)?;
            for (i, o) in out.chunks_exact_mut(channels).enumerate() {
                for (c, d) in o.iter_mut().enumerate() {
                    *d = pixels[c * n + i];
                }
            }
            if channels == 3 {
                rgb_image(width, height, out)
            } else {
                rgba_image(width, height, out)
            }
        }
        _ => Err(DecodeError::Unsupported),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(order: Order, w: u32, h: u32, bands: u32) -> Vec<u8> {
        let mut f = vec![0u8; HEADER_LEN];
        f[0] = MAGIC;
        f[1] = 1;
        f[4] = if order == Order::Big { 0x02 } else { 0x08 };
        let put = |f: &mut Vec<u8>, at: usize, v: u32| {
            f[at..at + 4].copy_from_slice(&match order {
                Order::Big => v.to_be_bytes(),
                Order::Little => v.to_le_bytes(),
            });
        };
        put(&mut f, 520, w);
        put(&mut f, 524, h);
        put(&mut f, 560, bands);
        put(&mut f, 564, 1);
        f
    }

    #[test]
    fn gray_and_planar_rgb() {
        let mut f = header(Order::Little, 2, 2, 1);
        f.extend([0, 64, 128, 255]);
        let img = decode(&f).unwrap().to_luma8();
        assert_eq!(img.dimensions(), (2, 2));
        assert_eq!(img.as_raw(), &[0, 64, 128, 255]);

        let mut f = header(Order::Big, 2, 1, 3);
        f.extend([255, 0, 0, 255, 0, 0]); // R plane, G plane, B plane
        let img = decode(&f).unwrap().to_rgb8();
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 255, 0]);
    }

    #[test]
    fn colour_mapped_image() {
        let mut f = header(Order::Big, 2, 1, 1);
        let put =
            |f: &mut Vec<u8>, at: usize, v: u32| f[at..at + 4].copy_from_slice(&v.to_be_bytes());
        put(&mut f, 572, 1); // one map per band
        put(&mut f, 576, 1); // byte map values
        put(&mut f, 580, 3);
        put(&mut f, 584, 2);
        f.extend([10, 200, 20, 210, 30, 220]); // R0 R1 G0 G1 B0 B1
        f.extend([1, 0]);
        let img = decode(&f).unwrap().to_rgb8();
        assert_eq!(img.get_pixel(0, 0).0, [200, 210, 220]);
        assert_eq!(img.get_pixel(1, 0).0, [10, 20, 30]);
    }

    #[test]
    fn hostile_inputs_are_rejected() {
        assert_eq!(decode(b"BM"), Err(DecodeError::Unsupported));
        assert!(matches!(
            decode(&[MAGIC, 1, 0, 0]),
            Err(DecodeError::Malformed(_))
        ));
        assert_eq!(
            decode(&header(Order::Big, 65535, 65535, 3)),
            Err(DecodeError::TooLarge)
        );
        assert_eq!(
            decode(&header(Order::Little, 20000, 20000, 4)),
            Err(DecodeError::TooLarge)
        );
        assert!(matches!(
            decode(&header(Order::Big, 3000, 3000, 3)),
            Err(DecodeError::Malformed(_))
        ));
    }
}
