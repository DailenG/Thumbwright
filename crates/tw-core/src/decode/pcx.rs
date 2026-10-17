//! ZSoft PCX (1/2/4/8 bits per plane, 1-4 planes, RLE or raw) and the multi-page DCX
//! wrapper (first page only).

use image::{DynamicImage, RgbImage, RgbaImage};

use super::{DecodeError, Result, check_alloc, check_dimensions};

pub const EXTENSIONS: &[&str] = &["pcx", "dcx"];

const HEADER_LEN: usize = 128;
const DCX_MAGIC: u32 = 0x3ADE_68B1;
/// DCX page table: up to 1023 offsets followed by a zero terminator.
const DCX_MAX_PAGES: usize = 1023;
/// Longest RLE run (count byte `0xC0 | 63`) emitted for two input bytes.
const RLE_MAX_RUN: u64 = 63;

/// Standard 16-colour EGA palette, used when a planar header carries an all-zero palette.
const EGA_PALETTE: [[u8; 3]; 16] = [
    [0x00, 0x00, 0x00],
    [0x00, 0x00, 0xAA],
    [0x00, 0xAA, 0x00],
    [0x00, 0xAA, 0xAA],
    [0xAA, 0x00, 0x00],
    [0xAA, 0x00, 0xAA],
    [0xAA, 0x55, 0x00],
    [0xAA, 0xAA, 0xAA],
    [0x55, 0x55, 0x55],
    [0x55, 0x55, 0xFF],
    [0x55, 0xFF, 0x55],
    [0x55, 0xFF, 0xFF],
    [0xFF, 0x55, 0x55],
    [0xFF, 0x55, 0xFF],
    [0xFF, 0xFF, 0x55],
    [0xFF, 0xFF, 0xFF],
];

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    match read_u32_le(data, 0) {
        Some(DCX_MAGIC) => decode_dcx(data),
        _ => decode_pcx(data),
    }
}

fn read_u16_le(data: &[u8], at: usize) -> Option<u16> {
    let b = data.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

fn read_u32_le(data: &[u8], at: usize) -> Option<u32> {
    let b = data.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn decode_dcx(data: &[u8]) -> Result<DynamicImage> {
    let mut table = (0..DCX_MAX_PAGES).map_while(|i| read_u32_le(data, 4 + i * 4));
    let first = table
        .next()
        .ok_or_else(|| DecodeError::malformed("DCX page table truncated"))?;
    if first == 0 {
        return Err(DecodeError::malformed("DCX has no pages"));
    }
    let start = usize::try_from(first).unwrap_or(usize::MAX);
    // Page one ends where page two starts (its VGA palette sits at the page end).
    let end = table
        .next()
        .and_then(|next| usize::try_from(next).ok())
        .filter(|&next| next > start && next <= data.len())
        .unwrap_or(data.len());
    let page = data
        .get(start..end)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| DecodeError::malformed("DCX page offset out of range"))?;
    decode_pcx(page)
}

/// Streaming PCX run-length decoder; runs may cross scan-line boundaries.
struct Rle<'a> {
    src: &'a [u8],
    pos: usize,
    run_value: u8,
    run_left: u8,
    compressed: bool,
}

impl Rle<'_> {
    /// Fills `out` completely; returns `false` when the input ran out first.
    fn fill(&mut self, out: &mut [u8]) -> bool {
        for slot in out.iter_mut() {
            if !self.compressed {
                match self.src.get(self.pos) {
                    Some(&b) => {
                        *slot = b;
                        self.pos += 1;
                        continue;
                    }
                    None => return false,
                }
            }
            // Zero-length runs (`0xC0 v`) produce nothing; keep reading codes.
            while self.run_left == 0 {
                let Some(&b) = self.src.get(self.pos) else {
                    return false;
                };
                self.pos += 1;
                if b >= 0xC0 {
                    let Some(&v) = self.src.get(self.pos) else {
                        return false;
                    };
                    self.pos += 1;
                    self.run_value = v;
                    self.run_left = b & 0x3F;
                } else {
                    self.run_value = b;
                    self.run_left = 1;
                }
            }
            *slot = self.run_value;
            self.run_left -= 1;
        }
        true
    }
}

fn decode_pcx(data: &[u8]) -> Result<DynamicImage> {
    let header = data.get(..HEADER_LEN).ok_or(DecodeError::Unsupported)?;
    if header[0] != 0x0A || !matches!(header[1], 0 | 2 | 3 | 4 | 5) || header[2] > 1 {
        return Err(DecodeError::Unsupported);
    }
    let compressed = header[2] == 1;
    let bpp = header[3];
    let planes = header[65];
    let field = |at| read_u16_le(header, at).unwrap_or(0);
    let (xmin, ymin, xmax, ymax) = (field(4), field(6), field(8), field(10));
    let bytes_per_line = usize::from(field(66));
    if xmax < xmin || ymax < ymin {
        return Err(DecodeError::malformed("PCX window is inverted"));
    }
    let width = u32::from(xmax - xmin) + 1;
    let height = u32::from(ymax - ymin) + 1;

    let direct = bpp == 8 && matches!(planes, 3 | 4);
    let indexed = matches!((bpp, planes), (1, 1..=4) | (2, 1) | (4, 1) | (8, 1));
    if !direct && !indexed {
        return Err(DecodeError::malformed(format!(
            "unsupported PCX layout: {bpp} bits x {planes} planes"
        )));
    }
    check_dimensions(width, height)?;
    let min_line = (u64::from(width) * u64::from(bpp)).div_ceil(8);
    if (bytes_per_line as u64) < min_line {
        return Err(DecodeError::malformed(
            "PCX bytes-per-line shorter than the width",
        ));
    }

    let line_len = bytes_per_line * usize::from(planes);
    let body = &data[HEADER_LEN..];
    // Reject inputs that cannot possibly hold the declared raster before allocating it.
    let total = line_len as u64 * u64::from(height);
    let max_out = if compressed {
        (body.len() as u64 / 2).saturating_mul(RLE_MAX_RUN)
    } else {
        body.len() as u64
    };
    if total > max_out {
        return Err(DecodeError::malformed("PCX data truncated"));
    }

    let mut rle = Rle {
        src: body,
        pos: 0,
        run_value: 0,
        run_left: 0,
        compressed,
    };
    let mut line = vec![0u8; line_len];
    let (w, h) = (width as usize, height as usize);

    if direct {
        let channels = usize::from(planes);
        check_alloc(u64::from(width) * u64::from(height) * u64::from(planes))?;
        let mut out = vec![0u8; w * h * channels];
        for row in out.chunks_exact_mut(w * channels) {
            if !rle.fill(&mut line) {
                return Err(DecodeError::malformed("PCX data truncated"));
            }
            for (x, px) in row.chunks_exact_mut(channels).enumerate() {
                for (c, v) in px.iter_mut().enumerate() {
                    *v = line[c * bytes_per_line + x];
                }
            }
        }
        return Ok(if channels == 4 {
            DynamicImage::ImageRgba8(
                RgbaImage::from_raw(width, height, out).ok_or_else(buffer_error)?,
            )
        } else {
            DynamicImage::ImageRgb8(
                RgbImage::from_raw(width, height, out).ok_or_else(buffer_error)?,
            )
        });
    }

    let palette = palette_for(data, header, bpp, planes);
    check_alloc(u64::from(width) * u64::from(height) * 3)?;
    let mut out = vec![0u8; w * h * 3];
    let bpp = usize::from(bpp);
    let per_byte = 8 / bpp;
    let mask: u8 = if bpp == 8 { 0xFF } else { (1u8 << bpp) - 1 };
    for row in out.chunks_exact_mut(w * 3) {
        if !rle.fill(&mut line) {
            return Err(DecodeError::malformed("PCX data truncated"));
        }
        for (x, px) in row.as_chunks_mut::<3>().0.iter_mut().enumerate() {
            let byte = x / per_byte;
            let shift = 8 - bpp - (x % per_byte) * bpp;
            let mut index = 0usize;
            for p in 0..usize::from(planes) {
                let bits = (line[p * bytes_per_line + byte] >> shift) & mask;
                index |= usize::from(bits) << (p * bpp);
            }
            *px = palette[index.min(255)];
        }
    }
    Ok(DynamicImage::ImageRgb8(
        RgbImage::from_raw(width, height, out).ok_or_else(buffer_error)?,
    ))
}

fn buffer_error() -> DecodeError {
    DecodeError::malformed("PCX buffer size mismatch")
}

fn palette_for(data: &[u8], header: &[u8], bpp: u8, planes: u8) -> Vec<[u8; 3]> {
    let mut pal = vec![[0u8; 3]; 256];
    if bpp == 8 {
        // VGA palette: 0x0C marker followed by 768 bytes at the very end of the file.
        let tail = data
            .len()
            .checked_sub(769)
            .filter(|&at| at >= HEADER_LEN && data[at] == 0x0C)
            .map(|at| &data[at + 1..]);
        match tail {
            Some(rgb) => {
                for (slot, c) in pal.iter_mut().zip(rgb.as_chunks::<3>().0) {
                    *slot = *c;
                }
            }
            None => {
                for (i, slot) in pal.iter_mut().enumerate() {
                    *slot = [i as u8; 3];
                }
            }
        }
        return pal;
    }
    if bpp == 1 && planes == 1 {
        pal[0] = [0, 0, 0];
        pal[1] = [255, 255, 255];
        return pal;
    }
    let ega = &header[16..64];
    if ega.iter().all(|&b| b == 0) {
        pal[..16].copy_from_slice(&EGA_PALETTE);
    } else {
        for (slot, c) in pal.iter_mut().zip(ega.as_chunks::<3>().0) {
            *slot = *c;
        }
    }
    pal
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(bpp: u8, planes: u8, w: u16, h: u16, bpl: u16) -> Vec<u8> {
        let mut hd = vec![0u8; HEADER_LEN];
        hd[0] = 0x0A;
        hd[1] = 5;
        hd[2] = 1;
        hd[3] = bpp;
        hd[8..10].copy_from_slice(&(w - 1).to_le_bytes());
        hd[10..12].copy_from_slice(&(h - 1).to_le_bytes());
        hd[65] = planes;
        hd[66..68].copy_from_slice(&bpl.to_le_bytes());
        hd
    }

    #[test]
    fn rgb_three_plane_rle() {
        // 2x2, 3 planes; row 0 red|green, row 1 blue|white.
        let mut f = header(8, 3, 2, 2, 2);
        // Row 0: R plane = [255, 0], G = [0, 255], B = [0, 0] (run of two zeros).
        f.extend_from_slice(&[0xC1, 0xFF, 0x00, 0x00, 0xC1, 0xFF, 0xC2, 0x00]);
        // Row 1: R = [0, 255], G = [0, 255], B = [255, 255] (RLE run crossing planes).
        f.extend_from_slice(&[0x00, 0xC1, 0xFF, 0x00, 0xC3, 0xFF]);
        let img = decode(&f).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (2, 2));
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 255, 0]);
        assert_eq!(img.get_pixel(0, 1).0, [0, 0, 255]);
        assert_eq!(img.get_pixel(1, 1).0, [255, 255, 255]);
    }

    #[test]
    fn palette_8bit_with_vga_tail_inside_dcx() {
        let mut page = header(8, 1, 3, 1, 4);
        page.extend_from_slice(&[0x01, 0x02, 0xC2, 0x01]);
        page.push(0x0C);
        let mut pal = vec![0u8; 768];
        pal[3..6].copy_from_slice(&[10, 20, 30]);
        pal[6..9].copy_from_slice(&[40, 50, 60]);
        page.extend_from_slice(&pal);

        let mut dcx = DCX_MAGIC.to_le_bytes().to_vec();
        dcx.extend_from_slice(&12u32.to_le_bytes());
        dcx.extend_from_slice(&0u32.to_le_bytes());
        dcx.extend_from_slice(&page);
        let img = decode(&dcx).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (3, 1));
        assert_eq!(img.get_pixel(0, 0).0, [10, 20, 30]);
        assert_eq!(img.get_pixel(1, 0).0, [40, 50, 60]);
        assert_eq!(img.get_pixel(2, 0).0, [10, 20, 30]);
    }

    #[test]
    fn monochrome_bits() {
        let mut f = header(1, 1, 8, 1, 2);
        f.extend_from_slice(&[0b1010_0000, 0x00]);
        let img = decode(&f).unwrap().to_rgb8();
        assert_eq!(img.get_pixel(0, 0).0, [255, 255, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 0, 0]);
        assert_eq!(img.get_pixel(2, 0).0, [255, 255, 255]);
    }

    #[test]
    fn hostile_inputs_are_rejected() {
        // Huge declared raster backed by a few bytes: rejected before allocation.
        let mut f = header(8, 3, 20000, 20000, 20000);
        f.extend_from_slice(&[0xC5, 0x00]);
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        // Over the pixel budget.
        let f = header(8, 1, 65535, 65535, 65535);
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
        // Truncated scan lines.
        let mut f = header(8, 3, 2, 2, 2);
        f.extend_from_slice(&[0xC6, 0x01, 0x05]);
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        // Bytes-per-line too short for the width.
        let mut f = header(8, 1, 10, 1, 2);
        f.extend_from_slice(&[0; 32]);
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        // DCX pointing past the end.
        let mut dcx = DCX_MAGIC.to_le_bytes().to_vec();
        dcx.extend_from_slice(&9999u32.to_le_bytes());
        assert!(matches!(decode(&dcx), Err(DecodeError::Malformed(_))));
        // Not a PCX at all.
        assert_eq!(decode(b"hello"), Err(DecodeError::Unsupported));
    }

    #[test]
    fn output_over_byte_budget_is_too_large() {
        // 16384 x 16384 x 4 is under the pixel budget, and a 34 MB body can plausibly
        // RLE-expand to it, but the 1 GiB output exceeds the decode byte budget.
        let mut f = header(8, 4, 16384, 16384, 16384);
        let total = 16384u64 * 4 * 16384;
        f.resize(HEADER_LEN + (total.div_ceil(RLE_MAX_RUN) * 2) as usize, 0);
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
    }
}
