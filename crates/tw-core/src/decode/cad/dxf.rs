//! ASCII DXF (`.dxf`): the `THUMBNAILIMAGE` section.
//!
//! ```text
//!   0
//! SECTION
//!   2
//! THUMBNAILIMAGE
//!  90
//!      1234          byte count of the image
//! 310
//! 28000000...        hex-encoded chunks (group 310), repeated
//!   0
//! ENDSEC
//! ```
//!
//! The decoded bytes are a DIB without a file header (newer writers may store a PNG).
//! The section sits right after `HEADER`, so only a bounded head window is scanned.
//! Binary DXF is not supported.

use image::DynamicImage;

use super::{MAX_PREVIEW_BYTES, decode_preview};
use crate::decode::{DecodeError, Result};

const WINDOW: usize = 16 << 20;
const NAME: &[u8] = b"THUMBNAILIMAGE";
const BINARY_MAGIC: &[u8] = b"AutoCAD Binary DXF";

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if data.starts_with(BINARY_MAGIC) {
        return Err(DecodeError::Unsupported);
    }
    let window = &data[..data.len().min(WINDOW)];
    let mut from = 0;
    while let Some(rel) = find(&window[from..], NAME) {
        let at = from + rel;
        from = at + NAME.len();
        if let Some(bytes) = section_bytes(window, at)? {
            return decode_preview(&bytes);
        }
    }
    Err(DecodeError::Unsupported)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Splits off the next line (without `\r\n`), advancing `pos`.
fn next_line<'a>(data: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    if *pos >= data.len() {
        return None;
    }
    let rest = &data[*pos..];
    let len = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
    *pos += (len + 1).min(rest.len());
    Some(rest[..len].trim_ascii())
}

/// The image bytes of the section whose name token sits at `at`, `None` when this
/// occurrence is not a section name (`2` group, whole line).
fn section_bytes(window: &[u8], at: usize) -> Result<Option<Vec<u8>>> {
    let line_start = window[..at]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |i| i + 1);
    let mut pos = line_start;
    let Some(name) = next_line(window, &mut pos) else {
        return Ok(None);
    };
    if name != NAME {
        return Ok(None);
    }
    // The group code line before the name must be `2`.
    let before = &window[..line_start];
    let prev_end = before.len().saturating_sub(1);
    let prev_start = before[..prev_end]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |i| i + 1);
    if before[prev_start..].trim_ascii() != b"2" {
        return Ok(None);
    }

    let mut declared: Option<usize> = None;
    let mut bytes: Vec<u8> = Vec::new();
    while let (Some(code), Some(value)) = (next_line(window, &mut pos), next_line(window, &mut pos))
    {
        match std::str::from_utf8(code)
            .ok()
            .and_then(|c| c.parse::<i32>().ok())
        {
            Some(0) => break,
            Some(90) => {
                declared = std::str::from_utf8(value).ok().and_then(|v| v.parse().ok());
                if declared.is_some_and(|d| d > MAX_PREVIEW_BYTES) {
                    return Err(DecodeError::TooLarge);
                }
            }
            Some(310) => {
                if value.len() % 2 != 0 {
                    return Err(DecodeError::malformed("odd-length hex chunk"));
                }
                if bytes.len() + value.len() / 2 > MAX_PREVIEW_BYTES {
                    return Err(DecodeError::TooLarge);
                }
                for &[hi, lo] in value.as_chunks::<2>().0 {
                    let hi = hex(hi).ok_or_else(|| DecodeError::malformed("bad hex digit"))?;
                    let lo = hex(lo).ok_or_else(|| DecodeError::malformed("bad hex digit"))?;
                    bytes.push(hi << 4 | lo);
                }
            }
            _ => {}
        }
    }
    if let Some(d) = declared {
        if bytes.len() < d {
            return Err(DecodeError::malformed("thumbnail shorter than declared"));
        }
        bytes.truncate(d);
    }
    if bytes.is_empty() {
        return Err(DecodeError::Unsupported);
    }
    Ok(Some(bytes))
}

fn hex(c: u8) -> Option<u8> {
    Some(match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;

    fn dxf(payload: &[u8], declared: usize, eol: &str) -> Vec<u8> {
        let mut s = String::new();
        let mut line = |text: &str| {
            s.push_str(text);
            s.push_str(eol);
        };
        line("  0");
        line("SECTION");
        line("  2");
        line("HEADER");
        line("  9");
        line("$ACADVER");
        line("  1");
        line("AC1027");
        line("  0");
        line("ENDSEC");
        line("  0");
        line("SECTION");
        line("  2");
        line("THUMBNAILIMAGE");
        line(" 90");
        line(&format!("{declared}"));
        for chunk in payload.chunks(128) {
            line("310");
            line(&chunk.iter().map(|b| format!("{b:02X}")).collect::<String>());
        }
        line("  0");
        line("ENDSEC");
        line("  0");
        line("SECTION");
        line("  2");
        line("ENTITIES");
        line("  0");
        line("ENDSEC");
        line("  0");
        line("EOF");
        s.into_bytes()
    }

    #[test]
    fn dib_thumbnail_unix_and_dos_line_endings() {
        let dib = dib24(5, 4);
        for eol in ["\n", "\r\n"] {
            let img = decode(&dxf(&dib, dib.len(), eol)).unwrap().to_rgb8();
            assert_eq!(img.dimensions(), (5, 4));
            assert_eq!(img.get_pixel(4, 0).0, [255, 0, 0]);
            assert_eq!(img.get_pixel(4, 3).0, [0, 0, 255]);
        }
    }

    #[test]
    fn png_thumbnail_and_trailing_pad_bytes() {
        let p = png(6, 6, [1, 100, 200]);
        let mut padded = p.clone();
        padded.extend_from_slice(&[0, 0, 0]);
        let img = decode(&dxf(&padded, p.len(), "\n")).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (6, 6));
        assert_eq!(img.get_pixel(0, 0).0, [1, 100, 200]);
    }

    #[test]
    fn files_without_a_thumbnail() {
        assert_eq!(
            decode(b"  0\nSECTION\n  2\nHEADER\n  0\nEOF\n"),
            Err(DecodeError::Unsupported)
        );
        assert_eq!(
            decode(b"AutoCAD Binary DXF\r\n\x1a\0"),
            Err(DecodeError::Unsupported)
        );
        // The name inside a string value is not a section.
        assert_eq!(
            decode(b"  1\nTHUMBNAILIMAGE\n 90\n4\n310\n00000000\n"),
            Err(DecodeError::Unsupported)
        );
        // Section with no data.
        assert_eq!(decode(&dxf(&[], 0, "\n")), Err(DecodeError::Unsupported));
    }

    #[test]
    fn hostile_sections() {
        let dib = dib24(5, 4);
        // Declared longer than present, bad hex, odd chunk, absurd declared size.
        assert!(matches!(
            decode(&dxf(&dib, dib.len() + 10, "\n")),
            Err(DecodeError::Malformed(_))
        ));
        assert!(matches!(
            decode(b"  2\nTHUMBNAILIMAGE\n 90\n2\n310\nZZZZ\n"),
            Err(DecodeError::Malformed(_))
        ));
        assert!(matches!(
            decode(b"  2\nTHUMBNAILIMAGE\n 90\n2\n310\nABC\n"),
            Err(DecodeError::Malformed(_))
        ));
        assert_eq!(
            decode(b"  2\nTHUMBNAILIMAGE\n 90\n4294967295\n310\n00\n"),
            Err(DecodeError::TooLarge)
        );
        // Truncated mid-section and truncated mid-name.
        let full = dxf(&dib, dib.len(), "\n");
        let cut = find(&full, NAME).unwrap();
        assert!(decode(&full[..cut + 20]).is_err());
        assert_eq!(decode(&full[..cut + 5]), Err(DecodeError::Unsupported));
        // DIB declaring 65535 x 65535.
        let mut d = dib24(2, 2);
        d[4..8].copy_from_slice(&65535u32.to_le_bytes());
        d[8..12].copy_from_slice(&65535u32.to_le_bytes());
        assert_eq!(decode(&dxf(&d, d.len(), "\n")), Err(DecodeError::TooLarge));
    }
}
