//! CALS Raster Type 1 (`.cal`, `.cals`, MIL-STD-1840 / MIL-R-28002): a 2048-byte header of
//! sixteen 128-byte text records followed by one CCITT Group 4 bilevel image. The G4 stream
//! is decoded by the `image` crate's TIFF decoder (pure Rust, `fax`): the stream is wrapped
//! in a minimal in-memory TIFF whose dimensions come from `rpelcnt`, so the usual allocation
//! limits apply. `rorient` (pel path, line progression) is honoured.

use image::{DynamicImage, GrayImage, ImageFormat};

use super::{DecodeError, Result, gray_image, pixel_bytes};
use crate::decode::decode_limited;

const HEADER_LEN: usize = 2048;
const RECORD_LEN: usize = 128;

struct Header {
    width: u32,
    height: u32,
    /// Pel path and line progression directions in degrees.
    orient: (u32, u32),
}

fn parse_header(data: &[u8]) -> Result<Header> {
    let mut width_height = None;
    let mut orient = (0, 270);
    let mut rtype = None;
    for rec in data[..HEADER_LEN].chunks_exact(RECORD_LEN) {
        let text = String::from_utf8_lossy(rec);
        let text = text.trim_matches(|c: char| c == '\0' || c.is_whitespace());
        let Some((key, value)) = text.split_once(':') else {
            continue;
        };
        let value = value.trim();
        let nums =
            |s: &str| -> Option<Vec<u32>> { s.split(',').map(|n| n.trim().parse().ok()).collect() };
        match key.trim().to_ascii_lowercase().as_str() {
            "rtype" => rtype = value.parse::<u32>().ok(),
            "rpelcnt" => match nums(value).as_deref() {
                Some(&[w, h]) => width_height = Some((w, h)),
                _ => return Err(DecodeError::malformed("bad CALS rpelcnt")),
            },
            "rorient" => match nums(value).as_deref() {
                Some(&[p, l]) => orient = (p, l),
                _ => return Err(DecodeError::malformed("bad CALS rorient")),
            },
            _ => {}
        }
    }
    if rtype != Some(1) {
        return Err(DecodeError::malformed("unsupported CALS raster type"));
    }
    let (width, height) =
        width_height.ok_or_else(|| DecodeError::malformed("CALS without rpelcnt"))?;
    Ok(Header {
        width,
        height,
        orient,
    })
}

/// Zero bytes appended after the G4 data: the decoder looks ahead a few bits when reading
/// each code, and a stream without its EOFB marker would otherwise fail on its last row.
const LOOKAHEAD_PAD: usize = 8;

/// Wraps a CCITT G4 strip (MSB first, `WhiteIsZero`) in a little-endian TIFF.
fn wrap_tiff(width: u32, height: u32, strip: &[u8]) -> Vec<u8> {
    const ENTRIES: u16 = 9;
    let strip_at = 8 + 2 + u32::from(ENTRIES) * 12 + 4;
    let mut t = Vec::with_capacity(strip_at as usize + strip.len() + LOOKAHEAD_PAD);
    t.extend_from_slice(b"II*\0");
    t.extend_from_slice(&8u32.to_le_bytes());
    t.extend_from_slice(&ENTRIES.to_le_bytes());
    let mut entry = |tag: u16, ty: u16, value: u32| {
        t.extend_from_slice(&tag.to_le_bytes());
        t.extend_from_slice(&ty.to_le_bytes());
        t.extend_from_slice(&1u32.to_le_bytes());
        t.extend_from_slice(&value.to_le_bytes());
    };
    const SHORT: u16 = 3;
    const LONG: u16 = 4;
    entry(256, LONG, width);
    entry(257, LONG, height);
    entry(258, SHORT, 1); // bits per sample
    entry(259, SHORT, 4); // compression: CCITT Group 4
    entry(262, SHORT, 0); // photometric: WhiteIsZero
    entry(273, LONG, strip_at); // strip offset
    entry(277, SHORT, 1); // samples per pixel
    entry(278, LONG, height); // rows per strip
    entry(279, LONG, (strip.len() + LOOKAHEAD_PAD) as u32); // strip byte count
    t.extend_from_slice(&0u32.to_le_bytes());
    t.extend_from_slice(strip);
    t.extend_from_slice(&[0; LOOKAHEAD_PAD]);
    t
}

/// Places pixel `(i, j)` (position `i` on line `j`) at `i * pel + j * line` and returns the
/// upright image; both directions are unit vectors at multiples of 90 degrees.
fn orient(img: &GrayImage, pel_deg: u32, line_deg: u32) -> Result<GrayImage> {
    let dir = |deg: u32| -> Result<(i64, i64)> {
        match deg {
            0 => Ok((1, 0)),
            90 => Ok((0, -1)),
            180 => Ok((-1, 0)),
            270 => Ok((0, 1)),
            _ => Err(DecodeError::malformed("unsupported CALS orientation")),
        }
    };
    let (p, l) = (dir(pel_deg)?, dir(line_deg)?);
    if p.0 * l.0 + p.1 * l.1 != 0 {
        return Err(DecodeError::malformed("CALS orientation is not orthogonal"));
    }
    if (p, l) == ((1, 0), (0, 1)) {
        return Ok(img.clone());
    }
    let (w, h) = (i64::from(img.width()), i64::from(img.height()));
    let (out_w, out_h) = if p.0 != 0 { (w, h) } else { (h, w) };
    let x0 = (p.0 * (w - 1)).min(0) + (l.0 * (h - 1)).min(0);
    let y0 = (p.1 * (w - 1)).min(0) + (l.1 * (h - 1)).min(0);
    let mut out = GrayImage::new(out_w as u32, out_h as u32);
    for (i, j, px) in img.enumerate_pixels() {
        let (i, j) = (i64::from(i), i64::from(j));
        let x = i * p.0 + j * l.0 - x0;
        let y = i * p.1 + j * l.1 - y0;
        out.put_pixel(x as u32, y as u32, *px);
    }
    Ok(out)
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if data.len() < 9 || !data[..9].eq_ignore_ascii_case(b"srcdocid:") {
        return Err(DecodeError::Unsupported);
    }
    if data.len() < HEADER_LEN {
        return Err(DecodeError::malformed("CALS header truncated"));
    }
    let hd = parse_header(data)?;
    pixel_bytes(hd.width, hd.height, 1)?;
    let strip = &data[HEADER_LEN..];
    if strip.is_empty() {
        return Err(DecodeError::malformed("CALS without image data"));
    }
    // Every Group 4 row needs at least one bit, so the input bounds the row count.
    if u64::from(hd.height) > strip.len() as u64 * 8 {
        return Err(DecodeError::malformed("CALS data too short for its height"));
    }
    let tiff = wrap_tiff(hd.width, hd.height, strip);
    let (img, _) = decode_limited(&tiff, ImageFormat::Tiff)?;
    let gray = img.to_luma8();
    if gray.dimensions() != (hd.width, hd.height) {
        return Err(DecodeError::malformed("CALS image size mismatch"));
    }
    gray_image_from(orient(&gray, hd.orient.0, hd.orient.1)?)
}

fn gray_image_from(img: GrayImage) -> Result<DynamicImage> {
    let (w, h) = img.dimensions();
    gray_image(w, h, img.into_raw())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(w: u32, h: u32, orient: &str) -> Vec<u8> {
        let mut f = vec![0u8; HEADER_LEN];
        let mut put = |i: usize, s: String| {
            f[i * RECORD_LEN..i * RECORD_LEN + s.len()].copy_from_slice(s.as_bytes())
        };
        put(0, "srcdocid: NONE".into());
        put(6, "rtype: 1".into());
        put(7, format!("rorient: {orient}"));
        put(8, format!("rpelcnt: {w:06},{h:06}"));
        put(9, "rdensty: 0200".into());
        f
    }

    /// G4 for a 16x2 image: row 0 is white then a 4-pixel black run starting at x = 4;
    /// row 1 repeats row 0 (vertical mode 0 everywhere).
    ///
    /// Row 0 vs an all-white reference: horizontal mode `001`, white run 4 (`1011`), black
    /// run 4 (`011`), then the rest of the line is white: V0 (`1`) with b1 at the line end.
    /// Row 1 equals row 0: V0, V0, V0 (`111`).
    fn g4_payload() -> Vec<u8> {
        // 001 1011 011 1 | 111 => 0011 0110 1111 11(00)
        vec![0b0011_0110, 0b1111_1100]
    }

    #[test]
    fn decodes_g4_and_keeps_black_as_dark() {
        let mut f = header(16, 2, "000,270");
        f.extend(g4_payload());
        let img = decode(&f).unwrap().to_luma8();
        assert_eq!(img.dimensions(), (16, 2));
        assert_eq!(img.get_pixel(0, 0).0, [255]);
        assert_eq!(img.get_pixel(3, 0).0, [255]);
        assert_eq!(img.get_pixel(4, 0).0, [0]);
        assert_eq!(img.get_pixel(7, 0).0, [0]);
        assert_eq!(img.get_pixel(8, 0).0, [255]);
        assert_eq!(img.get_pixel(5, 1).0, [0]);
    }

    #[test]
    fn rotated_orientation_swaps_axes() {
        // Pel path down (270), lines progress right (000): the image is transposed.
        let mut f = header(16, 2, "270,000");
        f.extend(g4_payload());
        let img = decode(&f).unwrap().to_luma8();
        assert_eq!(img.dimensions(), (2, 16));
        assert_eq!(img.get_pixel(0, 4).0, [0]);
        assert_eq!(img.get_pixel(1, 5).0, [0]);
        assert_eq!(img.get_pixel(0, 8).0, [255]);
    }

    #[test]
    fn hostile_inputs() {
        assert_eq!(decode(b"II*\0").unwrap_err(), DecodeError::Unsupported);
        assert!(matches!(
            decode(&header(1, 1, "000,270")[..100]),
            Err(DecodeError::Malformed(_))
        ));
        let mut f = header(65535, 65535, "000,270");
        f.push(0xFF);
        assert_eq!(decode(&f).unwrap_err(), DecodeError::TooLarge);
        // Plausible dimensions but data that is not G4 for them.
        let mut f = header(20000, 20000, "000,270");
        f.push(0xFF);
        assert!(decode(&f).is_err());
        // Header without image data.
        assert!(matches!(
            decode(&header(8, 8, "000,270")),
            Err(DecodeError::Malformed(_))
        ));
        // Non-orthogonal orientation.
        let mut f = header(16, 2, "000,000");
        f.extend(g4_payload());
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
    }
}
