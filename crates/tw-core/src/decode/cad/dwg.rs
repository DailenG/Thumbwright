//! AutoCAD drawings (`.dwg`): the preview image section.
//!
//! Every release from R13 (`AC1012`) to R2018 (`AC1032`) keeps a 32-bit "image seeker"
//! at file offset 0x0D: the absolute offset of the preview section, which starts with a
//! 16-byte sentinel, then `overall size u32`, `image count u8` and one record per image
//! (`code u8` (1 header data, 2 BMP, 3 WMF, 4 PNG), `start u32`, `size u32`, offsets
//! absolute). The BMP payload is a DIB without a file header; WMF previews are not
//! decoded.

use image::DynamicImage;

use super::{decode_preview, le32};
use crate::decode::{DecodeError, Result};
use crate::limits;

const SENTINEL: [u8; 16] = [
    0x1F, 0x25, 0x6D, 0x07, 0xD4, 0x36, 0x28, 0x28, 0x9D, 0x57, 0xCA, 0x3F, 0x9D, 0x44, 0x10, 0x2B,
];
const SEEKER_AT: usize = 0x0D;
const CODE_BMP: u8 = 2;
const CODE_PNG: u8 = 4;

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    let version = data
        .get(..6)
        .filter(|v| v.starts_with(b"AC10"))
        .and_then(|v| std::str::from_utf8(&v[2..]).ok())
        .and_then(|v| v.parse::<u32>().ok())
        .ok_or(DecodeError::Unsupported)?;
    if version < 1012 {
        // Releases before R13 have no preview section.
        return Err(DecodeError::Unsupported);
    }
    let seeker = le32(data, SEEKER_AT).ok_or_else(|| DecodeError::malformed("short header"))?;
    if seeker == 0 {
        return Err(DecodeError::Unsupported);
    }
    let section = data
        .get(seeker as usize..)
        .ok_or_else(|| DecodeError::malformed("preview offset outside file"))?;
    if section.get(..16) != Some(&SENTINEL[..]) {
        return Err(DecodeError::malformed("preview sentinel mismatch"));
    }
    let count = *section
        .get(20)
        .ok_or_else(|| DecodeError::malformed("short preview section"))?;
    let mut images: Vec<(u8, &[u8])> = Vec::new();
    // Directory entries may alias one payload; decoding it again cannot do better.
    let mut seen = std::collections::HashSet::new();
    for i in 0..usize::from(count) {
        let at = 21 + i * 9;
        let (Some(&code), Some(start), Some(size)) = (
            section.get(at),
            le32(section, at + 1),
            le32(section, at + 5),
        ) else {
            return Err(DecodeError::malformed("short preview directory"));
        };
        if code != CODE_BMP && code != CODE_PNG {
            continue;
        }
        if !seen.insert((start, size)) {
            continue;
        }
        let payload = (start as usize)
            .checked_add(size as usize)
            .and_then(|end| data.get(start as usize..end))
            .ok_or_else(|| DecodeError::malformed("preview image outside file"))?;
        images.push((code, payload));
    }
    // PNG (R2013+) is at least as good as the BMP, and exact; try it first.
    images.sort_by_key(|&(code, _)| std::cmp::Reverse(code));
    let mut last = DecodeError::Unsupported;
    for (_, payload) in images.into_iter().take(limits::MAX_PAYLOAD_ATTEMPTS) {
        match decode_preview(payload) {
            Ok(img) => return Ok(img),
            Err(e) if e.falls_through() => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;

    /// A minimal DWG: 0x80 header bytes, a preview section with `images`
    /// (code, payload), payloads stored after the directory.
    fn dwg(version: &str, images: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let mut f = vec![0u8; 0x80];
        f[..6].copy_from_slice(version.as_bytes());
        f[SEEKER_AT..SEEKER_AT + 4].copy_from_slice(&0x80u32.to_le_bytes());
        f.extend_from_slice(&SENTINEL);
        f.extend_from_slice(&0u32.to_le_bytes());
        f.push(images.len() as u8);
        let mut start = 0x80 + 21 + images.len() * 9;
        for (code, p) in images {
            f.push(*code);
            f.extend_from_slice(&(start as u32).to_le_bytes());
            f.extend_from_slice(&(p.len() as u32).to_le_bytes());
            start += p.len();
        }
        for (_, p) in images {
            f.extend_from_slice(p);
        }
        f
    }

    #[test]
    fn bmp_preview_without_file_header() {
        let f = dwg(
            "AC1015",
            &[(1, vec![0; 16]), (2, dib24(4, 3)), (3, vec![1; 10])],
        );
        let img = decode(&f).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (4, 3));
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0]);
        assert_eq!(img.get_pixel(0, 2).0, [0, 0, 255]);
    }

    #[test]
    fn png_preview_is_preferred() {
        let f = dwg("AC1032", &[(2, dib24(4, 3)), (4, png(9, 7, [10, 20, 30]))]);
        let img = decode(&f).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (9, 7));
        assert_eq!(img.get_pixel(3, 3).0, [10, 20, 30]);
    }

    #[test]
    fn hostile_files() {
        assert_eq!(decode(b"AC1009\0\0"), Err(DecodeError::Unsupported));
        assert_eq!(decode(b"MZ"), Err(DecodeError::Unsupported));
        assert!(matches!(decode(b"AC1015"), Err(DecodeError::Malformed(_))));
        // WMF only.
        assert_eq!(
            decode(&dwg("AC1015", &[(3, vec![0; 8])])),
            Err(DecodeError::Unsupported)
        );
        // Seeker beyond end of file.
        let mut f = dwg("AC1015", &[(2, dib24(2, 2))]);
        f[SEEKER_AT..SEEKER_AT + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        // Wrong sentinel.
        let mut f = dwg("AC1015", &[(2, dib24(2, 2))]);
        f[0x80] ^= 0xFF;
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        // Lying size and start.
        let mut f = dwg("AC1015", &[(2, dib24(2, 2))]);
        f[0x80 + 21 + 5..0x80 + 21 + 9].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        let mut f = dwg("AC1015", &[(2, dib24(2, 2))]);
        f[0x80 + 21 + 1..0x80 + 21 + 5].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        // Count claims more records than the file holds.
        let mut f = dwg("AC1015", &[(2, dib24(2, 2))]);
        f[0x80 + 20] = 255;
        assert!(decode(&f).is_err());
        // DIB declaring 65535 x 65535.
        let mut d = dib24(2, 2);
        d[4..8].copy_from_slice(&65535u32.to_le_bytes());
        d[8..12].copy_from_slice(&65535u32.to_le_bytes());
        assert_eq!(
            decode(&dwg("AC1015", &[(2, d)])),
            Err(DecodeError::TooLarge)
        );
        // Truncated preview section.
        let f = dwg("AC1015", &[(2, dib24(2, 2))]);
        assert!(decode(&f[..0x80 + 10]).is_err());
    }

    /// A PNG-looking payload that fails to decode, distinct per `n`.
    fn bad_png(n: u8) -> Vec<u8> {
        let mut p = b"\x89PNG\r\n\x1a\n".to_vec();
        p.extend_from_slice(&[n; 24]);
        p
    }

    /// DB-04: a directory of undecodable payloads is not retried without bound; each failed
    /// attempt can be a full-size decode.
    #[test]
    fn gives_up_after_the_attempt_cap() {
        let bad: Vec<(u8, Vec<u8>)> = (1..=5).map(|n| (4, bad_png(n))).collect();
        let mut images = bad.clone();
        images.push((2, dib24(4, 3)));
        let err = decode(&dwg("AC1032", &images)).unwrap_err();
        assert!(err.falls_through(), "{err:?}");
        // Within the cap the BMP behind the failures is still found.
        let mut images = bad[..limits::MAX_PAYLOAD_ATTEMPTS - 1].to_vec();
        images.push((2, dib24(4, 3)));
        assert_eq!(decode(&dwg("AC1032", &images)).unwrap().width(), 4);
    }

    /// DB-04: records that alias one payload count as one attempt.
    #[test]
    fn aliased_directory_entries_are_one_attempt() {
        let count = 12;
        let mut images: Vec<(u8, Vec<u8>)> = (0..count).map(|_| (4, bad_png(7))).collect();
        images.push((2, dib24(4, 3)));
        let mut f = dwg("AC1032", &images);
        // Point every PNG record at the first one's payload.
        let dir = 0x80 + 21;
        let (start, size) = (f[dir + 1..dir + 5].to_vec(), f[dir + 5..dir + 9].to_vec());
        for i in 1..count {
            let at = dir + i * 9;
            f[at + 1..at + 5].copy_from_slice(&start);
            f[at + 5..at + 9].copy_from_slice(&size);
        }
        assert_eq!(decode(&f).unwrap().width(), 4);
    }
}
