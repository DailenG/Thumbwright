//! Radiance XYZE (`.xyze`): the RGBE container with CIE XYZ primaries. The RGBE bytes are
//! decoded by the `image` HDR codec (the format tag is rewritten to `rgbe`, same length),
//! then XYZ is converted to linear sRGB primaries and encoded for display.

use image::DynamicImage;

use super::{
    DecodeError, Result, check_alloc, check_dimensions, pixel_buffer, rgb_image, srgb_encode,
};

const HEADER_SEARCH: usize = 8192;
const XYZE: &[u8] = b"32-bit_rle_xyze";
const RGBE: &[u8] = b"32-bit_rle_rgbe";

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Dimensions from the resolution line (`-Y h +X w` and the other orientations).
fn declared_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let head = &data[..data.len().min(HEADER_SEARCH)];
    let blank = find(head, b"\n\n")?;
    let line_start = blank + 2;
    let line_end = line_start + head.get(line_start..)?.iter().position(|b| *b == b'\n')?;
    let line = std::str::from_utf8(&head[line_start..line_end]).ok()?;
    let mut it = line.split_whitespace();
    let (a, na, b, nb) = (it.next()?, it.next()?, it.next()?, it.next()?);
    let (na, nb): (u32, u32) = (na.parse().ok()?, nb.parse().ok()?);
    // `Y` is the row count, `X` the column count, in either order.
    match (a.get(1..)?, b.get(1..)?) {
        ("Y", "X") => Some((nb, na)),
        ("X", "Y") => Some((na, nb)),
        _ => None,
    }
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if !data.starts_with(b"#?") {
        return Err(DecodeError::Unsupported);
    }
    let head = &data[..data.len().min(HEADER_SEARCH)];
    let tag = find(head, XYZE);
    if let Some((w, h)) = declared_dimensions(data) {
        check_dimensions(w, h)?;
        check_alloc(u64::from(w) * u64::from(h) * 12)?;
    }
    check_alloc(data.len() as u64)?;
    let mut patched = data.to_vec();
    if let Some(at) = tag {
        patched[at..at + RGBE.len()].copy_from_slice(RGBE);
    }
    let (img, _) = super::super::decode_limited(&patched, image::ImageFormat::Hdr)?;
    let rgb = match img {
        DynamicImage::ImageRgb32F(buf) => buf,
        other => other.to_rgb32f(),
    };
    let (w, h) = rgb.dimensions();
    let mut out = pixel_buffer(w, h, 3)?;
    // Radiance's XYZ (equal-energy white) to RGB matrix for the Rec. 709 primaries.
    for (o, px) in out.as_chunks_mut::<3>().0.iter_mut().zip(rgb.pixels()) {
        let [x, y, z] = px.0;
        let (r, g, b) = if tag.is_some() {
            (
                2.5651 * x - 1.1665 * y - 0.3986 * z,
                -1.0217 * x + 1.9777 * y + 0.0439 * z,
                0.0753 * x - 0.2543 * y + 1.1892 * z,
            )
        } else {
            (x, y, z)
        };
        o.copy_from_slice(&[srgb_encode(r), srgb_encode(g), srgb_encode(b)]);
    }
    rgb_image(w, h, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(format: &str, res: &str, pixels: &[[u8; 4]]) -> Vec<u8> {
        let mut f = format!("#?RADIANCE\nFORMAT={format}\n\n{res}\n").into_bytes();
        for p in pixels {
            f.extend(p);
        }
        f
    }

    #[test]
    fn xyz_white_and_black_become_srgb() {
        // X = Y = Z = 1.0 is 128 * 2^(129 - 136); flat (non-RLE) scanlines for width < 8.
        let f = file(
            "32-bit_rle_xyze",
            "-Y 1 +X 2",
            &[[128, 128, 128, 129], [0, 0, 0, 0]],
        );
        let img = decode(&f).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (2, 1));
        let w = img.get_pixel(0, 0).0;
        assert!(w.iter().all(|c| *c >= 254), "{w:?}");
        assert_eq!(img.get_pixel(1, 0).0, [0, 0, 0]);
    }

    #[test]
    fn pure_xyz_primaries_map_to_red() {
        // Linear red (1, 0, 0) in sRGB primaries is about X=.5141 Y=.2651 Z=.0241 (scaled
        // here to 2^0 with a common exponent): mantissas 131, 68, 6 at exponent 129.
        let f = file("32-bit_rle_xyze", "-Y 1 +X 1", &[[131, 68, 6, 128]]);
        let p = decode(&f).unwrap().to_rgb8().get_pixel(0, 0).0;
        assert!(p[0] > 200 && p[1] < 40 && p[2] < 40, "{p:?}");
    }

    #[test]
    fn hostile_inputs_are_rejected() {
        assert_eq!(decode(b"BM...."), Err(DecodeError::Unsupported));
        assert!(decode(b"#?RADIANCE\n").is_err());
        let f = file("32-bit_rle_xyze", "-Y 65535 +X 65535", &[]);
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
        let f = file("32-bit_rle_xyze", "-Y 4294967295 +X 4294967295", &[]);
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
        // 12000 x 12000 would need 1.7 GB of floats.
        let f = file("32-bit_rle_xyze", "-Y 12000 +X 12000", &[]);
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
        // Plausible size, missing pixels.
        let f = file("32-bit_rle_xyze", "-Y 100 +X 100", &[[1, 2, 3, 128]]);
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
    }
}
