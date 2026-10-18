//! Encapsulated PostScript previews, tried in order:
//! 1. DOS EPS binary header (`C5 D0 D3 C6`): the embedded TIFF preview. A WMF preview is
//!    not decoded here (metafiles are not handled by this module).
//! 2. A `%%BeginPreview:` (EPSI) hex bitmap of 1, 2, 4 or 8 bits per pixel.
//! 3. An Illustrator-style XMP `xmpGImg:image` thumbnail.

use image::{DynamicImage, GrayImage};
use memchr::memmem;

use super::xmp;
use crate::decode::{DecodeError, Result, check_alloc, check_dimensions, decode_bytes};

const DOS_MAGIC: [u8; 4] = [0xC5, 0xD0, 0xD3, 0xC6];
/// EPSI previews live in the header comments; do not search further than this.
const PREVIEW_SEARCH_BYTES: usize = 16 * 1024 * 1024;

fn le32(d: &[u8], i: usize) -> Option<usize> {
    Some(u32::from_le_bytes(d.get(i..i.checked_add(4)?)?.try_into().ok()?) as usize)
}

pub fn extract(data: &[u8]) -> Result<DynamicImage> {
    let mut last = DecodeError::Unsupported;
    let mut keep = |r: Result<DynamicImage>| -> Result<Option<DynamicImage>> {
        match r {
            Ok(img) => Ok(Some(img)),
            Err(e) if e.falls_through() => {
                if e != DecodeError::Unsupported {
                    last = e;
                }
                Ok(None)
            }
            Err(e) => Err(e),
        }
    };

    if data.starts_with(&DOS_MAGIC)
        && let Some(img) = keep(dos_tiff(data))?
    {
        return Ok(img);
    }
    if let Some(img) = keep(epsi(data))? {
        return Ok(img);
    }
    if let Some(img) = keep(xmp::thumbnail(data))? {
        return Ok(img);
    }
    Err(last)
}

/// The TIFF preview of a DOS EPS file (header: PS, WMF, TIFF offset/length pairs).
fn dos_tiff(data: &[u8]) -> Result<DynamicImage> {
    let (Some(off), Some(len)) = (le32(data, 20), le32(data, 24)) else {
        return Err(DecodeError::malformed("eps: truncated DOS header"));
    };
    if len == 0 {
        return Err(DecodeError::Unsupported);
    }
    let tiff = off
        .checked_add(len)
        .and_then(|end| data.get(off..end))
        .ok_or_else(|| DecodeError::malformed("eps: TIFF preview outside the file"))?;
    decode_bytes(tiff)
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// `%%BeginPreview: width height depth lines`, then `lines` lines of `%` + hex digits.
/// Rows are padded to whole bytes; sample 0 is black.
fn epsi(data: &[u8]) -> Result<DynamicImage> {
    const KEY: &[u8] = b"%%BeginPreview:";
    let head = &data[..data.len().min(PREVIEW_SEARCH_BYTES)];
    let at = memmem::find(head, KEY).ok_or(DecodeError::Unsupported)?;
    let rest = &head[at + KEY.len()..];
    let line_end = rest
        .iter()
        .position(|&b| b == b'\n' || b == b'\r')
        .ok_or_else(|| DecodeError::malformed("eps: truncated preview header"))?;
    let nums: Vec<u64> = std::str::from_utf8(&rest[..line_end])
        .map_err(|_| DecodeError::malformed("eps: preview header"))?
        .split_whitespace()
        .map_while(|t| t.parse().ok())
        .collect();
    let [w, h, depth, lines] = nums[..] else {
        return Err(DecodeError::malformed("eps: preview header fields"));
    };
    if !matches!(depth, 1 | 2 | 4 | 8) {
        return Err(DecodeError::malformed("eps: preview depth"));
    }
    let (Ok(w32), Ok(h32)) = (u32::try_from(w), u32::try_from(h)) else {
        return Err(DecodeError::TooLarge);
    };
    check_dimensions(w32, h32)?;
    let row_bytes = (w * depth).div_ceil(8);
    let total = row_bytes * h;
    check_alloc(total)?;
    let total = total as usize;

    let mut packed: Vec<u8> = Vec::new();
    let mut nibble: Option<u8> = None;
    for line in rest[line_end..]
        .split(|&b| b == b'\n' || b == b'\r')
        .filter(|l| !l.is_empty())
        .take(lines.min(total as u64 * 2 + 16) as usize)
    {
        let Some(body) = line.strip_prefix(b"%") else {
            break;
        };
        if body.starts_with(b"%") {
            break; // %%EndImage / %%EndPreview
        }
        for &c in body {
            let Some(v) = hex(c) else { continue };
            match nibble.take() {
                None => nibble = Some(v),
                Some(hi) => {
                    packed.push(hi << 4 | v);
                    if packed.len() == total {
                        break;
                    }
                }
            }
        }
        if packed.len() == total {
            break;
        }
    }
    if packed.len() != total {
        return Err(DecodeError::malformed(
            "eps: preview bitmap shorter than declared",
        ));
    }

    let maxval = (1u16 << depth) - 1;
    let mut px = Vec::with_capacity(w as usize * h as usize);
    for row in packed.chunks_exact(row_bytes as usize) {
        for x in 0..w {
            let bit = x * depth;
            let byte = row[(bit / 8) as usize];
            let shift = 8 - depth - bit % 8;
            let v = u16::from(byte >> shift) & maxval;
            px.push((v * 255 / maxval) as u8);
        }
    }
    GrayImage::from_raw(w32, h32, px)
        .map(DynamicImage::ImageLuma8)
        .ok_or_else(|| DecodeError::malformed("eps: pixel buffer"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::design::testutil;

    fn dos(ps: &[u8], tiff: &[u8]) -> Vec<u8> {
        let ps_off = 30usize;
        let tiff_off = ps_off + ps.len();
        let mut f = DOS_MAGIC.to_vec();
        for v in [ps_off, ps.len(), 0, 0, tiff_off, tiff.len()] {
            f.extend((v as u32).to_le_bytes());
        }
        f.extend([0xFF, 0xFF]); // checksum (unused)
        f.extend(ps);
        f.extend(tiff);
        f
    }

    fn tiff(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        image::RgbImage::from_pixel(w, h, image::Rgb(rgb))
            .write_to(&mut out, image::ImageFormat::Tiff)
            .unwrap();
        out.into_inner()
    }

    #[test]
    fn dos_eps_tiff_preview() {
        let f = dos(b"%!PS-Adobe-3.0 EPSF-3.0\n", &tiff(9, 5, [11, 22, 33]));
        let img = extract(&f).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (9, 5));
        assert_eq!(img.get_pixel(4, 2).0, [11, 22, 33]);
    }

    #[test]
    fn epsi_bitmaps_at_each_depth() {
        // 1 bpp, 10x2: row bytes = 2. Row 0 all white, row 1 first 8 pixels black.
        let eps =
            b"%!PS-Adobe-3.0 EPSI-1.2\n%%BeginPreview: 10 2 1 2\n%FFC0\n%0000\n%%EndPreview\n";
        let img = extract(eps).unwrap().to_luma8();
        assert_eq!(img.dimensions(), (10, 2));
        assert_eq!(img.get_pixel(0, 0).0, [255]);
        assert_eq!(img.get_pixel(9, 0).0, [255]);
        assert_eq!(img.get_pixel(5, 1).0, [0]);

        // 4 bpp, 3x1 (padded to 2 bytes): levels 0, 15, 5 split over two lines.
        let eps = b"%!PS-Adobe-3.0 EPSF-3.0\r%%BeginPreview: 3 1 4 2\r%0F\r%50\r%%EndPreview\r";
        let img = extract(eps).unwrap().to_luma8();
        assert_eq!(img.dimensions(), (3, 1));
        assert_eq!(img.get_pixel(0, 0).0, [0]);
        assert_eq!(img.get_pixel(1, 0).0, [255]);
        assert_eq!(img.get_pixel(2, 0).0, [85]);

        // 8 bpp and 2 bpp.
        let eps = b"%%BeginPreview: 2 1 8 1\n%80FF\n";
        assert_eq!(extract(eps).unwrap().to_luma8().get_pixel(0, 0).0, [128]);
        let eps = b"%%BeginPreview: 4 1 2 1\n%1B\n";
        let img = extract(eps).unwrap().to_luma8();
        assert_eq!(
            (0..4).map(|x| img.get_pixel(x, 0).0[0]).collect::<Vec<_>>(),
            [0, 85, 170, 255]
        );
    }

    #[test]
    fn dos_header_without_tiff_falls_back_to_epsi_then_xmp() {
        let ps = b"%%BeginPreview: 8 1 1 1\n%0F\n";
        let mut f = dos(ps, b"");
        f[24..28].copy_from_slice(&0u32.to_le_bytes());
        let img = extract(&f).unwrap().to_luma8();
        assert_eq!(img.get_pixel(0, 0).0, [0]);
        assert_eq!(img.get_pixel(7, 0).0, [255]);

        let jpg = super::super::b64::encode(&testutil::jpeg(7, 7, [90, 90, 90]));
        let xmp = format!("%!PS-Adobe-3.0 EPSF-3.0\n<xmpGImg:image>{jpg}</xmpGImg:image>\n");
        assert_eq!(
            extract(xmp.as_bytes()).unwrap().to_rgb8().dimensions(),
            (7, 7)
        );
    }

    #[test]
    fn hostile_inputs() {
        assert_eq!(
            extract(b"%!PS-Adobe-3.0\nshowpage\n"),
            Err(DecodeError::Unsupported)
        );
        // DOS header with a TIFF range outside the file, and a truncated header.
        let mut f = dos(b"%!PS\n", &tiff(2, 2, [1, 1, 1]));
        f[24..28].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(extract(&f), Err(DecodeError::Malformed(_))));
        assert!(extract(&DOS_MAGIC).is_err());
        // Offset + length overflow.
        let mut f = dos(b"%!PS\n", &tiff(2, 2, [1, 1, 1]));
        f[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(extract(&f).is_err());
        // Huge declared EPSI size.
        assert_eq!(
            extract(b"%%BeginPreview: 65535 65535 8 10\n%00\n"),
            Err(DecodeError::TooLarge)
        );
        assert_eq!(
            extract(b"%%BeginPreview: 4294967296 1 8 1\n%00\n"),
            Err(DecodeError::TooLarge)
        );
        // Bitmap shorter than declared; bad depth; truncated header line.
        assert!(matches!(
            extract(b"%%BeginPreview: 8 8 8 1\n%00\n"),
            Err(DecodeError::Malformed(_))
        ));
        assert!(matches!(
            extract(b"%%BeginPreview: 8 8 3 1\n%00\n"),
            Err(DecodeError::Malformed(_))
        ));
        assert!(matches!(
            extract(b"%%BeginPreview: 8 8"),
            Err(DecodeError::Malformed(_))
        ));
        // Truncated TIFF preview.
        let mut t = tiff(2, 2, [1, 1, 1]);
        t.truncate(12);
        assert!(extract(&dos(b"%!PS\n", &t)).is_err());
    }
}
