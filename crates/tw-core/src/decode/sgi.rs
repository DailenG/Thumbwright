//! Silicon Graphics image (`.sgi`, `.bw`): verbatim or RLE, 1-4 channels, 8 or 16 bits
//! per channel (16-bit samples are reduced to their high byte).

use image::{DynamicImage, GrayAlphaImage, GrayImage, RgbImage, RgbaImage};

use super::{DecodeError, Result, check_alloc, check_dimensions};

pub const EXTENSIONS: &[&str] = &["sgi", "bw"];

const MAGIC: u16 = 474;
const HEADER_LEN: usize = 512;

fn be16(data: &[u8], at: usize) -> Option<u16> {
    let b = data.get(at..at.checked_add(2)?)?;
    Some(u16::from_be_bytes([b[0], b[1]]))
}

fn be32(data: &[u8], at: usize) -> Option<u32> {
    let b = data.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if be16(data, 0) != Some(MAGIC) {
        return Err(DecodeError::Unsupported);
    }
    if data.len() < HEADER_LEN {
        return Err(DecodeError::malformed("SGI header truncated"));
    }
    let storage = data[2];
    let bpc = data[3];
    let dimension = be16(data, 4).unwrap_or(0);
    let xsize = be16(data, 6).unwrap_or(0);
    let mut ysize = be16(data, 8).unwrap_or(0);
    let mut zsize = be16(data, 10).unwrap_or(0);
    let colormap = be32(data, 104).unwrap_or(0);
    if storage > 1 || !matches!(bpc, 1 | 2) {
        return Err(DecodeError::malformed("unsupported SGI storage or depth"));
    }
    match dimension {
        1 => {
            ysize = 1;
            zsize = 1;
        }
        2 => zsize = 1,
        3 => {}
        _ => return Err(DecodeError::malformed("bad SGI dimension")),
    }
    if colormap != 0 {
        return Err(DecodeError::malformed(
            "SGI colour-mapped images are not supported",
        ));
    }
    if zsize == 0 {
        return Err(DecodeError::malformed("SGI image without channels"));
    }
    let (width, height) = (u32::from(xsize), u32::from(ysize));
    check_dimensions(width, height)?;

    // Channels beyond the fourth are auxiliary and ignored.
    let channels = usize::from(zsize.min(4));
    let (w, h, bpc) = (usize::from(xsize), usize::from(ysize), usize::from(bpc));
    // RLE rows may all share one short run, so the input size does not bound the output.
    check_alloc(u64::from(width) * u64::from(height) * channels as u64)?;
    if storage == 0 && data.len() < HEADER_LEN + w * h * bpc * channels {
        return Err(DecodeError::malformed("SGI data truncated"));
    }
    let mut out = vec![0u8; w * h * channels];
    let mut row = vec![0u8; w * bpc];

    if storage == 0 {
        let plane = w * h * bpc;
        for c in 0..channels {
            for y in 0..h {
                let at = HEADER_LEN + c * plane + y * w * bpc;
                row.copy_from_slice(&data[at..at + w * bpc]);
                put_row(&mut out, &row, w, h, channels, c, y, bpc);
            }
        }
    } else {
        let rows = h * usize::from(zsize);
        let tables = HEADER_LEN + rows * 8;
        if data.len() < tables {
            return Err(DecodeError::malformed("SGI RLE tables truncated"));
        }
        for c in 0..channels {
            for y in 0..h {
                let i = c * h + y;
                let start = be32(data, HEADER_LEN + i * 4).unwrap_or(0) as usize;
                let len = be32(data, HEADER_LEN + rows * 4 + i * 4).unwrap_or(0) as usize;
                let src = start
                    .checked_add(len)
                    .and_then(|end| data.get(start..end))
                    .ok_or_else(|| DecodeError::malformed("SGI RLE row out of range"))?;
                unpack_rle(src, &mut row, bpc)?;
                put_row(&mut out, &row, w, h, channels, c, y, bpc);
            }
        }
    }

    let buf_err = || DecodeError::malformed("SGI buffer size mismatch");
    Ok(match channels {
        1 => DynamicImage::ImageLuma8(GrayImage::from_raw(width, height, out).ok_or_else(buf_err)?),
        2 => DynamicImage::ImageLumaA8(
            GrayAlphaImage::from_raw(width, height, out).ok_or_else(buf_err)?,
        ),
        3 => DynamicImage::ImageRgb8(RgbImage::from_raw(width, height, out).ok_or_else(buf_err)?),
        _ => DynamicImage::ImageRgba8(RgbaImage::from_raw(width, height, out).ok_or_else(buf_err)?),
    })
}

/// Stores one decoded scan line (bottom-up in the file) into the top-down interleaved
/// output, keeping the high byte of 16-bit samples.
#[allow(clippy::too_many_arguments)]
fn put_row(
    out: &mut [u8],
    row: &[u8],
    w: usize,
    h: usize,
    channels: usize,
    c: usize,
    y: usize,
    bpc: usize,
) {
    let dst_y = h - 1 - y;
    let line = &mut out[dst_y * w * channels..(dst_y + 1) * w * channels];
    for (px, sample) in line.chunks_exact_mut(channels).zip(row.chunks_exact(bpc)) {
        px[c] = sample[0];
    }
}

/// SGI RLE: a count byte/word whose high bit selects a literal copy of `count` samples,
/// otherwise the next sample repeated `count` times; a zero count ends the row.
fn unpack_rle(src: &[u8], row: &mut [u8], bpc: usize) -> Result<()> {
    let truncated = || DecodeError::malformed("SGI RLE row truncated");
    let mut s = 0usize;
    let mut d = 0usize;
    let read = |s: usize| -> Option<u16> {
        if bpc == 1 {
            src.get(s).map(|&b| u16::from(b))
        } else {
            be16(src, s)
        }
    };
    loop {
        let code = read(s).ok_or_else(truncated)?;
        s += bpc;
        let count = usize::from(code & 0x7F);
        if count == 0 {
            break;
        }
        let bytes = count * bpc;
        let dst = row
            .get_mut(d..d + bytes)
            .ok_or_else(|| DecodeError::malformed("SGI RLE row overflows the width"))?;
        if code & 0x80 != 0 {
            dst.copy_from_slice(src.get(s..s + bytes).ok_or_else(truncated)?);
            s += bytes;
        } else {
            let value = src.get(s..s + bpc).ok_or_else(truncated)?;
            for chunk in dst.chunks_exact_mut(bpc) {
                chunk.copy_from_slice(value);
            }
            s += bpc;
        }
        d += bytes;
    }
    if d != row.len() {
        return Err(DecodeError::malformed("SGI RLE row shorter than the width"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(storage: u8, bpc: u8, dim: u16, x: u16, y: u16, z: u16) -> Vec<u8> {
        let mut h = vec![0u8; HEADER_LEN];
        h[0..2].copy_from_slice(&MAGIC.to_be_bytes());
        h[2] = storage;
        h[3] = bpc;
        h[4..6].copy_from_slice(&dim.to_be_bytes());
        h[6..8].copy_from_slice(&x.to_be_bytes());
        h[8..10].copy_from_slice(&y.to_be_bytes());
        h[10..12].copy_from_slice(&z.to_be_bytes());
        h
    }

    #[test]
    fn verbatim_rgb_is_flipped_to_top_down() {
        // 2x2 RGB; file rows are bottom-up: bottom row red|green, top row blue|white.
        let mut f = header(0, 1, 3, 2, 2, 3);
        f.extend_from_slice(&[255, 0, 0, 255]); // R: bottom, top
        f.extend_from_slice(&[0, 255, 0, 255]); // G
        f.extend_from_slice(&[0, 0, 255, 255]); // B
        let img = decode(&f).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (2, 2));
        assert_eq!(img.get_pixel(0, 1).0, [255, 0, 0]);
        assert_eq!(img.get_pixel(1, 1).0, [0, 255, 0]);
        assert_eq!(img.get_pixel(0, 0).0, [0, 0, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [255, 255, 255]);
    }

    #[test]
    fn rle_16bit_grayscale() {
        // 3x1 grey, 16-bit: run of two 0x8000 then literal 0xFF00.
        let mut f = header(1, 2, 2, 3, 1, 1);
        let data_at = (HEADER_LEN + 8) as u32;
        let row: Vec<u8> = [0x0002u16, 0x8000, 0x0081, 0xFF00, 0x0000]
            .iter()
            .flat_map(|v| v.to_be_bytes())
            .collect();
        f.extend_from_slice(&data_at.to_be_bytes());
        f.extend_from_slice(&(row.len() as u32).to_be_bytes());
        f.extend_from_slice(&row);
        let img = decode(&f).unwrap();
        let g = img.to_luma8();
        assert_eq!(g.dimensions(), (3, 1));
        assert_eq!(g.as_raw(), &vec![0x80, 0x80, 0xFF]);
    }

    #[test]
    fn hostile_inputs_are_rejected() {
        // Verbatim data shorter than declared.
        let mut f = header(0, 1, 3, 100, 100, 3);
        f.extend_from_slice(&[0; 10]);
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        // RLE row pointing past the end of the file.
        let mut f = header(1, 1, 2, 4, 1, 1);
        f.extend_from_slice(&0xFFFF_FFF0u32.to_be_bytes());
        f.extend_from_slice(&0x20u32.to_be_bytes());
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        // RLE run overflowing the row.
        let mut f = header(1, 1, 2, 2, 1, 1);
        f.extend_from_slice(&((HEADER_LEN + 8) as u32).to_be_bytes());
        f.extend_from_slice(&3u32.to_be_bytes());
        f.extend_from_slice(&[0x7F, 0x01, 0x00]);
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        // Over budget dimensions are refused before allocation.
        let f = header(0, 1, 3, 65535, 65535, 4);
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
        // Truncated header and foreign data.
        assert!(matches!(
            decode(&MAGIC.to_be_bytes()),
            Err(DecodeError::Malformed(_))
        ));
        assert_eq!(decode(b"not sgi"), Err(DecodeError::Unsupported));
    }

    #[test]
    fn verbatim_header_bomb_is_too_large() {
        // 20000 x 20000 x 4 is under the pixel-area budget but needs 1.6 GB.
        let f = header(0, 1, 3, 20000, 20000, 4);
        assert_eq!(f.len(), 512);
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
    }

    #[test]
    fn rle_rows_sharing_one_offset_are_too_large() {
        // Every row points at the same 2-byte run: a ~640 KB file declaring 1.6 GB.
        let (h, z) = (20000usize, 4usize);
        let mut f = header(1, 1, 3, 20000, h as u16, z as u16);
        let run_at = (HEADER_LEN + h * z * 8) as u32;
        for _ in 0..h * z {
            f.extend_from_slice(&run_at.to_be_bytes());
        }
        for _ in 0..h * z {
            f.extend_from_slice(&2u32.to_be_bytes());
        }
        f.extend_from_slice(&[0x00, 0x00]);
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
    }
}
