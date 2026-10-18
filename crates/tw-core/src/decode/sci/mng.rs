//! MNG (`.mng`): the first embedded image object. A PNG object (bare `IHDR`..`IEND`
//! chunks) is rewrapped with a PNG signature and decoded by the PNG codec; a JNG object
//! is handed to the JNG decoder. Everything else in the stream (frames, loops, deltas) is
//! ignored.

use image::DynamicImage;

use super::{DecodeError, Result, be32, check_alloc, check_dimensions, jng};

pub(super) const MNG_SIGNATURE: [u8; 8] = [0x8A, b'M', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// One chunk: length, type, data and CRC, as laid out in the file.
pub(super) struct Chunk<'a> {
    pub(super) ty: [u8; 4],
    pub(super) data: &'a [u8],
    /// The whole chunk including length, type and CRC.
    pub(super) raw: &'a [u8],
}

/// Parses the chunk at `pos`; `Ok(None)` at the clean end of the stream.
pub(super) fn chunk_at(data: &[u8], pos: usize) -> Result<Option<(Chunk<'_>, usize)>> {
    if pos == data.len() {
        return Ok(None);
    }
    let len =
        be32(data, pos).ok_or_else(|| DecodeError::malformed("chunk header truncated"))? as usize;
    let ty: [u8; 4] = data
        .get(pos + 4..pos + 8)
        .and_then(|t| t.try_into().ok())
        .ok_or_else(|| DecodeError::malformed("chunk header truncated"))?;
    let end = pos
        .checked_add(12)
        .and_then(|e| e.checked_add(len))
        .filter(|e| *e <= data.len())
        .ok_or_else(|| DecodeError::malformed("chunk extends past the end of the file"))?;
    Ok(Some((
        Chunk {
            ty,
            data: &data[pos + 8..pos + 8 + len],
            raw: &data[pos..end],
        },
        end,
    )))
}

/// CRC-32 as used by PNG chunks, over the concatenation of `parts`.
pub(super) fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for b in parts.iter().flat_map(|p| p.iter()) {
        crc ^= u32::from(*b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Serialises a chunk with a freshly computed CRC.
pub(super) fn make_chunk(ty: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 12);
    out.extend((data.len() as u32).to_be_bytes());
    out.extend(ty);
    out.extend(data);
    out.extend(crc32(&[ty, data]).to_be_bytes());
    out
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if !data.starts_with(&MNG_SIGNATURE) {
        return Err(DecodeError::Unsupported);
    }
    let mut pos = MNG_SIGNATURE.len();
    while let Some((chunk, next)) = chunk_at(data, pos)? {
        match &chunk.ty {
            b"JHDR" => return jng::decode_from(data, pos),
            b"IHDR" => return decode_png_object(data, pos),
            b"MEND" => break,
            _ => {}
        }
        pos = next;
    }
    Err(DecodeError::Unsupported)
}

/// Rewraps the chunks from the `IHDR` at `pos` through `IEND` as a PNG and decodes it.
fn decode_png_object(data: &[u8], pos: usize) -> Result<DynamicImage> {
    let (ihdr, _) = chunk_at(data, pos)?.ok_or_else(|| DecodeError::malformed("IHDR missing"))?;
    if let (Some(w), Some(h)) = (be32(ihdr.data, 0), be32(ihdr.data, 4)) {
        check_dimensions(w, h)?;
    }
    check_alloc((data.len() - pos + PNG_SIGNATURE.len()) as u64)?;
    let mut png = Vec::with_capacity(data.len() - pos + PNG_SIGNATURE.len());
    png.extend_from_slice(&PNG_SIGNATURE);
    let mut at = pos;
    while let Some((chunk, next)) = chunk_at(data, at)? {
        png.extend_from_slice(chunk.raw);
        if &chunk.ty == b"IEND" {
            break;
        }
        at = next;
    }
    Ok(super::super::decode_limited(&png, image::ImageFormat::Png)?.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_png(img: &image::DynamicImage) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    fn mng_around(png: &[u8]) -> Vec<u8> {
        let mut f = MNG_SIGNATURE.to_vec();
        f.extend(make_chunk(b"MHDR", &[0u8; 28]));
        f.extend(make_chunk(b"FRAM", &[1]));
        f.extend_from_slice(&png[8..]);
        f.extend(make_chunk(b"MEND", &[]));
        f
    }

    #[test]
    fn first_png_object_is_decoded() {
        let src =
            image::RgbImage::from_fn(3, 2, |x, y| image::Rgb([x as u8 * 100, y as u8 * 200, 7]));
        let png = encode_png(&DynamicImage::ImageRgb8(src));
        let img = decode(&mng_around(&png)).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (3, 2));
        assert_eq!(img.get_pixel(2, 1).0, [200, 200, 7]);
    }

    #[test]
    fn crc_matches_the_png_check_value() {
        assert_eq!(crc32(&[b"123456789"]), 0xCBF4_3926);
    }

    #[test]
    fn hostile_inputs_are_rejected() {
        assert_eq!(decode(b"\x89PNG\r\n\x1a\n"), Err(DecodeError::Unsupported));
        // No image object at all.
        let mut f = MNG_SIGNATURE.to_vec();
        f.extend(make_chunk(b"MHDR", &[0u8; 28]));
        f.extend(make_chunk(b"MEND", &[]));
        assert_eq!(decode(&f), Err(DecodeError::Unsupported));
        // Chunk length running past the file.
        let mut f = MNG_SIGNATURE.to_vec();
        f.extend(u32::MAX.to_be_bytes());
        f.extend(b"MHDR");
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        // IHDR declaring 65535 x 65535.
        let mut ihdr = Vec::new();
        ihdr.extend(65535u32.to_be_bytes());
        ihdr.extend(65535u32.to_be_bytes());
        ihdr.extend([8, 2, 0, 0, 0]);
        let mut f = MNG_SIGNATURE.to_vec();
        f.extend(make_chunk(b"IHDR", &ihdr));
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
        // Truncated PNG data inside the MNG.
        let src = image::RgbImage::new(4, 4);
        let png = encode_png(&DynamicImage::ImageRgb8(src));
        let f = mng_around(&png[..png.len() - 20]);
        assert!(decode(&f).is_err());
    }
}
