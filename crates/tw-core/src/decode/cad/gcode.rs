//! Text G-code thumbnails (`.gcode`, `.gco`).
//!
//! Slicers embed previews as comment blocks:
//!
//! ```text
//! ; thumbnail begin 300x300 12345
//! ; iVBORw0KGgoAAAANSUhEUg...
//! ; thumbnail end
//! ```
//!
//! `thumbnail_PNG`, `thumbnail_JPG`, `thumbnail_QOI` (PrusaSlicer, OrcaSlicer, Bambu
//! Studio) and the bare `png`/`jpg` tags (Klipper-oriented Creality profiles) all share
//! the shape; the image format is sniffed from the decoded bytes. Blocks live near the
//! top of the file, some slicers append them to the end, so only a bounded head window
//! and, failing that, a bounded tail window are scanned. The largest declared thumbnail
//! that decodes wins.

use image::DynamicImage;

use super::{MAX_PREVIEW_BYTES, base64_decode, decode_preview};
use crate::decode::{DecodeError, Result};

/// Bytes scanned at each end of the file.
const WINDOW: usize = 4 << 20;
/// Thumbnail blocks collected per window.
const MAX_BLOCKS: usize = 16;
/// Decode attempts per window.
const MAX_ATTEMPTS: usize = 4;
/// Base64 text kept per block (slicer previews are a few hundred KiB at most).
const MAX_BLOCK_TEXT: usize = WINDOW;

struct Block {
    /// Declared `width * height`; orders the candidates.
    area: u64,
    text: Vec<u8>,
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if data.len() <= 2 * WINDOW {
        return try_blocks(scan(data));
    }
    let head = try_blocks(scan(&data[..WINDOW]));
    if head.is_ok() {
        return head;
    }
    let tail = &data[data.len() - WINDOW..];
    // The window may begin mid-line: drop the partial line.
    let start = tail.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1);
    match try_blocks(scan(&tail[start..])) {
        Ok(img) => Ok(img),
        Err(DecodeError::Unsupported) => head,
        Err(e) => Err(e),
    }
}

fn try_blocks(mut blocks: Vec<Block>) -> Result<DynamicImage> {
    // Largest first; the sort is stable so equal sizes keep file order.
    blocks.sort_by_key(|b| std::cmp::Reverse(b.area));
    let mut last = DecodeError::Unsupported;
    for block in blocks.iter().take(MAX_ATTEMPTS) {
        let attempt = base64_decode(&block.text, MAX_PREVIEW_BYTES)
            .and_then(|bytes| decode_preview_image(&bytes));
        match attempt {
            Ok(img) => return Ok(img),
            Err(e) if e.falls_through() => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

/// Only complete image files are accepted (a headerless DIB never appears in G-code).
fn decode_preview_image(bytes: &[u8]) -> Result<DynamicImage> {
    if image::guess_format(bytes).is_err() {
        return Err(DecodeError::malformed("thumbnail is not an image"));
    }
    decode_preview(bytes)
}

fn is_thumbnail_tag(tag: &[u8]) -> bool {
    let lower = tag.to_ascii_lowercase();
    lower.starts_with(b"thumbnail") || lower == b"png" || lower == b"jpg" || lower == b"jpeg"
}

/// `300x300` or `300*300`.
fn parse_dims(token: &[u8]) -> Option<u64> {
    let sep = token
        .iter()
        .position(|&b| b == b'x' || b == b'X' || b == b'*')?;
    let num = |t: &[u8]| -> Option<u64> { std::str::from_utf8(t).ok()?.parse().ok() };
    Some(num(&token[..sep])?.saturating_mul(num(&token[sep + 1..])?))
}

fn is_end(tokens: &[&[u8]]) -> bool {
    tokens.len() == 2 && tokens[1].eq_ignore_ascii_case(b"end") && is_thumbnail_tag(tokens[0])
}

fn scan(window: &[u8]) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut open: Option<Block> = None;
    for raw in window.split(|&b| b == b'\n') {
        let line = raw.trim_ascii();
        let Some(body) = line.strip_prefix(b";") else {
            // A block is made of comment lines only.
            open = None;
            continue;
        };
        let body = body.trim_ascii_start();
        match open.take() {
            Some(block) => {
                let tokens: Vec<&[u8]> = body
                    .split(|b| b.is_ascii_whitespace())
                    .filter(|t| !t.is_empty())
                    .take(3)
                    .collect();
                if is_end(&tokens) {
                    if !block.text.is_empty() && blocks.len() < MAX_BLOCKS {
                        blocks.push(block);
                    }
                    if blocks.len() >= MAX_BLOCKS {
                        break;
                    }
                } else if block.text.len() + body.len() <= MAX_BLOCK_TEXT {
                    let mut block = block;
                    block.text.extend_from_slice(body);
                    open = Some(block);
                }
                // Oversized block: dropped.
            }
            None => {
                let mut tokens = body
                    .split(|b| b.is_ascii_whitespace())
                    .filter(|t| !t.is_empty());
                let (Some(tag), Some(begin), Some(dims)) =
                    (tokens.next(), tokens.next(), tokens.next())
                else {
                    continue;
                };
                if begin.eq_ignore_ascii_case(b"begin") && is_thumbnail_tag(tag) {
                    open = Some(Block {
                        area: parse_dims(dims).unwrap_or(0),
                        text: Vec::new(),
                    });
                }
            }
        }
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;

    fn block(tag: &str, w: u32, h: u32, bytes: &[u8]) -> String {
        let b64 = base64(bytes);
        let mut s = format!("; {tag} begin {w}x{h} {}\n", b64.len());
        for chunk in b64.as_bytes().chunks(78) {
            s.push_str("; ");
            s.push_str(std::str::from_utf8(chunk).unwrap());
            s.push('\n');
        }
        s.push_str(&format!("; {tag} end\n"));
        s
    }

    #[test]
    fn largest_thumbnail_wins() {
        let mut g = String::from("; generated by test\n");
        g += &block("thumbnail", 16, 16, &png(16, 16, [255, 0, 0]));
        g += &block("thumbnail", 48, 32, &png(48, 32, [0, 255, 0]));
        g += &block("thumbnail_JPG", 8, 8, &jpeg(8, 8, [0, 0, 255]));
        g += "G1 X10 Y10\r\n";
        let img = decode(g.as_bytes()).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (48, 32));
        assert_eq!(img.get_pixel(10, 10).0, [0, 255, 0]);
    }

    #[test]
    fn jpg_tag_and_crlf_lines() {
        let g = block("jpg", 24, 24, &jpeg(24, 24, [10, 200, 30])).replace('\n', "\r\n");
        let img = decode(g.as_bytes()).unwrap();
        assert_eq!((img.width(), img.height()), (24, 24));
    }

    #[test]
    fn thumbnail_at_end_of_a_large_file() {
        let mut data = vec![b';'; 0];
        let filler = "G1 X1.000 Y2.000 E0.0500\n";
        while data.len() < 2 * WINDOW + 1000 {
            data.extend_from_slice(filler.as_bytes());
        }
        data.extend_from_slice(block("thumbnail", 20, 10, &png(20, 10, [9, 8, 7])).as_bytes());
        let img = decode(&data).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (20, 10));
        assert_eq!(img.get_pixel(0, 0).0, [9, 8, 7]);
    }

    #[test]
    fn broken_blocks_are_malformed_or_unsupported() {
        assert_eq!(decode(b"G28\nG1 X0\n"), Err(DecodeError::Unsupported));
        // Not base64.
        let g = "; thumbnail begin 4x4 10\n; !!!!!!!!\n; thumbnail end\n";
        assert!(matches!(
            decode(g.as_bytes()),
            Err(DecodeError::Malformed(_))
        ));
        // Valid base64 that is not an image.
        let g = format!(
            "; thumbnail begin 4x4 10\n; {}\n; thumbnail end\n",
            base64(b"hello world")
        );
        assert!(matches!(
            decode(g.as_bytes()),
            Err(DecodeError::Malformed(_))
        ));
        // Unterminated block, then a non-comment line.
        let g = "; thumbnail begin 4x4 10\n; AAAA\nG1 X1\n";
        assert_eq!(decode(g.as_bytes()), Err(DecodeError::Unsupported));
    }

    #[test]
    fn truncated_png_and_huge_declared_dimensions() {
        let full = png(32, 32, [1, 2, 3]);
        let g = block("thumbnail", 32, 32, &full[..full.len() / 2]);
        assert!(decode(g.as_bytes()).is_err());
        // PNG header claiming 65535 x 65535 with no pixel data behind it.
        let mut hdr = full[..33].to_vec();
        hdr[16..20].copy_from_slice(&65535u32.to_be_bytes());
        hdr[20..24].copy_from_slice(&65535u32.to_be_bytes());
        let g = block("thumbnail", 65535, 65535, &hdr);
        assert_eq!(decode(g.as_bytes()), Err(DecodeError::TooLarge));
    }

    #[test]
    fn oversized_block_text_is_dropped() {
        let mut g = String::from("; thumbnail begin 4x4 10\n");
        let line = format!("; {}\n", "A".repeat(1 << 20));
        for _ in 0..(MAX_BLOCK_TEXT >> 20) + 2 {
            g.push_str(&line);
        }
        g.push_str("; thumbnail end\n");
        assert_eq!(decode(g.as_bytes()), Err(DecodeError::Unsupported));
    }
}
