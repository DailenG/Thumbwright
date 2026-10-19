//! XMP `xmpGImg:image` thumbnails (Adobe applications embed a base64 JPEG/PNG of the first
//! page in the plain-text XMP packet). Used for InDesign and as a fallback for EPS.

use image::DynamicImage;
use memchr::memmem;

use super::b64;
use crate::decode::{DecodeError, Result, decode_bytes};
use crate::limits;

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
    // First close tag at or after the last body probed. Each open tag reuses it or scans
    // forward to a new one, so the scans never overlap: open tags without any close tag
    // cost one pass over the file, not a 16 MiB search each.
    let mut next_close: Option<usize> = None;
    for start in open.find_iter(data) {
        if candidates.len() >= MAX_CANDIDATES {
            break;
        }
        let body_start = start + OPEN.len();
        let end = match next_close {
            Some(c) if c >= body_start => c,
            _ => match close.find(&data[body_start..]) {
                Some(rel) => {
                    next_close = Some(body_start + rel);
                    body_start + rel
                }
                // No close tag anywhere after this: no later open tag can end either.
                None => break,
            },
        };
        if end - body_start <= MAX_B64 {
            candidates.push(&data[body_start..end]);
        }
    }
    if candidates.is_empty() {
        return Err(DecodeError::Unsupported);
    }
    // Several resolutions may be listed: the longest payload is the biggest image.
    candidates.sort_by_key(|c| std::cmp::Reverse(c.len()));
    let mut last = DecodeError::Unsupported;
    for text in candidates.into_iter().take(limits::MAX_PAYLOAD_ATTEMPTS) {
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

    /// DB-05: each open tag without a close tag searched up to 16 MiB for one (O(n^2)).
    #[test]
    fn open_tags_without_a_close_tag_cost_one_pass() {
        let hostile = OPEN.repeat(80_000);
        let start = std::time::Instant::now();
        assert_eq!(thumbnail(&hostile), Err(DecodeError::Unsupported));
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "xmp took {:?}",
            start.elapsed()
        );
    }

    /// A close tag far past the 16 MiB window is not trusted, however many opens precede it.
    #[test]
    fn payload_past_the_window_is_ignored() {
        let mut data = OPEN.to_vec();
        data.extend(vec![b'A'; MAX_B64 + 4]);
        data.extend_from_slice(CLOSE);
        assert_eq!(thumbnail(&data), Err(DecodeError::Unsupported));
    }

    /// DB-04: undecodable candidates are not retried without bound.
    #[test]
    fn gives_up_after_the_attempt_cap() {
        let good = testutil::jpeg(8, 8, [200, 30, 30]);
        let good_b64 = super::b64::encode(&good);
        // Valid base64 that is not an image, each longer than the real thumbnail.
        let junk: Vec<String> = (0..5)
            .map(|i| "A".repeat(good_b64.len() + 8 * (i + 1)))
            .collect();
        let mut thumbs: Vec<(&str, &str)> = junk.iter().map(|j| ("8", j.as_str())).collect();
        thumbs.push(("8", &good_b64));
        let err = thumbnail(&packet(&thumbs)).unwrap_err();
        assert!(err.falls_through(), "{err:?}");
        // Within the cap the real thumbnail behind the junk is still found.
        let mut thumbs: Vec<(&str, &str)> = junk[..limits::MAX_PAYLOAD_ATTEMPTS - 1]
            .iter()
            .map(|j| ("8", j.as_str()))
            .collect();
        thumbs.push(("8", &good_b64));
        assert_eq!(thumbnail(&packet(&thumbs)).unwrap().width(), 8);
    }
}
