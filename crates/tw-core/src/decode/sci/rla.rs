//! Wavefront RLA (`.rla`): 740-byte big-endian header, a table of scanline offsets, and per
//! scanline one RLE segment per channel. 8-bit integer colour channels (gray or RGB) with
//! an optional matte; auxiliary channels are ignored. Scanlines are stored bottom first.

use image::DynamicImage;

use super::{
    DecodeError, Result, be16, be32, check_alloc, check_dimensions, gray_image, pixel_buffer,
    rgb_image, rgba_image,
};

const HEADER_LEN: usize = 740;

/// RLA run-length decoding: a non-negative count `n` repeats the next byte `n + 1` times, a
/// negative count copies `-n` literal bytes. Fills `out` exactly.
fn unpack(src: &[u8], out: &mut [u8]) -> Result<()> {
    let (mut i, mut o) = (0usize, 0usize);
    while o < out.len() {
        let n = *src
            .get(i)
            .ok_or_else(|| DecodeError::malformed("RLA scanline truncated"))? as i8;
        i += 1;
        let (count, literal) = if n >= 0 {
            (usize::from(n.unsigned_abs()) + 1, false)
        } else {
            (usize::from(n.unsigned_abs()), true)
        };
        let dst = out
            .get_mut(o..o + count)
            .ok_or_else(|| DecodeError::malformed("RLA run overruns the scanline"))?;
        if literal {
            let lit = src
                .get(i..i + count)
                .ok_or_else(|| DecodeError::malformed("RLA literal truncated"))?;
            dst.copy_from_slice(lit);
            i += count;
        } else {
            let b = *src
                .get(i)
                .ok_or_else(|| DecodeError::malformed("RLA run truncated"))?;
            dst.fill(b);
            i += 1;
        }
        o += count;
    }
    Ok(())
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if data.len() < HEADER_LEN {
        return Err(DecodeError::malformed("RLA header truncated"));
    }
    let coord = |at| i32::from(be16(data, at).unwrap_or(0) as i16);
    let (left, right, bottom, top) = (coord(8), coord(10), coord(12), coord(14));
    let width = i64::from(right) - i64::from(left) + 1;
    let height = i64::from(top) - i64::from(bottom) + 1;
    let (Ok(width), Ok(height)) = (u32::try_from(width), u32::try_from(height)) else {
        return Err(DecodeError::malformed("RLA window is empty"));
    };
    let storage_type = be16(data, 18).unwrap_or(0);
    let colors = usize::from(be16(data, 20).unwrap_or(0));
    let matte = usize::from(be16(data, 22).unwrap_or(0));
    if storage_type != 0 || !matches!(colors, 1 | 3) || matte > 1 {
        return Err(DecodeError::Unsupported);
    }
    check_dimensions(width, height)?;
    let channels = colors + matte;
    check_alloc(u64::from(width) * u64::from(height) * (channels as u64))?;
    let (w, h) = (width as usize, height as usize);
    let table_end = HEADER_LEN
        .checked_add(h.checked_mul(4).ok_or(DecodeError::TooLarge)?)
        .ok_or(DecodeError::TooLarge)?;
    if table_end > data.len() {
        return Err(DecodeError::malformed("RLA scanline table truncated"));
    }

    let mut out = pixel_buffer(width, height, channels as u64)?;
    let mut line = vec![0u8; w];
    for file_row in 0..h {
        let offset = be32(data, HEADER_LEN + file_row * 4).unwrap_or(0) as usize;
        let mut at = offset;
        // The first scanline in the file is the bottom one.
        let y = h - 1 - file_row;
        for c in 0..channels {
            let len = usize::from(
                be16(data, at)
                    .ok_or_else(|| DecodeError::malformed("RLA scanline offset out of range"))?,
            );
            let seg = data
                .get(at + 2..at + 2 + len)
                .ok_or_else(|| DecodeError::malformed("RLA segment out of range"))?;
            unpack(seg, &mut line)?;
            at += 2 + len;
            for (x, v) in line.iter().enumerate() {
                out[(y * w + x) * channels + c] = *v;
            }
        }
    }
    match (colors, matte) {
        (1, 0) => gray_image(width, height, out),
        (3, 0) => rgb_image(width, height, out),
        (3, 1) => rgba_image(width, height, out),
        _ => {
            // Gray with matte: replicate the gray channel.
            let rgba = out
                .as_chunks::<2>()
                .0
                .iter()
                .flat_map(|p| [p[0], p[0], p[0], p[1]])
                .collect();
            rgba_image(width, height, rgba)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(w: i16, h: i16, colors: u16, matte: u16) -> Vec<u8> {
        let mut f = vec![0u8; HEADER_LEN];
        let put =
            |f: &mut Vec<u8>, at: usize, v: i16| f[at..at + 2].copy_from_slice(&v.to_be_bytes());
        put(&mut f, 8, 0);
        put(&mut f, 10, w - 1);
        put(&mut f, 12, 0);
        put(&mut f, 14, h - 1);
        put(&mut f, 20, colors as i16);
        put(&mut f, 22, matte as i16);
        f
    }

    fn literal(bytes: &[u8]) -> Vec<u8> {
        let mut s = vec![(-(bytes.len() as i16)) as u8];
        s.extend(bytes);
        s
    }

    fn run(count: u8, byte: u8) -> Vec<u8> {
        vec![count - 1, byte]
    }

    fn scanline(channels: &[Vec<u8>]) -> Vec<u8> {
        let mut s = Vec::new();
        for c in channels {
            s.extend((c.len() as u16).to_be_bytes());
            s.extend(c);
        }
        s
    }

    #[test]
    fn rgba_with_both_run_kinds_bottom_first() {
        let mut f = header(2, 2, 3, 1);
        let bottom = scanline(&[run(2, 255), run(2, 0), run(2, 0), run(2, 255)]); // red
        let top = scanline(&[
            literal(&[0, 255]),
            literal(&[255, 0]),
            run(2, 0),
            literal(&[255, 128]),
        ]);
        let base = HEADER_LEN + 8;
        f.extend((base as u32).to_be_bytes());
        f.extend(((base + bottom.len()) as u32).to_be_bytes());
        f.extend(&bottom);
        f.extend(&top);
        let img = decode(&f).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (2, 2));
        assert_eq!(img.get_pixel(0, 1).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(1, 1).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(0, 0).0, [0, 255, 0, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [255, 0, 0, 128]);
    }

    #[test]
    fn hostile_inputs_are_rejected() {
        assert!(matches!(
            decode(&[0u8; 100]),
            Err(DecodeError::Malformed(_))
        ));
        // 32767-wide windows with too few pixels in the file.
        assert_eq!(
            decode(&header(32767, 32767, 3, 1)),
            Err(DecodeError::TooLarge)
        );
        // Table promises rows the file does not hold.
        assert!(matches!(
            decode(&header(10, 10, 3, 0)),
            Err(DecodeError::Malformed(_))
        ));
        // Offset pointing outside the file.
        let mut f = header(1, 1, 1, 0);
        f.extend(0xFFFF_FFF0u32.to_be_bytes());
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        // Run longer than the scanline.
        let mut f = header(2, 1, 1, 0);
        f.extend(((HEADER_LEN + 4) as u32).to_be_bytes());
        f.extend(scanline(&[run(9, 1)]));
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
    }
}
