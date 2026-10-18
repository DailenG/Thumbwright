//! Blender `.blend` / `.blendN`: the `TEST` thumbnail block near the start of the file.
//!
//! Header: `BLENDER`, then either the classic 5-byte tail (`_`/`-` pointer size, `v`/`V`
//! endianness, 3-digit version; blocks have a 4-byte size) or the Blender 5 tail
//! (2-digit header size, `-`, 2-digit format version, `v`, 4-digit version; 32-byte
//! blocks with 64-bit sizes). Files may be gzip or zstd compressed; only the bytes up to
//! the thumbnail are decompressed, through a hard cap.
//!
//! The `TEST` block holds `i32 width, i32 height` then `width * height` RGBA pixels,
//! bottom row first.

use std::io::{self, Read};

use image::{DynamicImage, RgbaImage};

use crate::decode::{DecodeError, Result, check_alloc, check_dimensions};

/// Bytes skipped over blocks that precede the thumbnail (`REND` is 72 bytes).
const MAX_SKIP: u64 = 1024 * 1024;
/// Blocks inspected before giving up on finding `TEST`.
const MAX_BLOCKS: usize = 16;
/// Largest thumbnail accepted (Blender writes at most a few hundred pixels per edge).
const MAX_THUMB_BYTES: u64 = 64 * 1024 * 1024;
/// Largest zstd window accepted (Blender writes 2 MiB windows).
const ZSTD_MAX_WINDOW: u64 = 32 * 1024 * 1024;

#[derive(Clone, Copy)]
struct Layout {
    big_endian: bool,
    kind: Kind,
}

#[derive(Clone, Copy)]
enum Kind {
    /// After the code: `size i32, old pointer (4 or 8 bytes), sdna i32, count i32`.
    Classic { ptr: u64 },
    /// After the code: `sdna i32, old pointer u64, size i64, count i64`.
    Large,
}

pub fn extract(data: &[u8]) -> Result<DynamicImage> {
    if data.starts_with(&[0x1F, 0x8B]) {
        read_thumbnail(flate2::read::GzDecoder::new(data))
    } else if data.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
        let dec =
            ruzstd::decoding::StreamingDecoder::new_with_max_window_size(data, ZSTD_MAX_WINDOW)
                .map_err(|e| DecodeError::malformed(format!("zstd: {e}")))?;
        read_thumbnail(dec)
    } else if data.starts_with(b"BLENDER") {
        read_thumbnail(data)
    } else {
        Err(DecodeError::Unsupported)
    }
}

fn read_err(e: io::Error) -> DecodeError {
    DecodeError::malformed(format!("blend: {e}"))
}

fn digits(b: &[u8]) -> bool {
    !b.is_empty() && b.iter().all(u8::is_ascii_digit)
}

fn parse_header(r: &mut impl Read) -> Result<Layout> {
    let mut head = [0u8; 12];
    r.read_exact(&mut head)
        .map_err(|_| DecodeError::Unsupported)?;
    if &head[..7] != b"BLENDER" {
        return Err(DecodeError::Unsupported);
    }
    if matches!(head[7], b'_' | b'-') {
        if !matches!(head[8], b'v' | b'V') || !digits(&head[9..12]) {
            return Err(DecodeError::Unsupported);
        }
        return Ok(Layout {
            big_endian: head[8] == b'V',
            kind: Kind::Classic {
                ptr: if head[7] == b'_' { 4 } else { 8 },
            },
        });
    }
    // Blender 5: "BLENDER17-01v0500" (header size 17).
    if digits(&head[7..9]) && head[9] == b'-' && digits(&head[10..12]) {
        let size: usize = std::str::from_utf8(&head[7..9])
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if size != 17 {
            return Err(DecodeError::Unsupported);
        }
        let mut rest = [0u8; 5];
        r.read_exact(&mut rest)
            .map_err(|_| DecodeError::Unsupported)?;
        if !matches!(rest[0], b'v' | b'V') || !digits(&rest[1..5]) {
            return Err(DecodeError::Unsupported);
        }
        return Ok(Layout {
            big_endian: rest[0] == b'V',
            kind: Kind::Large,
        });
    }
    Err(DecodeError::Unsupported)
}

fn read_thumbnail(r: impl Read) -> Result<DynamicImage> {
    // Hard cap on everything read: header, skipped blocks, and the thumbnail itself.
    let cap = MAX_SKIP + MAX_THUMB_BYTES + 4096;
    let mut r = r.take(cap);
    let layout = parse_header(&mut r)?;
    let mut skipped = 0u64;
    for _ in 0..MAX_BLOCKS {
        let (mut code, len) = read_block_header(&mut r, layout)?;
        if layout.big_endian {
            code.reverse();
        }
        match &code {
            b"TEST" => return read_pixels(&mut r, layout, len),
            b"ENDB" | b"DNA1" | b"GLOB" => return Err(DecodeError::Unsupported),
            _ => {}
        }
        skipped = skipped.saturating_add(len);
        if skipped > MAX_SKIP {
            return Err(DecodeError::Unsupported);
        }
        let copied = io::copy(&mut (&mut r).take(len), &mut io::sink()).map_err(read_err)?;
        if copied != len {
            return Err(DecodeError::malformed("blend: truncated block"));
        }
    }
    Err(DecodeError::Unsupported)
}

fn read_block_header(r: &mut impl Read, l: Layout) -> Result<([u8; 4], u64)> {
    let mut code = [0u8; 4];
    r.read_exact(&mut code)
        .map_err(|_| DecodeError::malformed("blend: truncated block header"))?;
    let rest_len = match l.kind {
        Kind::Classic { ptr } => 4 + ptr as usize + 8,
        Kind::Large => 28,
    };
    let mut rest = [0u8; 28];
    let rest = &mut rest[..rest_len];
    r.read_exact(rest)
        .map_err(|_| DecodeError::malformed("blend: truncated block header"))?;
    let i32_at = |i: usize| {
        let b: [u8; 4] = rest[i..i + 4].try_into().unwrap_or_default();
        if l.big_endian {
            i32::from_be_bytes(b)
        } else {
            i32::from_le_bytes(b)
        }
    };
    let len = match l.kind {
        Kind::Classic { .. } => i64::from(i32_at(0)),
        Kind::Large => {
            let b: [u8; 8] = rest[12..20].try_into().unwrap_or_default();
            if l.big_endian {
                i64::from_be_bytes(b)
            } else {
                i64::from_le_bytes(b)
            }
        }
    };
    let len = u64::try_from(len).map_err(|_| DecodeError::malformed("blend: negative size"))?;
    Ok((code, len))
}

fn read_pixels(r: &mut impl Read, l: Layout, block_len: u64) -> Result<DynamicImage> {
    let mut dims = [0u8; 8];
    r.read_exact(&mut dims)
        .map_err(|_| DecodeError::malformed("blend: truncated thumbnail"))?;
    let rd = |b: &[u8]| {
        let b: [u8; 4] = b.try_into().unwrap_or_default();
        if l.big_endian {
            i32::from_be_bytes(b)
        } else {
            i32::from_le_bytes(b)
        }
    };
    let (w, h) = (rd(&dims[..4]), rd(&dims[4..]));
    let (Ok(w), Ok(h)) = (u32::try_from(w), u32::try_from(h)) else {
        return Err(DecodeError::malformed("blend: negative thumbnail size"));
    };
    check_dimensions(w, h)?;
    let bytes = u64::from(w) * u64::from(h) * 4;
    check_alloc(bytes)?;
    if bytes > MAX_THUMB_BYTES {
        return Err(DecodeError::TooLarge);
    }
    if block_len < 8 + bytes {
        return Err(DecodeError::malformed(
            "blend: thumbnail larger than its block",
        ));
    }
    // Grows with the data actually present, so a short file never over-allocates.
    let mut px = Vec::new();
    r.take(bytes).read_to_end(&mut px).map_err(read_err)?;
    if px.len() as u64 != bytes {
        return Err(DecodeError::malformed("blend: truncated thumbnail pixels"));
    }
    // Bottom-up rows: flip.
    let stride = w as usize * 4;
    let mut flipped = Vec::with_capacity(px.len());
    for row in px.chunks_exact(stride).rev() {
        flipped.extend_from_slice(row);
    }
    RgbaImage::from_raw(w, h, flipped)
        .map(DynamicImage::ImageRgba8)
        .ok_or_else(|| DecodeError::malformed("blend: pixel buffer"))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    /// Classic 64-bit little-endian block.
    fn block64(code: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut b = code.to_vec();
        b.extend((payload.len() as i32).to_le_bytes());
        b.extend(0x1234u64.to_le_bytes());
        b.extend(0i32.to_le_bytes());
        b.extend(1i32.to_le_bytes());
        b.extend(payload);
        b
    }

    fn thumb_payload(w: i32, h: i32, px: &[u8]) -> Vec<u8> {
        let mut p = w.to_le_bytes().to_vec();
        p.extend(h.to_le_bytes());
        p.extend(px);
        p
    }

    /// 2x2 thumbnail, bottom row first: bottom = red, top = blue.
    fn sample() -> Vec<u8> {
        let px = [
            255, 0, 0, 255, 255, 0, 0, 255, // bottom row
            0, 0, 255, 255, 0, 0, 255, 128, // top row
        ];
        let mut f = b"BLENDER-v305".to_vec();
        f.extend(block64(b"REND", &[0u8; 72]));
        f.extend(block64(b"TEST", &thumb_payload(2, 2, &px)));
        f.extend(block64(b"GLOB", &[0u8; 16]));
        f
    }

    fn check_sample(img: &DynamicImage) {
        let img = img.to_rgba8();
        assert_eq!(img.dimensions(), (2, 2));
        assert_eq!(img.get_pixel(0, 0).0, [0, 0, 255, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 0, 255, 128]);
        assert_eq!(img.get_pixel(0, 1).0, [255, 0, 0, 255]);
    }

    #[test]
    fn plain_file_thumbnail_is_flipped() {
        check_sample(&extract(&sample()).unwrap());
    }

    #[test]
    fn gzip_file() {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&sample()).unwrap();
        check_sample(&extract(&gz.finish().unwrap()).unwrap());
    }

    #[test]
    fn zstd_file() {
        let z = ruzstd::encoding::compress_to_vec(
            &sample()[..],
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        check_sample(&extract(&z).unwrap());
    }

    #[test]
    fn thirty_two_bit_big_endian_file() {
        let px = [9, 8, 7, 255];
        let mut f = b"BLENDER_V279".to_vec();
        let mut b = b"TSET".to_vec(); // big-endian files store the code reversed
        let payload: Vec<u8> = [1i32.to_be_bytes(), 1i32.to_be_bytes()]
            .concat()
            .into_iter()
            .chain(px)
            .collect();
        b.extend((payload.len() as i32).to_be_bytes());
        b.extend(0u32.to_be_bytes()); // 4-byte pointer
        b.extend(0i32.to_be_bytes());
        b.extend(1i32.to_be_bytes());
        b.extend(payload);
        f.extend(b);
        let img = extract(&f).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (1, 1));
        assert_eq!(img.get_pixel(0, 0).0, [9, 8, 7, 255]);
    }

    #[test]
    fn blender5_large_header_file() {
        let px = [1, 2, 3, 4, 5, 6, 7, 8];
        let payload = thumb_payload(1, 2, &px);
        let mut f = b"BLENDER17-01v0500".to_vec();
        f.extend(b"TEST");
        f.extend(0i32.to_le_bytes());
        f.extend(0u64.to_le_bytes());
        f.extend((payload.len() as i64).to_le_bytes());
        f.extend(1i64.to_le_bytes());
        f.extend(payload);
        let img = extract(&f).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (1, 2));
        assert_eq!(img.get_pixel(0, 0).0, [5, 6, 7, 8]);
    }

    #[test]
    fn file_without_thumbnail_is_unsupported() {
        let mut f = b"BLENDER-v305".to_vec();
        f.extend(block64(b"GLOB", &[0u8; 16]));
        assert_eq!(extract(&f), Err(DecodeError::Unsupported));
        assert_eq!(extract(b"not a blend file"), Err(DecodeError::Unsupported));
    }

    #[test]
    fn hostile_headers() {
        // Truncated.
        let s = sample();
        for cut in [3, 12, 20, 40, 100, 120, 140, 150] {
            assert!(extract(&s[..cut]).is_err(), "cut {cut}");
        }
        // Huge declared thumbnail, tiny block.
        let mut f = b"BLENDER-v305".to_vec();
        f.extend(block64(b"TEST", &thumb_payload(i32::MAX, i32::MAX, &[])));
        assert_eq!(extract(&f), Err(DecodeError::TooLarge));
        // Dimensions within budget but larger than the block.
        let mut f = b"BLENDER-v305".to_vec();
        f.extend(block64(b"TEST", &thumb_payload(1000, 1000, &[0; 16])));
        assert!(matches!(extract(&f), Err(DecodeError::Malformed(_))));
        // Negative size and negative block length.
        let mut f = b"BLENDER-v305".to_vec();
        f.extend(block64(b"TEST", &thumb_payload(-5, 4, &[])));
        assert!(matches!(extract(&f), Err(DecodeError::Malformed(_))));
        let mut f = b"BLENDER-v305".to_vec();
        let mut b = block64(b"REND", &[]);
        b[4..8].copy_from_slice(&(-1i32).to_le_bytes());
        f.extend(b);
        assert!(matches!(extract(&f), Err(DecodeError::Malformed(_))));
        // Lying block length: claims 100 MB before the thumbnail.
        let mut f = b"BLENDER-v305".to_vec();
        let mut b = block64(b"REND", &[0; 8]);
        b[4..8].copy_from_slice(&100_000_000i32.to_le_bytes());
        f.extend(b);
        assert_eq!(extract(&f), Err(DecodeError::Unsupported));
    }

    fn gzip_with_zero_padding(head: &[u8], pad_mib: usize) -> Vec<u8> {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(head).unwrap();
        let zeros = vec![0u8; 1 << 20];
        for _ in 0..pad_mib {
            gz.write_all(&zeros).unwrap();
        }
        gz.finish().unwrap()
    }

    #[test]
    fn gzip_bomb_is_rejected_before_inflating() {
        // Declares a 16000x16000 thumbnail (1 GB) backed by a highly compressible stream.
        let mut head = b"BLENDER-v305".to_vec();
        let mut b = block64(b"TEST", &thumb_payload(16000, 16000, &[]));
        b[4..8].copy_from_slice(&(8 + 16000i32 * 16000 * 4).to_le_bytes());
        head.extend(b);
        let z = gzip_with_zero_padding(&head, 4);
        assert!(z.len() < 1 << 20);
        assert_eq!(extract(&z), Err(DecodeError::TooLarge));
    }

    #[test]
    fn gzip_stream_shorter_than_declared_thumbnail_is_malformed() {
        let mut head = b"BLENDER-v305".to_vec();
        let mut b = block64(b"TEST", &thumb_payload(4000, 4000, &[]));
        b[4..8].copy_from_slice(&(8 + 4000i32 * 4000 * 4).to_le_bytes());
        head.extend(b);
        let z = gzip_with_zero_padding(&head, 4);
        assert!(matches!(extract(&z), Err(DecodeError::Malformed(_))));
    }
}
