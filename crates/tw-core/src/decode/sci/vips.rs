//! VIPS native image (`.vips`): a 64-byte header (magic `08 F2 A6 B6` in the file's own
//! byte order, width, height, bands, legacy bit depth, band format, coding, type, ...)
//! followed by the uncompressed samples in scanline, band-interleaved order. Uncoded images
//! with 8-bit or 16-bit unsigned samples and 1-4 bands in a display colour space.

use image::DynamicImage;

use super::{DecodeError, Order, Result, check_alloc, check_dimensions, interleaved_image};

const HEADER_LEN: usize = 64;
const MAGIC_LSB: u32 = 0x08F2_A6B6;

const FMT_UCHAR: u32 = 0;
const FMT_USHORT: u32 = 2;
const CODING_NONE: u32 = 0;

/// VIPS `Type` values whose samples are display RGB/gray (or untyped multiband). Lab, XYZ,
/// CMYK, Fourier and the other colour spaces need conversion and are not decoded.
fn displayable_type(ty: u32) -> bool {
    matches!(ty, 0 | 1 | 15 | 20 | 21)
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    let order = match data.get(..4) {
        Some(m) if Order::Little.u32(m, 0) == Some(MAGIC_LSB) => Order::Little,
        Some(m) if Order::Big.u32(m, 0) == Some(MAGIC_LSB) => Order::Big,
        _ => return Err(DecodeError::Unsupported),
    };
    let field = |at: usize| {
        order
            .u32(data, at)
            .ok_or_else(|| DecodeError::malformed("VIPS header truncated"))
    };
    let (width, height, bands) = (field(4)?, field(8)?, field(12)?);
    let (format, coding, ty) = (field(20)?, field(24)?, field(28)?);
    if coding != CODING_NONE || !displayable_type(ty) {
        return Err(DecodeError::malformed(
            "unsupported VIPS coding or colour space",
        ));
    }
    let sample_bytes: u64 = match format {
        FMT_UCHAR => 1,
        FMT_USHORT => 2,
        _ => return Err(DecodeError::malformed("unsupported VIPS band format")),
    };
    if !matches!(bands, 1 | 3 | 4) {
        return Err(DecodeError::malformed("unsupported VIPS band count"));
    }
    check_dimensions(width, height)?;
    let samples = u64::from(width) * u64::from(height) * u64::from(bands);
    check_alloc(samples)?;
    let need = samples
        .checked_mul(sample_bytes)
        .and_then(|n| n.checked_add(HEADER_LEN as u64))
        .ok_or(DecodeError::TooLarge)?;
    if need > data.len() as u64 {
        return Err(DecodeError::malformed("VIPS pixel data truncated"));
    }
    let body = &data[HEADER_LEN..need as usize];
    let pixels: Vec<u8> = if sample_bytes == 1 {
        body.to_vec()
    } else {
        // 16-bit: keep the high byte of each sample.
        body.as_chunks::<2>()
            .0
            .iter()
            .map(|c| match order {
                Order::Little => c[1],
                Order::Big => c[0],
            })
            .collect()
    };
    interleaved_image(width, height, bands as usize, pixels)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(order: Order, w: u32, h: u32, bands: u32, fmt: u32, coding: u32, ty: u32) -> Vec<u8> {
        let put = |v: u32| match order {
            Order::Little => v.to_le_bytes(),
            Order::Big => v.to_be_bytes(),
        };
        let mut out = Vec::new();
        for v in [MAGIC_LSB, w, h, bands, 8, fmt, coding, ty] {
            out.extend_from_slice(&put(v));
        }
        out.resize(HEADER_LEN, 0);
        out
    }

    #[test]
    fn rgb_uchar_both_byte_orders() {
        for order in [Order::Little, Order::Big] {
            let mut f = header(order, 2, 1, 3, FMT_UCHAR, 0, 15);
            f.extend_from_slice(&[255, 0, 0, 0, 0, 200]);
            let img = decode(&f).unwrap().to_rgb8();
            assert_eq!((img.width(), img.height()), (2, 1));
            assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0]);
            assert_eq!(img.get_pixel(1, 0).0, [0, 0, 200]);
        }
    }

    #[test]
    fn gray_ushort_keeps_the_high_byte() {
        let mut f = header(Order::Little, 2, 1, 1, FMT_USHORT, 0, 1);
        f.extend_from_slice(&[0x34, 0x12, 0xFF, 0xF0]);
        let img = decode(&f).unwrap().to_luma8();
        assert_eq!(img.get_pixel(0, 0).0, [0x12]);
        assert_eq!(img.get_pixel(1, 0).0, [0xF0]);
    }

    #[test]
    fn hostile_headers_are_rejected_without_allocating() {
        assert_eq!(decode(b"not vips"), Err(DecodeError::Unsupported));
        assert!(matches!(
            decode(&MAGIC_LSB.to_le_bytes()),
            Err(DecodeError::Malformed(_))
        ));
        let huge = header(Order::Little, 65_535, 65_535, 4, FMT_UCHAR, 0, 15);
        assert_eq!(decode(&huge), Err(DecodeError::TooLarge));
        let lying = header(Order::Little, 4096, 4096, 3, FMT_UCHAR, 0, 15);
        assert!(matches!(decode(&lying), Err(DecodeError::Malformed(_))));
        let lab = header(Order::Little, 1, 1, 3, FMT_UCHAR, 0, 7);
        assert!(matches!(decode(&lab), Err(DecodeError::Malformed(_))));
    }
}
