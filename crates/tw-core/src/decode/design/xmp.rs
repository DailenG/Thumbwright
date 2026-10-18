//! XMP `xmpGImg:image` thumbnails (Adobe applications embed a base64 JPEG/PNG of the first
//! page in the plain-text XMP packet). Used for InDesign and as a fallback for EPS.

use image::DynamicImage;
use memchr::memmem;

use super::b64;
use crate::decode::{DecodeError, Result, decode_bytes};

const OPEN: &[u8] = b"<xmpGImg:image>";
const CLOSE: &[u8] = b"</xmpGImg:image>";
/// Thumbnails considered per file (XMP packets repeat the same list in several places).
const MAX_CANDIDATES: usize = 16;
/// Largest base64 text of one thumbnail (about 12 MiB of image).
const MAX_B64: usize = 16 * 1024 * 1024;

/// The largest decodable XMP thumbnail anywhere in `data`.
pub fn thumbnail(data: &[u8]) -> Result<DynamicImage> {
    let mut candidates: Vec<&[u8]> = Vec::new();
    let open = memmem::Finder::new(OPEN);
    let close = memmem::Finder::new(CLOSE);
    for start in open.find_iter(data) {
        if candidates.len() >= MAX_CANDIDATES {
            break;
        }
        let body = &data[start + OPEN.len()..];
        let window = &body[..body.len().min(MAX_B64 + CLOSE.len())];
        if let Some(end) = close.find(window) {
            candidates.push(&window[..end]);
        }
    }
    if candidates.is_empty() {
        return Err(DecodeError::Unsupported);
    }
    // Several resolutions may be listed: the longest payload is the biggest image.
    candidates.sort_by_key(|c| std::cmp::Reverse(c.len()));
    let mut last = DecodeError::Unsupported;
    for text in candidates {
        let bytes = match b64::decode(text, MAX_B64 / 4 * 3) {
            Ok(b) => b,
            Err(e @ DecodeError::TooLarge) => return Err(e),
            Err(e) => {
                last = e;
                continue;
            }
        };
        match decode_bytes(&bytes) {
            Ok(img) => return Ok(img),
            Err(e) if e.falls_through() => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::design::testutil;

    fn packet(thumbs: &[(&str, &str)]) -> Vec<u8> {
        let mut s = String::from("junk\0<?xpacket begin='x'?><x:xmpmeta><xmp:Thumbnails><rdf:Alt>");
        for (dims, b64) in thumbs {
            s.push_str(&format!(
                "<rdf:li rdf:parseType=\"Resource\"><xmpGImg:width>{dims}</xmpGImg:width>\
                 <xmpGImg:format>JPEG</xmpGImg:format><xmpGImg:image>{b64}</xmpGImg:image></rdf:li>"
            ));
        }
        s.push_str("</rdf:Alt></xmp:Thumbnails></x:xmpmeta><?xpacket end='w'?>\0tail");
        s.into_bytes()
    }

    #[test]
    fn picks_the_largest_thumbnail_with_wrapped_base64() {
        let small = testutil::jpeg(8, 8, [200, 30, 30]);
        let big = testutil::jpeg(48, 32, [30, 30, 200]);
        let wrap = |b: &[u8]| {
            super::b64::encode(b)
                .as_bytes()
                .chunks(76)
                .map(|c| format!("{}&#xA;", std::str::from_utf8(c).unwrap()))
                .collect::<String>()
        };
        let data = packet(&[("8", &wrap(&small)), ("48", &wrap(&big))]);
        let img = thumbnail(&data).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (48, 32));
        let p = img.get_pixel(24, 16).0;
        assert!(p[2] > 150 && p[0] < 80, "{p:?}");
    }

    #[test]
    fn missing_or_truncated_packets_do_not_panic() {
        assert_eq!(thumbnail(b"no xmp here"), Err(DecodeError::Unsupported));
        let mut data = packet(&[("8", "/9j/4AAQSkZJRg==")]);
        assert!(thumbnail(&data).is_err());
        data.truncate(data.len() - 60);
        assert!(thumbnail(&data).is_err());
        // Open tag without a close tag.
        assert_eq!(
            thumbnail(b"<xmpGImg:image>AAAA"),
            Err(DecodeError::Unsupported)
        );
    }

    #[test]
    fn lying_jpeg_dimensions_are_too_large() {
        let mut jpg = testutil::jpeg(8, 8, [1, 2, 3]);
        // SOF0 height/width -> 65535 x 65535.
        let sof = jpg.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
        jpg[sof + 5..sof + 9].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
        let data = packet(&[("8", &super::b64::encode(&jpg))]);
        assert_eq!(thumbnail(&data), Err(DecodeError::TooLarge));
    }
}
