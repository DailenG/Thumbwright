//! MATLAB level 5 MAT-files (`.mat`): the first image-like numeric array, stored plain or
//! inside a zlib `miCOMPRESSED` element. 2-D arrays become gray images, 3-D arrays with
//! 3 or 4 planes RGB. `uint8` arrays are used as is; other numeric classes are stretched
//! linearly between their minimum and maximum. Arrays are column-major.

use image::DynamicImage;
use miniz_oxide::inflate::decompress_to_vec_zlib_with_limit;

use super::{
    DecodeError, Order, Result, check_alloc, check_dimensions, gray_image, pixel_buffer, rgb_image,
    slice,
};
use crate::limits;

const HEADER_LEN: usize = 128;
/// Top-level variables examined.
const MAX_VARIABLES: usize = 16;
const MI_INT8: u32 = 1;
const MI_UINT8: u32 = 2;
const MI_INT16: u32 = 3;
const MI_UINT16: u32 = 4;
const MI_INT32: u32 = 5;
const MI_UINT32: u32 = 6;
const MI_SINGLE: u32 = 7;
const MI_DOUBLE: u32 = 9;
const MI_INT64: u32 = 12;
const MI_UINT64: u32 = 13;
const MI_MATRIX: u32 = 14;
const MI_COMPRESSED: u32 = 15;
const MX_UINT8_CLASS: u8 = 9;

/// One data element: type, payload, offset of the next element.
fn element(buf: &[u8], pos: usize, order: Order) -> Option<(u32, &[u8], usize)> {
    let first = order.u32(buf, pos)?;
    // Small element format: the upper half of the first word holds the byte count.
    if first >> 16 != 0 {
        let (ty, len) = (first & 0xFFFF, (first >> 16) as usize);
        if len > 4 {
            return None;
        }
        return Some((ty, slice(buf, pos + 4, len)?, pos + 8));
    }
    let len = order.u32(buf, pos + 4)? as usize;
    let data = slice(buf, pos + 8, len)?;
    let padded = if first == MI_COMPRESSED {
        len
    } else {
        len.checked_add(7)? / 8 * 8
    };
    Some((first, data, (pos + 8).checked_add(padded)?))
}

fn sample_size(ty: u32) -> Option<usize> {
    Some(match ty {
        MI_INT8 | MI_UINT8 => 1,
        MI_INT16 | MI_UINT16 => 2,
        MI_INT32 | MI_UINT32 | MI_SINGLE => 4,
        MI_DOUBLE | MI_INT64 | MI_UINT64 => 8,
        _ => return None,
    })
}

fn sample(ty: u32, b: &[u8], order: Order) -> f64 {
    let u32_at = |i| order.u32(b, i).unwrap_or(0);
    match ty {
        MI_INT8 => f64::from(b[0] as i8),
        MI_UINT8 => f64::from(b[0]),
        MI_INT16 => f64::from(order.u16(b, 0).unwrap_or(0) as i16),
        MI_UINT16 => f64::from(order.u16(b, 0).unwrap_or(0)),
        MI_INT32 => f64::from(u32_at(0) as i32),
        MI_UINT32 => f64::from(u32_at(0)),
        MI_SINGLE => f64::from(f32::from_bits(u32_at(0))),
        _ => {
            let (hi, lo) = (u64::from(u32_at(0)), u64::from(u32_at(4)));
            let bits = if order == Order::Big {
                hi << 32 | lo
            } else {
                lo << 32 | hi
            };
            match ty {
                MI_DOUBLE => f64::from_bits(bits),
                MI_INT64 => bits as i64 as f64,
                _ => bits as f64,
            }
        }
    }
}

struct Candidate<'a> {
    width: u32,
    height: u32,
    depth: usize,
    class: u8,
    data_type: u32,
    data: &'a [u8],
}

/// Parses the payload of an `miMATRIX` element; `None` when it is not a usable numeric array.
fn parse_matrix(m: &[u8], order: Order) -> Result<Option<Candidate<'_>>> {
    let Some((flags_ty, flags, pos)) = element(m, 0, order) else {
        return Ok(None);
    };
    let Some(flags) = order.u32(flags, 0).filter(|_| flags_ty == MI_UINT32) else {
        return Ok(None);
    };
    let class = (flags & 0xFF) as u8;
    let complex = flags & 0x0800 != 0;
    // Numeric classes: double, single, int8, uint8, int16, uint16, int32, uint32.
    if complex || !(6..=13).contains(&class) {
        return Ok(None);
    }
    let Some((dims_ty, dims, pos)) = element(m, pos, order) else {
        return Ok(None);
    };
    if dims_ty != MI_INT32 || !matches!(dims.len() / 4, 2 | 3) {
        return Ok(None);
    }
    let dim = |i: usize| order.u32(dims, i * 4).unwrap_or(0) as i32;
    let (rows, cols, depth) = (dim(0), dim(1), if dims.len() == 12 { dim(2) } else { 1 });
    if rows < 1 || cols < 1 || depth < 1 {
        return Ok(None);
    }
    let Some((_, _, pos)) = element(m, pos, order) else {
        return Ok(None); // name
    };
    let Some((data_type, data, _)) = element(m, pos, order) else {
        return Ok(None);
    };
    let Some(size) = sample_size(data_type) else {
        return Ok(None);
    };
    let (width, height, depth) = (cols as u32, rows as u32, depth as usize);
    check_dimensions(width, height)?;
    let plane = u64::from(width) * u64::from(height);
    let planes = if depth >= 3 { 3u64 } else { 1 };
    check_alloc(plane * planes)?;
    let needed = plane * planes * size as u64;
    if needed > data.len() as u64 {
        return Err(DecodeError::malformed("MAT array data truncated"));
    }
    Ok(Some(Candidate {
        width,
        height,
        depth,
        class,
        data_type,
        data,
    }))
}

fn convert(c: &Candidate<'_>, order: Order) -> Result<DynamicImage> {
    let size = sample_size(c.data_type).ok_or(DecodeError::Unsupported)?;
    let (w, h) = (c.width as usize, c.height as usize);
    let planes = if c.depth >= 3 { 3 } else { 1 };
    let plane = w * h;
    if c.data.len() < plane * planes * size {
        return Err(DecodeError::malformed("MAT array data truncated"));
    }
    let value = |k: usize, r: usize, col: usize| -> f64 {
        let at = (k * plane + col * h + r) * size;
        sample(c.data_type, &c.data[at..at + size], order)
    };
    let (mut lo, mut hi) = (0.0, 255.0);
    if c.class != MX_UINT8_CLASS {
        (lo, hi) = (f64::INFINITY, f64::NEG_INFINITY);
        for i in 0..plane * planes {
            let v = sample(c.data_type, &c.data[i * size..(i + 1) * size], order);
            if v.is_finite() {
                lo = lo.min(v);
                hi = hi.max(v);
            }
        }
        if !lo.is_finite() {
            (lo, hi) = (0.0, 1.0);
        }
    }
    let range = if hi > lo { hi - lo } else { 1.0 };
    let to8 = |v: f64| -> u8 {
        if v.is_finite() {
            (((v - lo) / range).clamp(0.0, 1.0) * 255.0 + 0.5) as u8
        } else {
            0
        }
    };
    let mut out = pixel_buffer(c.width, c.height, planes as u64)?;
    for r in 0..h {
        for col in 0..w {
            for k in 0..planes {
                out[(r * w + col) * planes + k] = to8(value(k, r, col));
            }
        }
    }
    if planes == 1 {
        gray_image(c.width, c.height, out)
    } else {
        rgb_image(c.width, c.height, out)
    }
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if !data.starts_with(b"MATLAB 5.0") {
        return Err(DecodeError::Unsupported);
    }
    if data.len() < HEADER_LEN {
        return Err(DecodeError::malformed("MAT header truncated"));
    }
    let order = match &data[126..128] {
        b"IM" => Order::Little,
        b"MI" => Order::Big,
        _ => return Err(DecodeError::malformed("MAT endian indicator invalid")),
    };

    let mut pos = HEADER_LEN;
    let mut fallback: Option<DynamicImage> = None;
    for _ in 0..MAX_VARIABLES {
        let Some((ty, payload, next)) = element(data, pos, order) else {
            break;
        };
        pos = next;
        let inflated;
        let matrix = match ty {
            MI_MATRIX => payload,
            MI_COMPRESSED => {
                inflated =
                    decompress_to_vec_zlib_with_limit(payload, limits::DECODE_ALLOC_BYTES as usize)
                        .map_err(|_| {
                            DecodeError::malformed("MAT compressed element invalid or too large")
                        })?;
                match element(&inflated, 0, order) {
                    Some((MI_MATRIX, inner, _)) => inner,
                    _ => continue,
                }
            }
            _ => continue,
        };
        let Some(cand) = parse_matrix(matrix, order)? else {
            continue;
        };
        if cand.width >= 8 && cand.height >= 8 {
            return convert(&cand, order);
        }
        if fallback.is_none() && cand.width * cand.height >= 4 {
            fallback = Some(convert(&cand, order)?);
        }
    }
    fallback.ok_or(DecodeError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(order: Order) -> Vec<u8> {
        let mut h = b"MATLAB 5.0 MAT-file, test".to_vec();
        h.resize(124, b' ');
        h.extend(match order {
            Order::Little => [0x00, 0x01, b'I', b'M'],
            Order::Big => [0x01, 0x00, b'M', b'I'],
        });
        h
    }

    fn put32(order: Order, v: u32) -> [u8; 4] {
        match order {
            Order::Little => v.to_le_bytes(),
            Order::Big => v.to_be_bytes(),
        }
    }

    fn el(order: Order, ty: u32, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend(put32(order, ty));
        out.extend(put32(order, data.len() as u32));
        out.extend(data);
        while !out.len().is_multiple_of(8) {
            out.push(0);
        }
        out
    }

    fn matrix(order: Order, class: u8, dims: &[i32], data_type: u32, data: &[u8]) -> Vec<u8> {
        let mut body = el(
            order,
            MI_UINT32,
            &[put32(order, u32::from(class)), [0; 4]].concat(),
        );
        let dim_bytes: Vec<u8> = dims.iter().flat_map(|d| put32(order, *d as u32)).collect();
        body.extend(el(order, MI_INT32, &dim_bytes));
        body.extend(el(order, MI_INT8, b"img"));
        body.extend(el(order, data_type, data));
        el(order, MI_MATRIX, &body)
    }

    #[test]
    fn plain_uint8_is_column_major_gray() {
        // 2 rows x 3 columns, column-major: (r0c0, r1c0, r0c1, r1c1, r0c2, r1c2).
        let mut f = header(Order::Little);
        f.extend(matrix(
            Order::Little,
            MX_UINT8_CLASS,
            &[2, 3],
            MI_UINT8,
            &[10, 20, 30, 40, 50, 60],
        ));
        let img = decode(&f).unwrap().to_luma8();
        assert_eq!(img.dimensions(), (3, 2));
        assert_eq!(img.get_pixel(1, 0).0, [30]);
        assert_eq!(img.get_pixel(2, 1).0, [60]);
    }

    #[test]
    fn compressed_big_endian_rgb_and_double_stretch() {
        // 8x8 RGB: red plane 255, green plane 0, blue plane 0 -> red.
        let order = Order::Big;
        let mut planes = vec![255u8; 64];
        planes.extend(vec![0u8; 128]);
        let m = matrix(order, MX_UINT8_CLASS, &[8, 8, 3], MI_UINT8, &planes);
        let z = miniz_oxide::deflate::compress_to_vec_zlib(&m, 6);
        let mut f = header(order);
        f.extend(el(order, MI_COMPRESSED, &z)[..8 + z.len()].to_vec());
        let img = decode(&f).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (8, 8));
        assert_eq!(img.get_pixel(3, 4).0, [255, 0, 0]);

        // 2x2 doubles 0..3 stretched to 0..255 (column-major: 0, 2, 1, 3).
        let doubles: Vec<u8> = [0.0f64, 2.0, 1.0, 3.0]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let mut f = header(Order::Little);
        f.extend(matrix(Order::Little, 6, &[2, 2], MI_DOUBLE, &doubles));
        let img = decode(&f).unwrap().to_luma8();
        assert_eq!(img.as_raw(), &[0, 85, 170, 255]);
    }

    #[test]
    fn hostile_inputs_are_rejected() {
        assert_eq!(decode(b"PK\x03\x04"), Err(DecodeError::Unsupported));
        assert!(matches!(
            decode(b"MATLAB 5.0 MAT-file"),
            Err(DecodeError::Malformed(_))
        ));
        // 65535 x 65535 uint8 with a few bytes of data.
        let mut f = header(Order::Little);
        f.extend(matrix(
            Order::Little,
            MX_UINT8_CLASS,
            &[65535, 65535],
            MI_UINT8,
            &[1, 2, 3, 4],
        ));
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
        // 20000 x 20000 x 3 declared, data missing.
        let mut f = header(Order::Little);
        f.extend(matrix(
            Order::Little,
            MX_UINT8_CLASS,
            &[20000, 20000, 3],
            MI_UINT8,
            &[1],
        ));
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
        // Plausible dimensions, data missing.
        let mut f = header(Order::Little);
        f.extend(matrix(
            Order::Little,
            MX_UINT8_CLASS,
            &[500, 500],
            MI_UINT8,
            &[1, 2, 3],
        ));
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        // Compressed element inflating beyond the budget limit is refused (invalid stream).
        let mut f = header(Order::Little);
        f.extend(el(Order::Little, MI_COMPRESSED, &[0x78, 0x9C, 0xFF, 0xFF]));
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
    }
}
