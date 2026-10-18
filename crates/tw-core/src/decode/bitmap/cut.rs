//! DR Halo CUT (`.cut`): little-endian `u16` width, height and a reserved word, then per row a
//! `u16` byte count and an RLE line (`0` ends the line, `0x80 | n` repeats the next byte
//! `n` times, otherwise `n` literal bytes follow). Pixels are 8-bit palette indices; the
//! palette lives in a separate `.pal` file that a thumbnail handler never sees, so indices
//! are shown as a grey ramp stretched over the highest index used.

use image::DynamicImage;

use super::{DecodeError, Result, gray_image, le16, pixel_bytes};

const HEADER_LEN: usize = 6;

/// Decodes one RLE line into `row`; the line must produce exactly `row.len()` pixels.
fn unpack_line(line: &[u8], row: &mut [u8]) -> Result<()> {
    let bad = || DecodeError::malformed("CUT line is corrupt");
    let mut s = 0usize;
    let mut d = 0usize;
    loop {
        let b = *line.get(s).ok_or_else(bad)?;
        s += 1;
        if b == 0 {
            break;
        }
        let n = usize::from(b & 0x7F);
        if b & 0x80 != 0 {
            let v = *line.get(s).ok_or_else(bad)?;
            s += 1;
            row.get_mut(d..d + n).ok_or_else(bad)?.fill(v);
        } else {
            let lit = line.get(s..s + n).ok_or_else(bad)?;
            row.get_mut(d..d + n).ok_or_else(bad)?.copy_from_slice(lit);
            s += n;
        }
        d += n;
    }
    if d == row.len() { Ok(()) } else { Err(bad()) }
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    let (Some(width), Some(height)) = (le16(data, 0), le16(data, 2)) else {
        return Err(DecodeError::Unsupported);
    };
    let (width, height) = (u32::from(width), u32::from(height));
    pixel_bytes(width, height, 1)?;
    let (w, h) = (width as usize, height as usize);
    // Every row costs at least its two-byte length word.
    if (data.len() - HEADER_LEN.min(data.len())) / 2 < h {
        return Err(DecodeError::malformed("CUT data truncated"));
    }

    let mut out = vec![0u8; w * h];
    let mut at = HEADER_LEN;
    for row in out.chunks_exact_mut(w) {
        let len =
            usize::from(le16(data, at).ok_or_else(|| DecodeError::malformed("CUT truncated"))?);
        let line = data
            .get(at + 2..at + 2 + len)
            .ok_or_else(|| DecodeError::malformed("CUT line truncated"))?;
        unpack_line(line, row)?;
        at += 2 + len;
    }

    let max = out.iter().copied().max().unwrap_or(0);
    if max > 0 && max < 255 {
        for v in &mut out {
            *v = (u32::from(*v) * 255 / u32::from(max)) as u8;
        }
    }
    gray_image(width, height, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(w: u16, h: u16, lines: &[&[u8]]) -> Vec<u8> {
        let mut f = Vec::new();
        for x in [w, h, 0] {
            f.extend_from_slice(&x.to_le_bytes());
        }
        for l in lines {
            f.extend_from_slice(&(l.len() as u16).to_le_bytes());
            f.extend_from_slice(l);
        }
        f
    }

    #[test]
    fn rle_and_literal_lines_stretch_to_full_range() {
        // Row 0: run of 3 x index 0 then literal [2]; row 1: literal [1, 1, 1, 1].
        let f = file(4, 2, &[&[0x83, 0, 1, 2, 0], &[4, 1, 1, 1, 1, 0]]);
        let img = decode(&f).unwrap().to_luma8();
        assert_eq!(img.dimensions(), (4, 2));
        assert_eq!(img.get_pixel(0, 0).0, [0]);
        assert_eq!(img.get_pixel(3, 0).0, [255]);
        assert_eq!(img.get_pixel(0, 1).0, [127]);
    }

    #[test]
    fn hostile_inputs() {
        assert_eq!(decode(&[1]).unwrap_err(), DecodeError::Unsupported);
        let f = file(4, 2, &[&[0x84, 1, 0], &[0x84, 1, 0]]);
        assert!(decode(&f).is_ok());
        assert!(matches!(
            decode(&f[..f.len() - 2]),
            Err(DecodeError::Malformed(_))
        ));
        // A run past the declared width.
        let f = file(2, 1, &[&[0x85, 1, 0]]);
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        // 65535 x 65535 exceeds the pixel budget; 60000 x 5000 is within it but the file
        // has no rows to back it.
        assert_eq!(
            decode(&file(65535, 65535, &[])).unwrap_err(),
            DecodeError::TooLarge
        );
        assert!(matches!(
            decode(&file(60000, 5000, &[&[0]])),
            Err(DecodeError::Malformed(_))
        ));
    }
}
