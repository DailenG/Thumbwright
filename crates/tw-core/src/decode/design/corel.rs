//! CorelDRAW (`.cdr`, `.cdt`): classic files are RIFF containers (`RIFF....CDR?`) with a
//! `DISP` chunk holding a header-less DIB preview; X4 and later are ZIP packages with a
//! thumbnail image entry.

use image::DynamicImage;

use crate::decode::{DecodeError, Result, decode_bytes, zip_preview};

/// RIFF chunks visited while looking for `DISP`.
const MAX_CHUNKS: usize = 4096;
const MAX_DEPTH: usize = 4;

pub fn extract(data: &[u8]) -> Result<DynamicImage> {
    if data.starts_with(b"PK") {
        return zip_preview::extract_thumbnail(
            data,
            &["thumbnail.bmp", "thumbnail.png", "thumbnail.jpg"],
        );
    }
    if data.len() < 12 || data.get(..4) != Some(b"RIFF") || data.get(8..11) != Some(b"CDR") {
        return Err(DecodeError::Unsupported);
    }
    let disp = find_disp(data)?;
    decode_dib(disp)
}

fn le32(d: &[u8], i: usize) -> Option<usize> {
    Some(u32::from_le_bytes(d.get(i..i.checked_add(4)?)?.try_into().ok()?) as usize)
}

/// The first `DISP` chunk, at the top level or inside nested `LIST`s.
fn find_disp(data: &[u8]) -> Result<&[u8]> {
    let riff_len = le32(data, 4).unwrap_or(0);
    let end = riff_len.saturating_add(8).clamp(12, data.len());
    let mut budget = MAX_CHUNKS;
    search(&data[12..end], 0, &mut budget).ok_or(DecodeError::Unsupported)
}

fn search<'a>(mut body: &'a [u8], depth: usize, budget: &mut usize) -> Option<&'a [u8]> {
    while body.len() >= 8 && *budget > 0 {
        *budget -= 1;
        let id = &body[..4];
        let len = le32(body, 4)?;
        let payload = body
            .get(8..8usize.saturating_add(len))
            .unwrap_or(&body[8..]);
        if id == b"DISP" {
            return Some(payload);
        }
        if id == b"LIST"
            && depth < MAX_DEPTH
            && payload.len() >= 4
            && let Some(found) = search(&payload[4..], depth + 1, budget)
        {
            return Some(found);
        }
        // Chunks are padded to an even length.
        let next = 8usize.checked_add(len)?.checked_add(len & 1)?;
        body = body.get(next..)?;
    }
    None
}

/// A DIB (`BITMAPINFOHEADER` + palette + pixels) located after an optional small prefix,
/// wrapped in a BMP file header and decoded by the BMP codec.
fn decode_dib(disp: &[u8]) -> Result<DynamicImage> {
    for offset in (0..=32).step_by(4) {
        let Some(dib) = disp.get(offset..) else { break };
        let (Some(size), Some(bpp), Some(compression), Some(clr_used)) = (
            le32(dib, 0),
            dib.get(14..16).map(|b| u16::from_le_bytes([b[0], b[1]])),
            le32(dib, 16),
            le32(dib, 32),
        ) else {
            continue;
        };
        let plausible = matches!(size, 40 | 108 | 124)
            && dib.get(12..14) == Some(&[1, 0])
            && matches!(bpp, 1 | 4 | 8 | 24 | 32)
            && compression == 0;
        if !plausible {
            continue;
        }
        let palette = match bpp {
            1 | 4 | 8 => {
                let max = 1usize << bpp;
                if clr_used == 0 {
                    max
                } else {
                    clr_used.min(max)
                }
            }
            _ => 0,
        };
        let pixels_at = size + palette * 4;
        let mut bmp = Vec::with_capacity(14 + dib.len());
        bmp.extend(b"BM");
        bmp.extend(((14 + dib.len()) as u32).to_le_bytes());
        bmp.extend([0u8; 4]);
        bmp.extend(((14 + pixels_at) as u32).to_le_bytes());
        bmp.extend(dib);
        return decode_bytes(&bmp);
    }
    Err(DecodeError::malformed(
        "cdr: DISP chunk holds no recognisable DIB",
    ))
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use super::*;
    use crate::decode::design::testutil;

    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut c = id.to_vec();
        c.extend((body.len() as u32).to_le_bytes());
        c.extend(body);
        if body.len() % 2 == 1 {
            c.push(0);
        }
        c
    }

    fn riff(version: u8, children: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = children.concat();
        let mut f = b"RIFF".to_vec();
        f.extend((body.len() as u32 + 4).to_le_bytes());
        f.extend([b'C', b'D', b'R', version]);
        f.extend(body);
        f
    }

    /// 24-bit DIB (no file header) of a flat colour, taken from the BMP encoder.
    fn dib(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        testutil::bmp24(w, h, rgb)[14..].to_vec()
    }

    #[test]
    fn disp_chunk_inside_a_list() {
        let f = riff(
            b'9',
            &[
                chunk(b"vrsn", &[9, 0]),
                chunk(
                    b"LIST",
                    &[b"page".as_slice(), &chunk(b"junk", &[1, 2, 3])].concat(),
                ),
                chunk(b"DISP", &dib(5, 3, [10, 20, 250])),
            ],
        );
        let img = extract(&f).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (5, 3));
        assert_eq!(img.get_pixel(2, 1).0, [10, 20, 250]);
    }

    #[test]
    fn disp_with_prefix_and_palette() {
        // 4 bytes of prefix, then an 8-bit paletted DIB 2x1: indices 1, 0.
        let mut d = vec![0xAA; 4];
        d.extend(40u32.to_le_bytes());
        d.extend(2i32.to_le_bytes());
        d.extend(1i32.to_le_bytes());
        d.extend(1u16.to_le_bytes());
        d.extend(8u16.to_le_bytes());
        d.extend([0u8; 16]);
        d.extend(2u32.to_le_bytes()); // colours used
        d.extend(0u32.to_le_bytes());
        d.extend([0, 0, 0, 0, 30, 60, 90, 0]); // palette (BGRA): black, (90,60,30)
        d.extend([1, 0, 0, 0]); // one 4-byte row
        let f = riff(b'7', &[chunk(b"DISP", &d)]);
        let img = extract(&f).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (2, 1));
        assert_eq!(img.get_pixel(0, 0).0, [90, 60, 30]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 0, 0]);
    }

    #[test]
    fn x4_zip_package() {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("content/riffData.cdr", opts).unwrap();
        zip.write_all(b"RIFFxxxx").unwrap();
        zip.start_file("metadata/thumbnails/thumbnail.bmp", opts)
            .unwrap();
        zip.write_all(&testutil::bmp24(6, 2, [250, 10, 10]))
            .unwrap();
        let bytes = zip.finish().unwrap().into_inner();
        let img = extract(&bytes).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (6, 2));
        assert_eq!(img.get_pixel(0, 0).0, [250, 10, 10]);
    }

    #[test]
    fn hostile_inputs() {
        assert_eq!(
            extract(b"RIFF\0\0\0\0WAVEfmt "),
            Err(DecodeError::Unsupported)
        );
        assert_eq!(extract(b""), Err(DecodeError::Unsupported));
        // No DISP chunk.
        assert_eq!(
            extract(&riff(b'9', &[chunk(b"vrsn", &[9, 0])])),
            Err(DecodeError::Unsupported)
        );
        // Chunk length lying past the end; truncated mid-chunk.
        // A chunk length past the end of the file is clamped to the bytes present.
        let mut f = riff(b'9', &[chunk(b"DISP", &dib(4, 4, [1, 2, 3]))]);
        f[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(extract(&f).unwrap().width(), 4);
        let good = riff(b'9', &[chunk(b"DISP", &dib(4, 4, [1, 2, 3]))]);
        for cut in [5, 11, 14, 20, 40, 60] {
            assert!(extract(&good[..cut]).is_err(), "cut {cut}");
        }
        // DIB declaring 2^31-1 x 2^31-1.
        let mut huge = dib(4, 4, [1, 2, 3]);
        huge[4..12].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0x7F, 0xFF, 0xFF, 0xFF, 0x7F]);
        assert_eq!(
            extract(&riff(b'9', &[chunk(b"DISP", &huge)])),
            Err(DecodeError::TooLarge)
        );
        // Deeply nested LISTs stay bounded.
        let mut nested = chunk(b"DISP", &dib(2, 2, [9, 9, 9]));
        for _ in 0..64 {
            nested = chunk(b"LIST", &[b"xxxx".as_slice(), &nested].concat());
        }
        assert!(extract(&riff(b'9', &[nested])).is_err());
    }
}
