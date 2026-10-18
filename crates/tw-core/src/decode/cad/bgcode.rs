//! PrusaSlicer binary G-code (`.bgcode`): thumbnail blocks.
//!
//! Layout (libbgcode): file header `GCDE`, version `u32`, checksum type `u16` (0 none,
//! 1 CRC32), then blocks. A block is `type u16`, `compression u16`, `uncompressed size
//! u32`, `compressed size u32` (only when compressed), the parameter bytes (2 bytes for
//! every block type except thumbnails, which carry `format u16` (0 PNG, 1 JPG, 2 QOI),
//! `width u16`, `height u16`), the payload, and a CRC32 when the file uses checksums.
//! Block type 5 is a thumbnail. Thumbnails precede the G-code blocks (type 1), so the
//! walk stops at the first G-code block; compression 1 is zlib deflate, 2 and 3 are
//! heatshrink (not supported: such blocks are skipped).

use image::DynamicImage;

use super::{MAX_PREVIEW_BYTES, decode_preview, le16, le32};
use crate::decode::{DecodeError, Result};

const MAGIC: &[u8; 4] = b"GCDE";
const HEADER_LEN: usize = 10;
const BLOCK_GCODE: u16 = 1;
const BLOCK_THUMBNAIL: u16 = 5;
/// Blocks inspected before the G-code stream (a handful in practice).
const MAX_BLOCKS: usize = 64;
const MAX_THUMBNAILS: usize = 16;
const MAX_ATTEMPTS: usize = 4;

struct Thumb<'a> {
    area: u64,
    compression: u16,
    uncompressed: usize,
    payload: &'a [u8],
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if data.get(..4) != Some(MAGIC) {
        return Err(DecodeError::Unsupported);
    }
    let checksum = le16(data, 8).ok_or_else(|| DecodeError::malformed("short header"))?;
    let crc_len = match checksum {
        0 => 0,
        1 => 4,
        _ => return Err(DecodeError::malformed("unknown checksum type")),
    };
    let mut thumbs: Vec<Thumb> = Vec::new();
    let mut pos = HEADER_LEN;
    for _ in 0..MAX_BLOCKS {
        let (Some(kind), Some(compression), Some(uncompressed)) =
            (le16(data, pos), le16(data, pos + 2), le32(data, pos + 4))
        else {
            break;
        };
        if kind == BLOCK_GCODE {
            break;
        }
        if kind > BLOCK_THUMBNAIL || compression > 3 {
            return Err(DecodeError::malformed("invalid block header"));
        }
        let mut at = pos + 8;
        let stored = if compression == 0 {
            uncompressed
        } else {
            let c = le32(data, at).ok_or_else(|| DecodeError::malformed("short block header"))?;
            at += 4;
            c
        };
        let params = if kind == BLOCK_THUMBNAIL { 6 } else { 2 };
        let width = le16(data, at + 2).map(u64::from);
        let height = le16(data, at + 4).map(u64::from);
        let payload_at = at
            .checked_add(params)
            .ok_or_else(|| DecodeError::malformed("block offset overflow"))?;
        let end = payload_at
            .checked_add(stored as usize)
            .and_then(|e| e.checked_add(crc_len))
            .filter(|&e| e <= data.len())
            .ok_or_else(|| DecodeError::malformed("block extends past end of file"))?;
        if kind == BLOCK_THUMBNAIL
            && let (Some(w), Some(h), true) = (width, height, thumbs.len() < MAX_THUMBNAILS)
        {
            thumbs.push(Thumb {
                area: w * h,
                compression,
                uncompressed: uncompressed as usize,
                payload: &data[payload_at..payload_at + stored as usize],
            });
        }
        pos = end;
    }

    thumbs.sort_by_key(|t| std::cmp::Reverse(t.area));
    let mut last = DecodeError::Unsupported;
    for t in thumbs.iter().take(MAX_ATTEMPTS) {
        match decode_thumb(t) {
            Ok(img) => return Ok(img),
            Err(e) if e.falls_through() => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

fn decode_thumb(t: &Thumb) -> Result<DynamicImage> {
    match t.compression {
        0 => decode_preview(t.payload),
        1 => {
            if t.uncompressed > MAX_PREVIEW_BYTES {
                return Err(DecodeError::TooLarge);
            }
            let bytes =
                miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(t.payload, t.uncompressed)
                    .map_err(|e| match e.status {
                        miniz_oxide::inflate::TINFLStatus::HasMoreOutput => DecodeError::TooLarge,
                        _ => DecodeError::malformed("thumbnail deflate stream"),
                    })?;
            decode_preview(&bytes)
        }
        // Heatshrink: no pure-Rust decoder is bundled for it.
        _ => Err(DecodeError::Unsupported),
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;

    fn file(checksum: u16, blocks: &[Vec<u8>]) -> Vec<u8> {
        let mut f = b"GCDE".to_vec();
        f.extend_from_slice(&1u32.to_le_bytes());
        f.extend_from_slice(&checksum.to_le_bytes());
        for b in blocks {
            f.extend_from_slice(b);
            if checksum == 1 {
                f.extend_from_slice(&[0xAA; 4]);
            }
        }
        f
    }

    fn block(
        kind: u16,
        compression: u16,
        uncompressed: u32,
        stored: &[u8],
        params: &[u8],
    ) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&kind.to_le_bytes());
        b.extend_from_slice(&compression.to_le_bytes());
        b.extend_from_slice(&uncompressed.to_le_bytes());
        if compression != 0 {
            b.extend_from_slice(&(stored.len() as u32).to_le_bytes());
        }
        b.extend_from_slice(params);
        b.extend_from_slice(stored);
        b
    }

    fn thumb_params(format: u16, w: u16, h: u16) -> Vec<u8> {
        [format, w, h]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect()
    }

    fn meta() -> Vec<u8> {
        block(0, 0, 4, b"k=v\n", &[0, 0])
    }

    fn gcode() -> Vec<u8> {
        block(BLOCK_GCODE, 0, 4, b"G28\n", &[0, 0])
    }

    #[test]
    fn largest_uncompressed_thumbnail_with_checksums() {
        let small = png(8, 8, [255, 0, 0]);
        let big = png(40, 30, [0, 0, 255]);
        let f = file(
            1,
            &[
                meta(),
                block(5, 0, small.len() as u32, &small, &thumb_params(0, 8, 8)),
                block(5, 0, big.len() as u32, &big, &thumb_params(0, 40, 30)),
                gcode(),
            ],
        );
        let img = decode(&f).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (40, 30));
        assert_eq!(img.get_pixel(5, 5).0, [0, 0, 255]);
    }

    #[test]
    fn deflate_thumbnail_and_jpeg() {
        let p = png(20, 12, [0, 255, 0]);
        let z = miniz_oxide::deflate::compress_to_vec_zlib(&p, 6);
        let f = file(
            0,
            &[
                block(5, 1, p.len() as u32, &z, &thumb_params(0, 20, 12)),
                gcode(),
            ],
        );
        let img = decode(&f).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (20, 12));
        assert_eq!(img.get_pixel(1, 1).0, [0, 255, 0]);

        let j = jpeg(16, 16, [200, 100, 50]);
        let f = file(
            0,
            &[block(5, 0, j.len() as u32, &j, &thumb_params(1, 16, 16))],
        );
        assert_eq!(decode(&f).unwrap().width(), 16);
    }

    #[test]
    fn heatshrink_only_is_unsupported_and_other_types_are_skipped() {
        let f = file(
            0,
            &[block(5, 2, 100, b"xxxx", &thumb_params(0, 8, 8)), gcode()],
        );
        assert_eq!(decode(&f), Err(DecodeError::Unsupported));
        let f = file(0, &[meta(), gcode()]);
        assert_eq!(decode(&f), Err(DecodeError::Unsupported));
        // A later PNG thumbnail still wins over an earlier heatshrink one.
        let p = png(8, 8, [1, 2, 3]);
        let f = file(
            0,
            &[
                block(5, 3, 100, b"xxxx", &thumb_params(0, 64, 64)),
                block(5, 0, p.len() as u32, &p, &thumb_params(0, 8, 8)),
            ],
        );
        assert_eq!(decode(&f).unwrap().width(), 8);
    }

    #[test]
    fn hostile_inputs() {
        assert_eq!(decode(b"GCOD"), Err(DecodeError::Unsupported));
        assert!(matches!(
            decode(b"GCDE\x01\x00\x00\x00"),
            Err(DecodeError::Malformed(_))
        ));
        assert!(matches!(
            decode(b"GCDE\x01\x00\x00\x00\x07\x00"),
            Err(DecodeError::Malformed(_))
        ));
        // Block length lies past the end of file.
        let mut f = file(0, &[block(5, 0, 10, b"0123456789", &thumb_params(0, 8, 8))]);
        f.truncate(f.len() - 3);
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        let mut f = file(0, &[block(5, 0, 10, b"", &thumb_params(0, 8, 8))]);
        f[14..18].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        // Deflate bomb: declared uncompressed size beyond the cap.
        let f = file(0, &[block(5, 1, u32::MAX, b"xx", &thumb_params(0, 8, 8))]);
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
        // Deflate stream larger than it declares.
        let z = miniz_oxide::deflate::compress_to_vec_zlib(&vec![0u8; 100_000], 6);
        let f = file(0, &[block(5, 1, 100, &z, &thumb_params(0, 8, 8))]);
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
        // PNG header declaring 65535 x 65535.
        let mut h = png(2, 2, [0, 0, 0]);
        h[16..20].copy_from_slice(&65535u32.to_be_bytes());
        h[20..24].copy_from_slice(&65535u32.to_be_bytes());
        let f = file(
            0,
            &[block(
                5,
                0,
                h.len() as u32,
                &h,
                &thumb_params(0, 65535, 65535),
            )],
        );
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
    }
}
