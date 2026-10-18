//! MacPaint (`.mac`): a fixed 576x720 one-bit image, 720 PackBits rows of 72 bytes, usually
//! after a 512-byte header (optionally inside a 128-byte MacBinary wrapper). There is no
//! magic number, so the candidate data offsets are tried until a full image decodes.

use image::DynamicImage;

use super::{DecodeError, Result, gray_image, unpack_bits};

const WIDTH: u32 = 576;
const HEIGHT: u32 = 720;
const ROW_BYTES: usize = 72;
const HEADER_LEN: usize = 512;
const MACBINARY_LEN: usize = 128;

fn looks_like_macbinary(d: &[u8]) -> bool {
    d.len() > MACBINARY_LEN
        && d[0] == 0
        && d[74] == 0
        && (1..=63).contains(&d[1])
        && d.get(65..69) == Some(b"PNTG".as_slice())
}

/// Decodes the 720 rows starting at `at`; the stream must not end early.
fn rows_at(data: &[u8], at: usize) -> Result<Vec<u8>> {
    let mut src = data
        .get(at..)
        .ok_or_else(|| DecodeError::malformed("MacPaint data missing"))?;
    let mut bits = vec![0u8; ROW_BYTES * HEIGHT as usize];
    for row in bits.chunks_exact_mut(ROW_BYTES) {
        let used = unpack_bits(src, row)?;
        src = &src[used..];
    }
    Ok(bits)
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    let mut candidates = Vec::with_capacity(3);
    if looks_like_macbinary(data) {
        candidates.push(MACBINARY_LEN + HEADER_LEN);
    }
    candidates.push(HEADER_LEN);
    candidates.push(0);
    let bits = candidates
        .into_iter()
        .find_map(|at| rows_at(data, at).ok())
        .ok_or_else(|| DecodeError::malformed("not a decodable MacPaint image"))?;

    let mut out = vec![255u8; (WIDTH * HEIGHT) as usize];
    for (y, row) in bits.chunks_exact(ROW_BYTES).enumerate() {
        for x in 0..WIDTH as usize {
            if row[x / 8] >> (7 - x % 8) & 1 == 1 {
                out[y * WIDTH as usize + x] = 0;
            }
        }
    }
    gray_image(WIDTH, HEIGHT, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 720 rows: row 0 has its first byte 0xFF (8 black pixels), the rest of row 0 and all
    /// other rows are white, using repeat runs.
    fn body() -> Vec<u8> {
        let mut b = Vec::new();
        // Row 0: literal 1 byte 0xFF then a run of 71 zero bytes.
        b.extend_from_slice(&[0, 0xFF, (1i16 - 71) as u8, 0]);
        for _ in 1..720 {
            b.extend_from_slice(&[(1i16 - 72) as u8, 0]);
        }
        b
    }

    #[test]
    fn decodes_with_512_byte_header() {
        let mut f = vec![0u8; HEADER_LEN];
        f.extend(body());
        let img = decode(&f).unwrap().to_luma8();
        assert_eq!(img.dimensions(), (576, 720));
        assert_eq!(img.get_pixel(0, 0).0, [0]);
        assert_eq!(img.get_pixel(7, 0).0, [0]);
        assert_eq!(img.get_pixel(8, 0).0, [255]);
        assert_eq!(img.get_pixel(0, 1).0, [255]);
    }

    #[test]
    fn decodes_headerless_and_macbinary() {
        let img = decode(&body()).unwrap().to_luma8();
        assert_eq!(img.get_pixel(3, 0).0, [0]);

        let mut f = vec![0u8; MACBINARY_LEN + HEADER_LEN];
        f[1] = 4;
        f[65..69].copy_from_slice(b"PNTG");
        f.extend(body());
        let img = decode(&f).unwrap().to_luma8();
        assert_eq!(img.get_pixel(3, 0).0, [0]);
        assert_eq!(img.get_pixel(9, 0).0, [255]);
    }

    #[test]
    fn hostile_inputs() {
        assert!(matches!(decode(b""), Err(DecodeError::Malformed(_))));
        let mut f = vec![0u8; HEADER_LEN];
        f.extend(body());
        f.truncate(HEADER_LEN + 1000);
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        // Runs that overshoot the 72-byte row.
        assert!(matches!(
            decode(&vec![0x81u8; 4096]),
            Err(DecodeError::Malformed(_))
        ));
    }
}
