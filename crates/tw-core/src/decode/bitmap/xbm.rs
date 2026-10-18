//! X Window System bitmap (`.xbm`): a C source file with `_width` / `_height` defines and a
//! `_bits[]` array. X11 uses bytes, X10 uses 16-bit words; bits run LSB first and a set bit
//! is foreground (black).

use image::DynamicImage;

use super::{DecodeError, Result, check_dimensions, gray_image, pixel_bytes};

/// Removes `/* ... */` comments (an unterminated one swallows the rest).
fn strip_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("/*") {
        out.push_str(&rest[..i]);
        out.push(' ');
        match rest[i + 2..].find("*/") {
            Some(j) => rest = &rest[i + 2 + j + 2..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

fn parse_number(tok: &str) -> Option<u32> {
    if let Some(hex) = tok.strip_prefix("0x").or_else(|| tok.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).ok()
    } else {
        tok.parse().ok()
    }
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    let text = String::from_utf8_lossy(data);
    let text = strip_comments(&text);
    let Some(brace) = text.find('{') else {
        return Err(DecodeError::Unsupported);
    };
    let (head, body) = text.split_at(brace);

    let mut width = None;
    let mut height = None;
    for line in head.lines() {
        let mut it = line.split_whitespace();
        if it.next() != Some("#define") {
            continue;
        }
        let (Some(name), Some(value)) = (it.next(), it.next()) else {
            continue;
        };
        let slot = if name.ends_with("_width") {
            &mut width
        } else if name.ends_with("_height") {
            &mut height
        } else {
            continue;
        };
        if slot.is_none() {
            *slot = Some(
                value
                    .parse::<u32>()
                    .map_err(|_| DecodeError::malformed("bad XBM dimension"))?,
            );
        }
    }
    let (Some(width), Some(height)) = (width, height) else {
        return Err(DecodeError::Unsupported);
    };
    if !head.contains("_bits") {
        return Err(DecodeError::Unsupported);
    }
    check_dimensions(width, height)?;
    pixel_bytes(width, height, 1)?;

    // X10 declares `short` (16-bit words, little-endian bit order); X11 declares `char`.
    let x10 = head.contains("short");
    let row_bytes = if x10 {
        usize::try_from(width.div_ceil(16) * 2).map_err(|_| DecodeError::TooLarge)?
    } else {
        usize::try_from(width.div_ceil(8)).map_err(|_| DecodeError::TooLarge)?
    };
    let need = row_bytes
        .checked_mul(height as usize)
        .ok_or(DecodeError::TooLarge)?;

    let body = &body[1..];
    let body = body.split('}').next().unwrap_or("");
    let mut bytes: Vec<u8> = Vec::with_capacity(need.min(data.len()));
    for tok in body
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
    {
        let v = parse_number(tok).ok_or_else(|| DecodeError::malformed("bad XBM data value"))?;
        if x10 {
            bytes.push((v & 0xFF) as u8);
            bytes.push(((v >> 8) & 0xFF) as u8);
        } else {
            if v > 0xFF {
                return Err(DecodeError::malformed("XBM byte out of range"));
            }
            bytes.push(v as u8);
        }
        if bytes.len() >= need {
            break;
        }
    }
    if bytes.len() < need {
        return Err(DecodeError::malformed("XBM data truncated"));
    }

    let (w, h) = (width as usize, height as usize);
    let mut out = vec![255u8; w * h];
    for y in 0..h {
        let row = &bytes[y * row_bytes..(y + 1) * row_bytes];
        for x in 0..w {
            if row[x / 8] >> (x % 8) & 1 == 1 {
                out[y * w + x] = 0;
            }
        }
    }
    gray_image(width, height, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const X11: &[u8] = b"#define t_width 10\n#define t_height 2\n#define t_x_hot 1\n\
static unsigned char t_bits[] = {\n  /* row 0 */ 0x01, 0x02,\n  0xff, 0x03 };\n";

    #[test]
    fn x11_bits_are_lsb_first_and_set_is_black() {
        let img = decode(X11).unwrap().to_luma8();
        assert_eq!(img.dimensions(), (10, 2));
        assert_eq!(img.get_pixel(0, 0).0, [0]); // bit 0 of 0x01
        assert_eq!(img.get_pixel(1, 0).0, [255]);
        assert_eq!(img.get_pixel(9, 0).0, [0]); // bit 1 of 0x02 -> x = 9
        assert_eq!(img.get_pixel(8, 0).0, [255]);
        assert_eq!(img.get_pixel(5, 1).0, [0]);
        assert_eq!(img.get_pixel(9, 1).0, [0]);
    }

    #[test]
    fn x10_words_are_split_little_endian() {
        let src = b"#define t_width 16\n#define t_height 1\nstatic short t_bits[] = { 0x8001 };";
        let img = decode(src).unwrap().to_luma8();
        assert_eq!(img.dimensions(), (16, 1));
        assert_eq!(img.get_pixel(0, 0).0, [0]);
        assert_eq!(img.get_pixel(15, 0).0, [0]);
        assert_eq!(img.get_pixel(7, 0).0, [255]);
    }

    #[test]
    fn hostile_inputs() {
        assert_eq!(decode(&X11[..20]).unwrap_err(), DecodeError::Unsupported);
        assert!(matches!(
            decode(&X11[..X11.len() - 12]),
            Err(DecodeError::Malformed(_))
        ));
        let huge =
            b"#define t_width 4294967295\n#define t_height 4294967295\nstatic char t_bits[] = {0};";
        assert_eq!(decode(huge).unwrap_err(), DecodeError::TooLarge);
        let huge = b"#define t_width 65535\n#define t_height 65535\nstatic char t_bits[] = {0};";
        assert_eq!(decode(huge).unwrap_err(), DecodeError::TooLarge);
        assert_eq!(
            decode(b"\x00\x01binary").unwrap_err(),
            DecodeError::Unsupported
        );
    }
}
