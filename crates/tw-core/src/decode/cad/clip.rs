//! Clip Studio Paint documents (`.clip`): the `CanvasPreview` table.
//!
//! A `.clip` file is a `CSFCHUNK` container: the 8-byte magic, the file size and the
//! offset of the first chunk (both big-endian `u64`), then chunks of `tag [8]`, `size
//! u64 BE`, data. The `CHNKSQLi` chunk holds a complete SQLite database whose
//! `CanvasPreview` table stores the canvas preview as a PNG blob. Large blobs spill
//! across SQLite overflow pages, so the database is read with [`super::sqlite`] rather
//! than scanned for a contiguous PNG.

use image::DynamicImage;

use super::sqlite::{Db, Value, parse_record};
use super::{be64, decode_preview};
use crate::decode::{DecodeError, Result};

const MAGIC: &[u8; 8] = b"CSFCHUNK";
const SQLITE_TAG: &[u8; 8] = b"CHNKSQLi";
const FOOTER_TAG: &[u8; 8] = b"CHNKFoot";
const DEFAULT_FIRST_CHUNK: usize = 24;
/// Chunks walked before the database chunk (one external-data chunk per layer block).
const MAX_CHUNKS: usize = 1 << 20;
const TABLE: &str = "CanvasPreview";

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if data.get(..8) != Some(&MAGIC[..]) {
        return Err(DecodeError::Unsupported);
    }
    let db = Db::open(sqlite_chunk(data)?)?;
    let root = db.table_root(TABLE)?.ok_or(DecodeError::Unsupported)?;

    // The largest image blob among the rows (normally exactly one row).
    let mut best: Option<Vec<u8>> = None;
    db.for_each_row(root, &mut |_, payload| {
        if let Some(cols) = parse_record(payload) {
            for col in cols {
                if let Value::Blob(blob) = col {
                    let is_image = image::guess_format(blob).is_ok();
                    if is_image && best.as_ref().is_none_or(|b| blob.len() > b.len()) {
                        best = Some(blob.to_vec());
                    }
                }
            }
        }
        Ok(true)
    })?;
    decode_preview(&best.ok_or(DecodeError::Unsupported)?)
}

/// Data of the first `CHNKSQLi` chunk.
fn sqlite_chunk(data: &[u8]) -> Result<&[u8]> {
    let first = be64(data, 16)
        .and_then(|v| usize::try_from(v).ok())
        .filter(|&v| (DEFAULT_FIRST_CHUNK..=4096).contains(&v))
        .unwrap_or(DEFAULT_FIRST_CHUNK);
    let mut pos = first;
    for _ in 0..MAX_CHUNKS {
        let tag = data
            .get(pos..pos + 8)
            .ok_or_else(|| DecodeError::malformed("chunk header past end of file"))?;
        let size = be64(data, pos + 8)
            .and_then(|s| usize::try_from(s).ok())
            .ok_or_else(|| DecodeError::malformed("short chunk header"))?;
        let body = pos + 16;
        let end = body
            .checked_add(size)
            .filter(|&e| e <= data.len())
            .ok_or_else(|| DecodeError::malformed("chunk extends past end of file"))?;
        if tag == SQLITE_TAG {
            return Ok(&data[body..end]);
        }
        if tag == FOOTER_TAG {
            break;
        }
        pos = end;
    }
    Err(DecodeError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::super::sqlite::fixtures::*;
    use super::*;

    fn chunk(tag: &[u8; 8], body: &[u8]) -> Vec<u8> {
        let mut c = tag.to_vec();
        c.extend_from_slice(&(body.len() as u64).to_be_bytes());
        c.extend_from_slice(body);
        c
    }

    /// A `.clip` whose canvas preview PNG spills over SQLite overflow pages.
    fn clip(blob: &[u8]) -> Vec<u8> {
        let rec = record(&[
            Col::Int(1),
            Col::Int(1),
            Col::Int(1),
            Col::Int(1),
            Col::Int(40),
            Col::Int(30),
            Col::Blob(blob),
        ]);
        let (cell, overflow) = leaf_cell(1, &rec, 3);
        let mut pages = vec![leaf_page(&[cell], 0)];
        pages.extend(overflow);
        let db = database(&[("Layer", 2), ("CanvasPreview", 2)], pages);
        let mut f = MAGIC.to_vec();
        f.extend_from_slice(&0u64.to_be_bytes());
        f.extend_from_slice(&24u64.to_be_bytes());
        f.extend_from_slice(&chunk(b"CHNKHead", &[0; 40]));
        f.extend_from_slice(&chunk(b"CHNKExta", &[7; 100]));
        f.extend_from_slice(&chunk(SQLITE_TAG, &db));
        f.extend_from_slice(&chunk(FOOTER_TAG, &[]));
        let len = f.len() as u64;
        f[8..16].copy_from_slice(&len.to_be_bytes());
        f
    }

    /// A PNG of `w` x `h` that does not compress, so it spans several 1 KiB pages.
    fn noisy_png(w: u32, h: u32) -> Vec<u8> {
        use image::{ImageFormat, Rgb, RgbImage};
        let mut state = 0x1234_5678u32;
        let img = RgbImage::from_fn(w, h, |x, y| {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            if x == 0 && y == 0 {
                Rgb([255, 0, 0])
            } else {
                Rgb([(state >> 16) as u8, (state >> 8) as u8, state as u8])
            }
        });
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn preview_png_reassembled_from_overflow_pages() {
        let p = noisy_png(40, 30);
        assert!(
            p.len() > 3000,
            "fixture must span several pages: {}",
            p.len()
        );
        let img = decode(&clip(&p)).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (40, 30));
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0]);
    }

    #[test]
    fn small_inline_preview() {
        let p = png(8, 6, [3, 4, 5]);
        let img = decode(&clip(&p)).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (8, 6));
        assert_eq!(img.get_pixel(7, 5).0, [3, 4, 5]);
    }

    #[test]
    fn missing_pieces() {
        assert_eq!(decode(b"PK\x03\x04"), Err(DecodeError::Unsupported));
        // No SQLite chunk before the footer.
        let mut f = MAGIC.to_vec();
        f.extend_from_slice(&[0; 8]);
        f.extend_from_slice(&24u64.to_be_bytes());
        f.extend_from_slice(&chunk(FOOTER_TAG, &[]));
        assert_eq!(decode(&f), Err(DecodeError::Unsupported));
        // Database without a CanvasPreview table.
        let db = database(&[("Layer", 2)], vec![leaf_page(&[], 0)]);
        let mut f = MAGIC.to_vec();
        f.extend_from_slice(&[0; 8]);
        f.extend_from_slice(&24u64.to_be_bytes());
        f.extend_from_slice(&chunk(SQLITE_TAG, &db));
        assert_eq!(decode(&f), Err(DecodeError::Unsupported));
    }

    #[test]
    fn hostile_files() {
        let p = noisy_png(40, 30);
        let f = clip(&p);
        // Every truncation fails cleanly.
        for cut in [10, 24, 30, 100, f.len() / 2, f.len() - 40] {
            assert!(decode(&f[..cut]).is_err(), "cut at {cut}");
        }
        // Chunk size lying past the end of the file.
        let mut lying = f.clone();
        lying[24 + 8..24 + 16].copy_from_slice(&u64::MAX.to_be_bytes());
        assert!(matches!(decode(&lying), Err(DecodeError::Malformed(_))));
        // Preview PNG header declaring 65535 x 65535.
        let mut huge = p.clone();
        huge[16..20].copy_from_slice(&65535u32.to_be_bytes());
        huge[20..24].copy_from_slice(&65535u32.to_be_bytes());
        assert_eq!(decode(&clip(&huge)), Err(DecodeError::TooLarge));
        // A blob that is not an image is ignored.
        assert_eq!(
            decode(&clip(b"not an image at all")),
            Err(DecodeError::Unsupported)
        );
    }
}
