//! Magick Image File Format (`.miff`): a `key=value` text header ended by `:` + Ctrl-Z, then
//! the first image's pixels, uncompressed, RLE (pixel packet + run length - 1) or zlib.
//! DirectClass (gray, RGB, CMYK, optional alpha) and PseudoClass at depth 8 or 16.

use image::{DynamicImage, GrayAlphaImage};
use miniz_oxide::inflate::decompress_to_vec_zlib_with_limit;

use super::{
    DecodeError, Result, check_alloc, check_dimensions, gray_image, pixel_buffer, rgb_image,
    rgba_image, slice,
};

const MAX_HEADER: usize = 1 << 20;
const MAX_COLORS: usize = 1 << 16;

#[derive(Default)]
struct Header {
    pseudo: bool,
    columns: u64,
    rows: u64,
    depth: u32,
    matte: bool,
    colors: usize,
    compression: String,
    colorspace: String,
    float: bool,
}

/// Offset of the first `:` immediately followed by Ctrl-Z that is outside `{...}` comments.
fn find_header_end(data: &[u8]) -> Option<usize> {
    let mut in_brace = false;
    for (i, &b) in data.iter().take(MAX_HEADER).enumerate() {
        match b {
            b'{' => in_brace = true,
            b'}' => in_brace = false,
            b':' if !in_brace && data.get(i + 1) == Some(&0x1A) => return Some(i),
            _ => {}
        }
    }
    None
}

fn parse_header(text: &[u8]) -> Header {
    let mut h = Header::default();
    let s = String::from_utf8_lossy(text);
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b[i] == b'{' {
            while i < b.len() && b[i] != b'}' {
                i += 1;
            }
            i += 1;
            continue;
        }
        let key_start = i;
        while i < b.len() && b[i] != b'=' && !b[i].is_ascii_whitespace() {
            i += 1;
        }
        let key = s[key_start..i].to_ascii_lowercase();
        if b.get(i) != Some(&b'=') {
            continue;
        }
        i += 1;
        let value = if b.get(i) == Some(&b'{') {
            let start = i + 1;
            while i < b.len() && b[i] != b'}' {
                i += 1;
            }
            let v = &s[start.min(s.len())..i.min(s.len())];
            i += 1;
            v.to_string()
        } else {
            let start = i;
            while i < b.len() && !b[i].is_ascii_whitespace() {
                i += 1;
            }
            s[start..i].to_string()
        };
        let lower = value.to_ascii_lowercase();
        match key.as_str() {
            "class" => h.pseudo = lower == "pseudoclass",
            "columns" => h.columns = value.parse().unwrap_or(0),
            "rows" => h.rows = value.parse().unwrap_or(0),
            "depth" => h.depth = value.parse().unwrap_or(0),
            "matte" => h.matte = lower == "true",
            "alpha-trait" => h.matte = !matches!(lower.as_str(), "undefined" | "off" | "false"),
            "colors" => h.colors = value.parse().unwrap_or(0),
            "compression" => h.compression = lower,
            "colorspace" => h.colorspace = lower,
            "quantum-format" => h.float = lower == "floating-point",
            _ => {}
        }
    }
    h
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
    Gray,
    Rgb,
    Cmyk,
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if !data.starts_with(b"id=ImageMagick") {
        return Err(DecodeError::Unsupported);
    }
    let end = find_header_end(data)
        .ok_or_else(|| DecodeError::malformed("MIFF header has no terminator"))?;
    let h = parse_header(&data[..end]);
    let body = end + 2;

    let (width, height) = match (u32::try_from(h.columns), u32::try_from(h.rows)) {
        (Ok(w), Ok(h)) => (w, h),
        _ => return Err(DecodeError::TooLarge),
    };
    check_dimensions(width, height)?;
    if h.float || !matches!(h.depth, 8 | 16) {
        return Err(DecodeError::Unsupported);
    }
    let db = (h.depth / 8) as usize;
    let layout = match h.colorspace.as_str() {
        "gray" | "lineargray" => Layout::Gray,
        "cmyk" => Layout::Cmyk,
        _ => Layout::Rgb,
    };
    let alpha = usize::from(h.matte);

    // Colormap and packet shape.
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut pos = body;
    let (idx_bytes, packet) = if h.pseudo {
        if h.colors == 0 || h.colors > MAX_COLORS {
            return Err(DecodeError::malformed("MIFF colormap size out of range"));
        }
        let table = slice(data, pos, h.colors * 3 * db)
            .ok_or_else(|| DecodeError::malformed("MIFF colormap truncated"))?;
        palette = table
            .chunks_exact(3 * db)
            .map(|c| [c[0], c[db], c[2 * db]])
            .collect();
        pos += table.len();
        let idx = if h.colors <= 256 { 1 } else { 2 };
        (idx, idx + alpha * db)
    } else {
        let comps = match layout {
            Layout::Gray => 1,
            Layout::Rgb => 3,
            Layout::Cmyk => 4,
        } + alpha;
        (0, comps * db)
    };

    let (w, hh) = (width as usize, height as usize);
    let n = w * hh;
    let out_channels: u64 = if h.pseudo || layout != Layout::Gray {
        3 + alpha as u64
    } else {
        1 + alpha as u64
    };
    check_alloc(u64::from(width) * u64::from(height) * out_channels)?;
    check_alloc(n as u64 * packet as u64)?;

    let to_px = |p: &[u8]| -> [u8; 4] {
        let c = |i: usize| p[i * db];
        if h.pseudo {
            let idx = if idx_bytes == 1 {
                usize::from(p[0])
            } else {
                usize::from(u16::from_be_bytes([p[0], p[1]]))
            };
            let [r, g, b] = palette.get(idx).copied().unwrap_or([0, 0, 0]);
            let a = if alpha == 1 { p[idx_bytes] } else { 255 };
            return [r, g, b, a];
        }
        match layout {
            Layout::Gray => [c(0), c(0), c(0), if alpha == 1 { c(1) } else { 255 }],
            Layout::Rgb => [c(0), c(1), c(2), if alpha == 1 { c(3) } else { 255 }],
            Layout::Cmyk => {
                let k = u32::from(c(3));
                let ink = |v: u8| ((255 - u32::from(v)) * (255 - k) / 255) as u8;
                [
                    ink(c(0)),
                    ink(c(1)),
                    ink(c(2)),
                    if alpha == 1 { c(4) } else { 255 },
                ]
            }
        }
    };

    let gray = !h.pseudo && layout == Layout::Gray;
    let write = |o: &mut [u8], px: [u8; 4]| match (gray, alpha == 1) {
        (true, false) => o[0] = px[0],
        (true, true) => {
            o[0] = px[0];
            o[1] = px[3];
        }
        (false, false) => o.copy_from_slice(&px[..3]),
        (false, true) => o.copy_from_slice(&px),
    };
    let mut out = pixel_buffer(width, height, out_channels)?;
    let pixels = out.chunks_exact_mut(out_channels as usize);
    match h.compression.as_str() {
        "" | "undefined" | "none" | "zip" | "zlib" => {
            let total = n * packet;
            let inflated;
            let src = if h.compression == "zip" || h.compression == "zlib" {
                // Some writers prefix the zlib stream with its 32-bit big-endian length.
                let rest = data.get(pos..).unwrap_or(&[]);
                let plain = rest.first() == Some(&0x78)
                    && rest
                        .get(..2)
                        .is_some_and(|z| (u16::from(z[0]) << 8 | u16::from(z[1])) % 31 == 0);
                let stream = if plain {
                    rest
                } else {
                    rest.get(4..).unwrap_or(&[])
                };
                inflated = decompress_to_vec_zlib_with_limit(stream, total)
                    .map_err(|_| DecodeError::malformed("MIFF zlib data invalid or too large"))?;
                &inflated[..]
            } else {
                data.get(pos..).unwrap_or(&[])
            };
            let src = src
                .get(..total)
                .ok_or_else(|| DecodeError::malformed("MIFF pixel data truncated"))?;
            for (o, p) in pixels.zip(src.chunks_exact(packet)) {
                write(o, to_px(p));
            }
        }
        "rle" | "runlengthencoded" => {
            let mut i = pos;
            let mut pixels = pixels;
            let mut remaining = n;
            while remaining > 0 {
                let p = slice(data, i, packet)
                    .ok_or_else(|| DecodeError::malformed("MIFF RLE data truncated"))?;
                let run = usize::from(
                    *data
                        .get(i + packet)
                        .ok_or_else(|| DecodeError::malformed("MIFF RLE data truncated"))?,
                ) + 1;
                i += packet + 1;
                if run > remaining {
                    return Err(DecodeError::malformed("MIFF RLE run overruns the image"));
                }
                let px = to_px(p);
                for o in pixels.by_ref().take(run) {
                    write(o, px);
                }
                remaining -= run;
            }
        }
        _ => return Err(DecodeError::Unsupported),
    }

    Ok(match (gray, alpha == 1) {
        (true, false) => gray_image(width, height, out)?,
        (true, true) => DynamicImage::ImageLumaA8(
            GrayAlphaImage::from_raw(width, height, out)
                .ok_or_else(|| DecodeError::malformed("pixel buffer size mismatch"))?,
        ),
        (false, false) => rgb_image(width, height, out)?,
        (false, true) => rgba_image(width, height, out)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(attrs: &str, body: &[u8]) -> Vec<u8> {
        let mut f =
            format!("id=ImageMagick  version=1\n{attrs}\n{{ a comment: with colon }}\n\u{c}\n:")
                .into_bytes();
        f.push(0x1A);
        f.extend_from_slice(body);
        f
    }

    #[test]
    fn direct_class_rgb_8bit() {
        let f = file(
            "class=DirectClass colorspace=sRGB columns=2 rows=1 depth=8 date:create=2024-01-01T10:00:00",
            &[255, 0, 0, 0, 0, 255],
        );
        let img = decode(&f).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (2, 1));
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 0, 255]);
    }

    #[test]
    fn pseudo_class_rle() {
        // Colormap: 0 = black, 1 = green. Four pixels: index 1 x3 then index 0 x1.
        let mut body = vec![0, 0, 0, 0, 255, 0];
        body.extend([1, 2, 0, 0]);
        let f = file(
            "class=PseudoClass colors=2 colorspace=sRGB columns=4 rows=1 depth=8 compression=RLE",
            &body,
        );
        let img = decode(&f).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (4, 1));
        assert_eq!(img.get_pixel(2, 0).0, [0, 255, 0]);
        assert_eq!(img.get_pixel(3, 0).0, [0, 0, 0]);
    }

    #[test]
    fn zip_gray_16bit_with_and_without_length_prefix() {
        let samples: Vec<u8> = [0x0000u16, 0x8000, 0xFF00, 0x4000]
            .iter()
            .flat_map(|v| v.to_be_bytes())
            .collect();
        let z = miniz_oxide::deflate::compress_to_vec_zlib(&samples, 6);
        let attrs = "class=DirectClass colorspace=Gray columns=2 rows=2 depth=16 compression=Zip";
        let plain = file(attrs, &z);
        let mut prefixed_body = (z.len() as u32).to_be_bytes().to_vec();
        prefixed_body.extend(&z);
        let prefixed = file(attrs, &prefixed_body);
        for f in [plain, prefixed] {
            let img = decode(&f).unwrap().to_luma8();
            assert_eq!(img.as_raw(), &[0x00, 0x80, 0xFF, 0x40]);
        }
    }

    #[test]
    fn hostile_inputs_are_rejected() {
        assert_eq!(decode(b"GIF89a"), Err(DecodeError::Unsupported));
        assert!(matches!(
            decode(b"id=ImageMagick columns=2 rows=2"),
            Err(DecodeError::Malformed(_))
        ));
        let attrs = "class=DirectClass columns=65535 rows=65535 depth=8";
        assert_eq!(decode(&file(attrs, &[])), Err(DecodeError::TooLarge));
        let attrs = "class=DirectClass columns=20000 rows=20000 depth=8 matte=True";
        assert_eq!(decode(&file(attrs, &[])), Err(DecodeError::TooLarge));
        let attrs = "class=DirectClass columns=4000 rows=4000 depth=8";
        assert!(matches!(
            decode(&file(attrs, &[1, 2, 3])),
            Err(DecodeError::Malformed(_))
        ));
        // RLE run longer than the image.
        let attrs = "class=DirectClass columns=2 rows=1 depth=8 compression=RLE";
        assert!(matches!(
            decode(&file(attrs, &[1, 2, 3, 200])),
            Err(DecodeError::Malformed(_))
        ));
        // Zip bomb: 2x2 gray promises 4 bytes but the stream inflates to more.
        let z = miniz_oxide::deflate::compress_to_vec_zlib(&vec![0u8; 1 << 20], 9);
        let attrs = "class=DirectClass colorspace=Gray columns=2 rows=2 depth=8 compression=Zip";
        assert!(matches!(
            decode(&file(attrs, &z)),
            Err(DecodeError::Malformed(_))
        ));
    }
}
