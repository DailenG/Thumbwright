//! Khronos KTX 1 textures (`.ktx`): first image of the first mip level of 8-bit
//! RGB / RGBA / LUMINANCE / LUMINANCE_ALPHA data and ETC1 RGB blocks. Images are
//! bottom-up unless `KTXorientation` says `T=d`.

use image::{DynamicImage, GrayAlphaImage};

use super::{
    DecodeError, Order, Result, check_alloc, check_dimensions, flip_rows, gray_image, pixel_buffer,
    rgb_image, rgba_image, slice,
};

const IDENTIFIER: [u8; 12] = [
    0xAB, b'K', b'T', b'X', b' ', b'1', b'1', 0xBB, b'\r', b'\n', 0x1A, b'\n',
];
const HEADER_LEN: usize = 64;
const ENDIANNESS: u32 = 0x0403_0201;

const GL_UNSIGNED_BYTE: u32 = 0x1401;
const GL_RED: u32 = 0x1903;
const GL_ALPHA: u32 = 0x1906;
const GL_RGB: u32 = 0x1907;
const GL_RGBA: u32 = 0x1908;
const GL_LUMINANCE: u32 = 0x1909;
const GL_LUMINANCE_ALPHA: u32 = 0x190A;
const GL_ETC1_RGB8_OES: u32 = 0x8D64;

const ETC1_MODIFIERS: [[i32; 4]; 8] = [
    [2, 8, -2, -8],
    [5, 17, -5, -17],
    [9, 29, -9, -29],
    [13, 42, -13, -42],
    [18, 60, -18, -60],
    [24, 80, -24, -80],
    [33, 106, -33, -106],
    [47, 183, -47, -183],
];

/// Whether `KTXorientation` declares the rows top-down (`T=d`).
fn top_down(data: &[u8], order: Order, kv_at: usize, kv_len: usize) -> bool {
    let Some(kv) = slice(data, kv_at, kv_len) else {
        return false;
    };
    let mut pos = 0usize;
    while let Some(size) = order.u32(kv, pos) {
        let size = size as usize;
        let Some(entry) = slice(kv, pos + 4, size) else {
            break;
        };
        if let Some(sep) = entry.iter().position(|b| *b == 0) {
            let (key, value) = (&entry[..sep], &entry[sep + 1..]);
            if key == b"KTXorientation" {
                return value.windows(3).any(|w| w == b"T=d");
            }
        }
        // Entries are padded to a multiple of four bytes.
        let Some(next) = size
            .checked_add(3)
            .map(|s| s / 4 * 4)
            .and_then(|s| pos.checked_add(4 + s))
        else {
            break;
        };
        pos = next;
    }
    false
}

fn expand5(v: i32) -> i32 {
    (v << 3) | (v >> 2)
}

/// Decodes one 8-byte ETC1 block into `out[y * 4 + x]`.
fn etc1_block(b: &[u8], out: &mut [[u8; 3]; 16]) {
    let diff = b[3] & 2 != 0;
    let flip = b[3] & 1 != 0;
    let (mut c1, mut c2) = ([0i32; 3], [0i32; 3]);
    for (c, &raw) in b[..3].iter().enumerate() {
        let byte = i32::from(raw);
        if diff {
            let base = byte >> 3;
            let delta = match byte & 7 {
                d @ 0..=3 => d,
                d => d - 8,
            };
            c1[c] = expand5(base);
            c2[c] = expand5((base + delta).clamp(0, 31));
        } else {
            c1[c] = (byte >> 4) * 17;
            c2[c] = (byte & 15) * 17;
        }
    }
    let cw = [usize::from(b[3] >> 5), usize::from((b[3] >> 2) & 7)];
    let msb = u32::from(u16::from_be_bytes([b[4], b[5]]));
    let lsb = u32::from(u16::from_be_bytes([b[6], b[7]]));
    for x in 0..4usize {
        for y in 0..4usize {
            let bit = x * 4 + y;
            let second = if flip { y >= 2 } else { x >= 2 };
            let (base, table) = if second { (c2, cw[1]) } else { (c1, cw[0]) };
            let sel = (((msb >> bit) & 1) << 1 | ((lsb >> bit) & 1)) as usize;
            let m = ETC1_MODIFIERS[table][sel];
            out[y * 4 + x] = [
                (base[0] + m).clamp(0, 255) as u8,
                (base[1] + m).clamp(0, 255) as u8,
                (base[2] + m).clamp(0, 255) as u8,
            ];
        }
    }
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if !data.starts_with(&IDENTIFIER) {
        return Err(DecodeError::Unsupported);
    }
    if data.len() < HEADER_LEN {
        return Err(DecodeError::malformed("KTX header truncated"));
    }
    let order = [Order::Little, Order::Big]
        .into_iter()
        .find(|o| o.u32(data, 12) == Some(ENDIANNESS))
        .ok_or_else(|| DecodeError::malformed("KTX endianness marker invalid"))?;
    let field = |i: usize| order.u32(data, 16 + i * 4).unwrap_or(0);
    let (gl_type, gl_format, gl_internal) = (field(0), field(2), field(3));
    let (width, height, depth) = (field(5), field(6), field(7));
    let kv_len = field(11) as usize;
    if depth > 1 {
        return Err(DecodeError::Unsupported);
    }
    check_dimensions(width, height)?;

    let image_size_at = HEADER_LEN
        .checked_add(kv_len)
        .ok_or_else(|| DecodeError::malformed("KTX key/value data too large"))?;
    let image_size = order
        .u32(data, image_size_at)
        .ok_or_else(|| DecodeError::malformed("KTX image data missing"))?
        as usize;
    let image_at = image_size_at + 4;
    let flip = !top_down(data, order, HEADER_LEN, kv_len);
    let (w, h) = (width as usize, height as usize);

    if gl_type == 0 && gl_internal == GL_ETC1_RGB8_OES {
        let (bx, by) = (w.div_ceil(4), h.div_ceil(4));
        let needed = bx * by * 8;
        if image_size < needed {
            return Err(DecodeError::malformed("KTX image smaller than its blocks"));
        }
        let blocks = slice(data, image_at, needed)
            .ok_or_else(|| DecodeError::malformed("KTX ETC1 data truncated"))?;
        let mut out = pixel_buffer(width, height, 3)?;
        let mut block = [[0u8; 3]; 16];
        for (i, b) in blocks.as_chunks::<8>().0.iter().enumerate() {
            etc1_block(b, &mut block);
            let (ox, oy) = (i % bx * 4, i / bx * 4);
            for (k, px) in block.iter().enumerate() {
                let (x, y) = (ox + k % 4, oy + k / 4);
                if x < w && y < h {
                    out[(y * w + x) * 3..(y * w + x) * 3 + 3].copy_from_slice(px);
                }
            }
        }
        if flip {
            flip_rows(&mut out, w * 3);
        }
        return rgb_image(width, height, out);
    }

    let channels: usize = match (gl_type, gl_format) {
        (GL_UNSIGNED_BYTE, GL_RGB) => 3,
        (GL_UNSIGNED_BYTE, GL_RGBA) => 4,
        (GL_UNSIGNED_BYTE, GL_LUMINANCE | GL_RED | GL_ALPHA) => 1,
        (GL_UNSIGNED_BYTE, GL_LUMINANCE_ALPHA) => 2,
        _ => return Err(DecodeError::Unsupported),
    };
    check_alloc(u64::from(width) * u64::from(height) * channels as u64)?;
    // Rows are padded to four bytes (GL unpack alignment).
    let row = w * channels;
    let stride = row.div_ceil(4) * 4;
    let needed = stride.checked_mul(h).ok_or(DecodeError::TooLarge)?;
    if image_size < needed {
        return Err(DecodeError::malformed("KTX image smaller than its pixels"));
    }
    let src = slice(data, image_at, needed)
        .ok_or_else(|| DecodeError::malformed("KTX pixel data truncated"))?;
    let mut out = pixel_buffer(width, height, channels as u64)?;
    for (d, s) in out.chunks_exact_mut(row).zip(src.chunks_exact(stride)) {
        d.copy_from_slice(&s[..row]);
    }
    if flip {
        flip_rows(&mut out, row);
    }
    match channels {
        1 => gray_image(width, height, out),
        2 => GrayAlphaImage::from_raw(width, height, out)
            .map(DynamicImage::ImageLumaA8)
            .ok_or_else(|| DecodeError::malformed("pixel buffer size mismatch")),
        3 => rgb_image(width, height, out),
        _ => rgba_image(width, height, out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(
        w: u32,
        h: u32,
        gl_type: u32,
        format: u32,
        internal: u32,
        kv: &[u8],
        image: &[u8],
    ) -> Vec<u8> {
        let mut f = IDENTIFIER.to_vec();
        f.extend(ENDIANNESS.to_le_bytes());
        for v in [
            gl_type,
            1,
            format,
            internal,
            format,
            w,
            h,
            0,
            0,
            1,
            1,
            kv.len() as u32,
        ] {
            f.extend(v.to_le_bytes());
        }
        f.extend(kv);
        f.extend((image.len() as u32).to_le_bytes());
        f.extend(image);
        f
    }

    fn orientation_kv(value: &str) -> Vec<u8> {
        let mut entry = b"KTXorientation\0".to_vec();
        entry.extend(value.as_bytes());
        entry.push(0);
        let mut kv = (entry.len() as u32).to_le_bytes().to_vec();
        kv.extend(&entry);
        while !kv.len().is_multiple_of(4) {
            kv.push(0);
        }
        kv
    }

    #[test]
    fn rgb_rows_are_padded_and_bottom_up() {
        // 2x2 RGB: 6 bytes per row padded to 8. File order: bottom row first.
        let image = [255, 0, 0, 0, 255, 0, 0, 0, 0, 0, 255, 255, 255, 255, 0, 0];
        let f = file(2, 2, GL_UNSIGNED_BYTE, GL_RGB, GL_RGB, &[], &image);
        let img = decode(&f).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (2, 2));
        assert_eq!(img.get_pixel(0, 1).0, [255, 0, 0]);
        assert_eq!(img.get_pixel(1, 1).0, [0, 255, 0]);
        assert_eq!(img.get_pixel(0, 0).0, [0, 0, 255]);
        // The same data declared top-down keeps its order.
        let f = file(
            2,
            2,
            GL_UNSIGNED_BYTE,
            GL_RGB,
            GL_RGB,
            &orientation_kv("S=r,T=d"),
            &image,
        );
        assert_eq!(decode(&f).unwrap().to_rgb8().get_pixel(0, 0).0, [255, 0, 0]);
    }

    #[test]
    fn etc1_individual_mode_block() {
        // Individual mode: colour1 = colour2 = (15, 0, 0) in 4 bits; table 0 (+-2, +-8).
        // All selectors 01 (+8) except pixel 0 which is 11 (-8).
        let block = [0xFF, 0x00, 0x00, 0x00, 0x00, 0x01, 0xFF, 0xFF];
        let f = file(
            4,
            4,
            0,
            0,
            GL_ETC1_RGB8_OES,
            &orientation_kv("S=r,T=d"),
            &block,
        );
        let img = decode(&f).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (4, 4));
        assert_eq!(img.get_pixel(0, 0).0, [247, 0, 0]);
        assert_eq!(img.get_pixel(3, 3).0, [255, 8, 8]);
    }

    #[test]
    fn hostile_inputs_are_rejected() {
        assert_eq!(decode(b"not ktx"), Err(DecodeError::Unsupported));
        assert!(matches!(
            decode(&IDENTIFIER),
            Err(DecodeError::Malformed(_))
        ));
        let f = file(65535, 65535, GL_UNSIGNED_BYTE, GL_RGBA, GL_RGBA, &[], &[]);
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
        let f = file(20000, 20000, GL_UNSIGNED_BYTE, GL_RGBA, GL_RGBA, &[], &[]);
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
        let f = file(u32::MAX, u32::MAX, 0, 0, GL_ETC1_RGB8_OES, &[], &[]);
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
        // Image size field larger than the data present.
        let f = file(64, 64, GL_UNSIGNED_BYTE, GL_RGB, GL_RGB, &[], &[0; 16]);
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        // Key/value length beyond the file.
        let mut f = file(1, 1, GL_UNSIGNED_BYTE, GL_RGB, GL_RGB, &[], &[0; 4]);
        f[16 + 11 * 4..16 + 12 * 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
    }
}
