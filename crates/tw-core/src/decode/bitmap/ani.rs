//! Windows animated cursor (`.ani`, RIFF `ACON`): the first `icon` chunk of the `fram` list
//! is an ICO/CUR image and is decoded by the `image` ICO decoder. The chunk is only ever
//! decoded as ICO, never routed again, so containers cannot recurse.

use image::{DynamicImage, ImageFormat};

use super::{DecodeError, Result, le32};
use crate::decode::raster;

/// Walks the chunks in `data[start..end]`, returning `(id, body)` pairs; each step moves
/// forward at least 8 bytes so the loop is bounded by the input size.
fn chunks(data: &[u8], start: usize, end: usize) -> impl Iterator<Item = (&[u8], &[u8])> {
    let mut pos = start;
    std::iter::from_fn(move || {
        let id = data.get(pos..pos.checked_add(4)?)?;
        let size = le32(data, pos + 4)? as usize;
        let body_start = pos + 8;
        let body_end = body_start.checked_add(size)?.min(end).min(data.len());
        let body = data.get(body_start..body_end)?;
        pos = body_start.checked_add(size)?.checked_add(size & 1)?;
        (pos <= end.saturating_add(1)).then_some((id, body))
    })
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if data.get(..4) != Some(b"RIFF") || data.get(8..12) != Some(b"ACON") {
        return Err(DecodeError::Unsupported);
    }
    let icon = chunks(data, 12, data.len())
        .filter(|(id, body)| *id == b"LIST" && body.get(..4) == Some(b"fram"))
        .find_map(|(_, list)| {
            let end = list.len();
            chunks(list, 4, end)
                .find(|(id, _)| *id == b"icon")
                .map(|(_, body)| body)
        })
        .ok_or_else(|| DecodeError::malformed("animated cursor without frames"))?;
    raster::decode_as(icon, ImageFormat::Ico)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2x2 32-bit ICO whose single pixel colours are known.
    fn ico() -> Vec<u8> {
        let mut f = vec![0, 0, 1, 0, 1, 0];
        // dir entry: w h colors reserved planes bpp size offset
        let dib_len = 40 + 2 * 2 * 4 + 2 * 4; // header + pixels + 1bpp AND mask (4 bytes/row)
        f.extend_from_slice(&[2, 2, 0, 0, 1, 0, 32, 0]);
        f.extend_from_slice(&(dib_len as u32).to_le_bytes());
        f.extend_from_slice(&22u32.to_le_bytes());
        // BITMAPINFOHEADER (height doubled for the AND mask)
        f.extend_from_slice(&40u32.to_le_bytes());
        f.extend_from_slice(&2i32.to_le_bytes());
        f.extend_from_slice(&4i32.to_le_bytes());
        f.extend_from_slice(&1u16.to_le_bytes());
        f.extend_from_slice(&32u16.to_le_bytes());
        f.extend_from_slice(&[0; 24]);
        // Pixels BGRA, bottom row first.
        for px in [
            [0u8, 0, 255, 255],
            [0, 255, 0, 255],
            [255, 0, 0, 255],
            [9, 9, 9, 255],
        ] {
            f.extend_from_slice(&px);
        }
        f.extend_from_slice(&[0; 8]);
        f
    }

    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = id.to_vec();
        v.extend_from_slice(&(body.len() as u32).to_le_bytes());
        v.extend_from_slice(body);
        if body.len() % 2 == 1 {
            v.push(0);
        }
        v
    }

    fn ani(frames: &[Vec<u8>]) -> Vec<u8> {
        let mut list = b"fram".to_vec();
        for f in frames {
            list.extend(chunk(b"icon", f));
        }
        let mut body = b"ACON".to_vec();
        body.extend(chunk(b"anih", &[0; 36]));
        body.extend(chunk(b"LIST", &list));
        let mut f = b"RIFF".to_vec();
        f.extend_from_slice(&(body.len() as u32).to_le_bytes());
        f.extend(body);
        f
    }

    #[test]
    fn first_frame_is_decoded_as_ico() {
        let img = decode(&ani(&[ico(), ico()])).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (2, 2));
        assert_eq!(img.get_pixel(0, 1).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(1, 1).0, [0, 255, 0, 255]);
        assert_eq!(img.get_pixel(0, 0).0, [0, 0, 255, 255]);
    }

    #[test]
    fn hostile_inputs() {
        assert_eq!(
            decode(b"RIFF\0\0\0\0WAVE").unwrap_err(),
            DecodeError::Unsupported
        );
        let good = ani(&[ico()]);
        assert!(decode(&good[..good.len() - 40]).is_err());
        assert!(matches!(
            decode(b"RIFF\x04\0\0\0ACON"),
            Err(DecodeError::Malformed(_))
        ));
        // A chunk claiming 4 GiB must not loop or panic.
        let mut bad = b"RIFF\0\0\0\0ACONLIST\xFF\xFF\xFF\xFFfram".to_vec();
        bad.extend_from_slice(b"icon\xFF\xFF\xFF\xFF");
        assert!(decode(&bad).is_err());
        // Frame payload that is not an icon at all.
        assert!(decode(&ani(&[b"not an icon at all".to_vec()])).is_err());
        // DIB header declaring an enormous image.
        let mut huge = ico();
        huge[26..30].copy_from_slice(&0x7FFF_FFFFi32.to_le_bytes());
        assert!(decode(&ani(&[huge])).is_err());
    }
}
