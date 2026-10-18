//! SMPTE 268M DPX (`.dpx`): image element 0 only. Luma/single-channel, RGB, RGBA and ABGR
//! at 8, 10, 12 and 16 bits with the packed and filled (methods A and B) layouts.
//! Samples are scaled to 8 bits without any transfer-function conversion.

use image::DynamicImage;

use super::{
    DecodeError, Order, Result, check_alloc, check_dimensions, interleaved_image, pixel_buffer,
};

const MAGIC_BE: &[u8; 4] = b"SDPX";
const MAGIC_LE: &[u8; 4] = b"XPDS";
const HEADER_LEN: usize = 852;

fn u8_at(d: &[u8], at: usize) -> Option<u8> {
    d.get(at).copied()
}

/// Unpacks `out.len()` samples from one image row.
fn unpack_row(row: &[u8], order: Order, bits: u32, packing: u16, out: &mut [u16]) {
    let word = |i: usize| order.u32(row, i * 4).unwrap_or(0);
    match (bits, packing) {
        (8, _) => {
            for (o, b) in out.iter_mut().zip(row) {
                *o = u16::from(*b);
            }
        }
        (16, _) => {
            for (i, o) in out.iter_mut().enumerate() {
                *o = order.u16(row, i * 2).unwrap_or(0);
            }
        }
        (12, 1) => {
            for (i, o) in out.iter_mut().enumerate() {
                *o = order.u16(row, i * 2).unwrap_or(0) >> 4;
            }
        }
        (12, 2) => {
            for (i, o) in out.iter_mut().enumerate() {
                *o = order.u16(row, i * 2).unwrap_or(0) & 0x0FFF;
            }
        }
        (10, 1 | 2) => {
            let shifts: [u32; 3] = if packing == 1 {
                [22, 12, 2]
            } else {
                [20, 10, 0]
            };
            for (i, o) in out.iter_mut().enumerate() {
                *o = ((word(i / 3) >> shifts[i % 3]) & 0x3FF) as u16;
            }
        }
        _ => {
            // Packed: samples are consecutive bit fields, most significant bit first within
            // each 32-bit word.
            let mask = (1u64 << bits) - 1;
            for (i, o) in out.iter_mut().enumerate() {
                let pos = i as u64 * u64::from(bits);
                let (w, off) = ((pos / 32) as usize, (pos % 32) as u32);
                let window = u64::from(word(w)) << 32 | u64::from(word(w + 1));
                *o = ((window >> (64 - off - bits)) & mask) as u16;
            }
        }
    }
}

fn row_bytes(width: u64, channels: u64, bits: u32, packing: u16) -> Option<u64> {
    let samples = width.checked_mul(channels)?;
    let bytes = match (bits, packing) {
        (8, _) => samples,
        (16, _) | (12, 1 | 2) => samples.checked_mul(2)?,
        (10, 1 | 2) => samples.div_ceil(3).checked_mul(4)?,
        _ => samples
            .checked_mul(u64::from(bits))?
            .div_ceil(32)
            .checked_mul(4)?,
    };
    // Every line starts on a 32-bit boundary.
    bytes.checked_add(3).map(|b| b / 4 * 4)
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    let order = match data.get(..4) {
        Some(m) if m == MAGIC_BE => Order::Big,
        Some(m) if m == MAGIC_LE => Order::Little,
        _ => return Err(DecodeError::Unsupported),
    };
    if data.len() < HEADER_LEN {
        return Err(DecodeError::malformed("DPX header truncated"));
    }
    let hdr = |at| order.u32(data, at).unwrap_or(0);
    let file_offset = hdr(4);
    let orientation = order.u16(data, 768).unwrap_or(0);
    let elements = order.u16(data, 770).unwrap_or(0);
    let (width, height) = (hdr(772), hdr(776));
    let descriptor = u8_at(data, 800).unwrap_or(0);
    let bits = u32::from(u8_at(data, 803).unwrap_or(0));
    let packing = order.u16(data, 804).unwrap_or(0);
    let encoding = order.u16(data, 806).unwrap_or(0);
    let element_offset = hdr(808);
    let eol_padding = u64::from(hdr(812));

    if elements == 0 {
        return Err(DecodeError::malformed("DPX has no image element"));
    }
    if encoding != 0 {
        return Err(DecodeError::Unsupported);
    }
    let channels: usize = match descriptor {
        1..=4 | 6 | 7 => 1,
        50 => 3,
        51 | 52 => 4,
        _ => return Err(DecodeError::Unsupported),
    };
    if !matches!(bits, 8 | 10 | 12 | 16) || ((bits == 10 || bits == 12) && packing > 2) {
        return Err(DecodeError::Unsupported);
    }
    check_dimensions(width, height)?;
    check_alloc(u64::from(width) * u64::from(height) * channels as u64)?;

    let offset = if element_offset != 0 {
        element_offset
    } else {
        file_offset
    };
    let line =
        row_bytes(u64::from(width), channels as u64, bits, packing).ok_or(DecodeError::TooLarge)?;
    let stride = line.checked_add(eol_padding).ok_or(DecodeError::TooLarge)?;
    let needed = stride
        .checked_mul(u64::from(height) - 1)
        .and_then(|v| v.checked_add(line))
        .and_then(|v| v.checked_add(u64::from(offset)))
        .ok_or(DecodeError::TooLarge)?;
    if needed > data.len() as u64 {
        return Err(DecodeError::malformed("DPX image data truncated"));
    }
    let (offset, line, stride) = (offset as usize, line as usize, stride as usize);

    let (w, h) = (width as usize, height as usize);
    let max = (1u32 << bits) - 1;
    let to8 = |v: u16| ((u32::from(v) * 255 + max / 2) / max).min(255) as u8;
    let mut out = pixel_buffer(width, height, channels as u64)?;
    let mut samples = vec![0u16; w * channels];
    for y in 0..h {
        let row = &data[offset + y * stride..offset + y * stride + line];
        unpack_row(row, order, bits, packing, &mut samples);
        let dst = &mut out[y * w * channels..(y + 1) * w * channels];
        if descriptor == 52 {
            for (d, s) in dst
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(samples.as_chunks::<4>().0)
            {
                d.copy_from_slice(&[to8(s[3]), to8(s[2]), to8(s[1]), to8(s[0])]);
            }
        } else {
            for (d, s) in dst.iter_mut().zip(&samples) {
                *d = to8(*s);
            }
        }
    }
    let mut img = interleaved_image(width, height, channels, out)?;
    // Orientation: bit 0 = right to left, bit 1 = bottom to top (transposed layouts ignored).
    if orientation < 4 {
        if orientation & 1 != 0 {
            img = img.fliph();
        }
        if orientation & 2 != 0 {
            img = img.flipv();
        }
    }
    Ok(img)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(order: Order, w: u32, h: u32, desc: u8, bits: u8, packing: u16) -> Vec<u8> {
        let mut f = vec![0u8; 2048];
        let put32 = |f: &mut Vec<u8>, at: usize, v: u32| {
            f[at..at + 4].copy_from_slice(&match order {
                Order::Big => v.to_be_bytes(),
                Order::Little => v.to_le_bytes(),
            });
        };
        let put16 = |f: &mut Vec<u8>, at: usize, v: u16| {
            f[at..at + 2].copy_from_slice(&match order {
                Order::Big => v.to_be_bytes(),
                Order::Little => v.to_le_bytes(),
            });
        };
        f[..4].copy_from_slice(if order == Order::Big {
            MAGIC_BE
        } else {
            MAGIC_LE
        });
        put32(&mut f, 4, 2048);
        put16(&mut f, 770, 1);
        put32(&mut f, 772, w);
        put32(&mut f, 776, h);
        f[800] = desc;
        f[803] = bits;
        put16(&mut f, 804, packing);
        put32(&mut f, 808, 2048);
        f
    }

    #[test]
    fn ten_bit_filled_method_a_rgb_big_endian() {
        let mut f = header(Order::Big, 2, 1, 50, 10, 1);
        // Pixel 0: R=1023 G=0 B=0 ; pixel 1: R=0 G=1023 B=512 (method A: shifts 22/12/2).
        f.extend((1023u32 << 22).to_be_bytes());
        f.extend(((1023u32 << 12) | (512 << 2)).to_be_bytes());
        let img = decode(&f).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (2, 1));
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 255, 128]);
    }

    #[test]
    fn ten_bit_method_b_and_packed() {
        // Method B, one pixel R=1 G=2 B=1023 (low-justified).
        let mut f = header(Order::Little, 1, 1, 50, 10, 2);
        f.extend(((1u32 << 20) | (2 << 10) | 1023).to_le_bytes());
        assert_eq!(decode(&f).unwrap().to_rgb8().get_pixel(0, 0).0, [0, 0, 255]);
        // Packed: four 10-bit gray samples straddle two words.
        let mut f = header(Order::Big, 4, 1, 6, 10, 0);
        let bits: u64 = (1023 << 54) | (1023 << 34);
        f.extend(((bits >> 32) as u32).to_be_bytes());
        f.extend((bits as u32).to_be_bytes());
        let img = decode(&f).unwrap().to_luma8();
        assert_eq!(img.as_raw(), &[255, 0, 255, 0]);
    }

    #[test]
    fn sixteen_bit_abgr_little_endian_top_down() {
        let mut f = header(Order::Little, 1, 2, 52, 16, 0);
        for px in [[0xFFFFu16, 0, 0, 0xFFFF], [0xFFFF, 0, 0xFFFF, 0]] {
            for s in px {
                f.extend(s.to_le_bytes());
            }
        }
        let img = decode(&f).unwrap().to_rgba8();
        // File order is A, B, G, R.
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(0, 1).0, [0, 255, 0, 255]);
    }

    #[test]
    fn hostile_inputs_are_rejected() {
        assert_eq!(decode(b"nope"), Err(DecodeError::Unsupported));
        assert!(matches!(decode(MAGIC_BE), Err(DecodeError::Malformed(_))));
        assert_eq!(
            decode(&header(Order::Big, 65535, 65535, 51, 16, 0)),
            Err(DecodeError::TooLarge)
        );
        assert_eq!(
            decode(&header(Order::Big, u32::MAX, u32::MAX, 51, 16, 0)),
            Err(DecodeError::TooLarge)
        );
        // 20000 x 20000 RGBA exceeds the buffer budget before any data is looked at.
        assert_eq!(
            decode(&header(Order::Big, 20000, 20000, 51, 8, 0)),
            Err(DecodeError::TooLarge)
        );
        // Plausible size but no pixel data.
        assert!(matches!(
            decode(&header(Order::Big, 4000, 4000, 50, 10, 1)),
            Err(DecodeError::Malformed(_))
        ));
    }
}
