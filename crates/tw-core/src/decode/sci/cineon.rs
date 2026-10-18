//! Kodak Cineon (`.cin`): big-endian, pixel-interleaved gray or RGB at 8, 10 or 16 bits.
//! 10-bit film scans are log-encoded printing densities; they are converted to display
//! values with the standard reference black 95 / white 685 curve.

use image::DynamicImage;

use super::{
    DecodeError, Order, Result, check_alloc, check_dimensions, interleaved_image, pixel_buffer,
    srgb_encode,
};

const MAGIC: u32 = 0x802A_5FD7;
const HEADER_LEN: usize = 712;
const ORDER: Order = Order::Big;

/// Log density code to display value for 10-bit data.
fn film_lut() -> [u8; 1024] {
    let lin = |v: f32| 10f32.powf((v - 685.0) * 0.002 / 0.6);
    let black = lin(95.0);
    let mut lut = [0u8; 1024];
    for (v, o) in lut.iter_mut().enumerate() {
        *o = srgb_encode((lin(v as f32) - black) / (1.0 - black));
    }
    lut
}

fn unpack_row(row: &[u8], bits: u8, packing: u8, out: &mut [u16]) {
    match (bits, packing) {
        (8, _) => {
            for (o, b) in out.iter_mut().zip(row) {
                *o = u16::from(*b);
            }
        }
        (16, _) => {
            for (i, o) in out.iter_mut().enumerate() {
                *o = ORDER.u16(row, i * 2).unwrap_or(0);
            }
        }
        (10, 5 | 6) => {
            let shifts: [u32; 3] = if packing == 5 {
                [22, 12, 2]
            } else {
                [20, 10, 0]
            };
            for (i, o) in out.iter_mut().enumerate() {
                let word = ORDER.u32(row, i / 3 * 4).unwrap_or(0);
                *o = ((word >> shifts[i % 3]) & 0x3FF) as u16;
            }
        }
        (10, 1..=4) => {
            let left = packing & 1 == 1;
            for (i, o) in out.iter_mut().enumerate() {
                let v = ORDER.u16(row, i * 2).unwrap_or(0);
                *o = if left { v >> 6 } else { v & 0x3FF };
            }
        }
        _ => {
            // Packed bit stream, most significant bit first.
            for (i, o) in out.iter_mut().enumerate() {
                let mut v = 0u16;
                for k in 0..10usize {
                    let bit = i * 10 + k;
                    let byte = row.get(bit / 8).copied().unwrap_or(0);
                    v = v << 1 | u16::from(byte >> (7 - bit % 8) & 1);
                }
                *o = v;
            }
        }
    }
}

fn line_bytes(samples: u64, bits: u8, packing: u8) -> Option<u64> {
    match (bits, packing) {
        (8, _) => Some(samples),
        (16, _) | (10, 1..=4) => samples.checked_mul(2),
        (10, 5 | 6) => samples.div_ceil(3).checked_mul(4),
        _ => Some(samples.checked_mul(u64::from(bits))?.div_ceil(8)),
    }
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if ORDER.u32(data, 0) != Some(MAGIC) {
        return Err(DecodeError::Unsupported);
    }
    if data.len() < HEADER_LEN {
        return Err(DecodeError::malformed("Cineon header truncated"));
    }
    let image_offset = ORDER.u32(data, 4).unwrap_or(0);
    let orientation = data[192];
    let channels = usize::from(data[193]);
    let bits = data[196 + 2];
    let width = ORDER.u32(data, 196 + 4).unwrap_or(0);
    let height = ORDER.u32(data, 196 + 8).unwrap_or(0);
    let interleave = data[680];
    let packing = data[681];
    let line_pad = u64::from(ORDER.u32(data, 684).unwrap_or(0));

    if !matches!(channels, 1 | 3) || interleave != 0 || !matches!(bits, 8 | 10 | 16) {
        return Err(DecodeError::Unsupported);
    }
    if (1..channels).any(|c| {
        let at = 196 + 28 * c;
        data[at + 2] != bits
            || ORDER.u32(data, at + 4) != Some(width)
            || ORDER.u32(data, at + 8) != Some(height)
    }) {
        return Err(DecodeError::Unsupported);
    }
    check_dimensions(width, height)?;
    check_alloc(u64::from(width) * u64::from(height) * channels as u64)?;

    let samples = u64::from(width) * channels as u64;
    let line = line_bytes(samples, bits, packing).ok_or(DecodeError::TooLarge)?;
    let stride = line.checked_add(line_pad).ok_or(DecodeError::TooLarge)?;
    let needed = stride
        .checked_mul(u64::from(height) - 1)
        .and_then(|v| v.checked_add(line))
        .and_then(|v| v.checked_add(u64::from(image_offset)))
        .ok_or(DecodeError::TooLarge)?;
    if needed > data.len() as u64 {
        return Err(DecodeError::malformed("Cineon image data truncated"));
    }
    let (offset, line, stride) = (image_offset as usize, line as usize, stride as usize);

    let lut = (bits == 10).then(film_lut);
    let max = (1u32 << bits) - 1;
    let to8 = |v: u16| match &lut {
        Some(l) => l[usize::from(v & 0x3FF)],
        None => ((u32::from(v) * 255 + max / 2) / max).min(255) as u8,
    };
    let (w, h) = (width as usize, height as usize);
    let mut out = pixel_buffer(width, height, channels as u64)?;
    let mut row_samples = vec![0u16; w * channels];
    for y in 0..h {
        let row = &data[offset + y * stride..offset + y * stride + line];
        unpack_row(row, bits, packing, &mut row_samples);
        let dst = &mut out[y * w * channels..(y + 1) * w * channels];
        for (d, s) in dst.iter_mut().zip(&row_samples) {
            *d = to8(*s);
        }
    }
    let mut img = interleaved_image(width, height, channels, out)?;
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

    fn header(w: u32, h: u32, channels: u8, bits: u8, packing: u8) -> Vec<u8> {
        let mut f = vec![0u8; 1024];
        f[..4].copy_from_slice(&MAGIC.to_be_bytes());
        f[4..8].copy_from_slice(&1024u32.to_be_bytes());
        f[193] = channels;
        for c in 0..usize::from(channels) {
            let at = 196 + 28 * c;
            f[at] = 0;
            f[at + 1] = c as u8 + 1;
            f[at + 2] = bits;
            f[at + 4..at + 8].copy_from_slice(&w.to_be_bytes());
            f[at + 8..at + 12].copy_from_slice(&h.to_be_bytes());
        }
        f[681] = packing;
        f
    }

    #[test]
    fn ten_bit_rgb_log_to_display() {
        let mut f = header(2, 1, 3, 10, 5);
        // Pixel 0 at reference white (685) on all channels, pixel 1 at reference black (95).
        let word = |v: u32| (v << 22) | (v << 12) | (v << 2);
        f.extend(word(685).to_be_bytes());
        f.extend(word(95).to_be_bytes());
        let img = decode(&f).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (2, 1));
        assert_eq!(img.get_pixel(0, 0).0, [255, 255, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 0, 0]);
    }

    #[test]
    fn eight_bit_gray_with_line_padding_and_flip() {
        let mut f = header(2, 2, 1, 8, 0);
        f[684..688].copy_from_slice(&2u32.to_be_bytes());
        f[192] = 2; // bottom to top
        f.extend([10, 20, 0, 0, 30, 40]);
        let img = decode(&f).unwrap().to_luma8();
        assert_eq!(img.as_raw(), &[30, 40, 10, 20]);
    }

    #[test]
    fn hostile_inputs_are_rejected() {
        assert_eq!(decode(b"junk"), Err(DecodeError::Unsupported));
        assert!(matches!(
            decode(&MAGIC.to_be_bytes()),
            Err(DecodeError::Malformed(_))
        ));
        assert_eq!(
            decode(&header(65535, 65535, 3, 10, 5)),
            Err(DecodeError::TooLarge)
        );
        assert_eq!(
            decode(&header(u32::MAX, u32::MAX, 3, 16, 0)),
            Err(DecodeError::TooLarge)
        );
        assert!(matches!(
            decode(&header(4000, 4000, 3, 10, 5)),
            Err(DecodeError::Malformed(_))
        ));
    }
}
