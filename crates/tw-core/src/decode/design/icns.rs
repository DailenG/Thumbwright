//! Apple icon images (`.icns`): big-endian `icns` + total length, then `(OSType, length)`
//! entries (length includes the 8-byte entry header).
//!
//! PNG payloads (`ic07`..`ic14`, `icp4`..`icp6`) are decoded with the PNG codec, largest
//! first. Otherwise the classic planar RLE icons (`is32`/`il32`/`ih32`/`it32`) are decoded
//! with their 8-bit masks (`s8mk`/`l8mk`/`h8mk`/`t8mk`). JPEG 2000 payloads are skipped.

use image::{DynamicImage, RgbaImage};

use crate::decode::{DecodeError, Result, check_dimensions, decode_bytes};

const MAX_ENTRIES: usize = 256;
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";
const PNG_TYPES: &[&[u8; 4]] = &[
    b"ic07", b"ic08", b"ic09", b"ic10", b"ic11", b"ic12", b"ic13", b"ic14", b"icp4", b"icp5",
    b"icp6",
];
/// (colour type, mask type, edge).
const CLASSIC: &[(&[u8; 4], &[u8; 4], u32)] = &[
    (b"it32", b"t8mk", 128),
    (b"ih32", b"h8mk", 48),
    (b"il32", b"l8mk", 32),
    (b"is32", b"s8mk", 16),
];

fn be32(d: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        d.get(i..i.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn entries(data: &[u8]) -> Result<Vec<([u8; 4], &[u8])>> {
    if data.get(..4) != Some(b"icns") {
        return Err(DecodeError::Unsupported);
    }
    let declared = be32(data, 4).ok_or_else(|| DecodeError::malformed("icns: short header"))?;
    // A lying total length is clamped to the bytes present.
    let end = (declared as usize).clamp(8, data.len());
    let mut out = Vec::new();
    let mut pos = 8usize;
    while pos + 8 <= end && out.len() < MAX_ENTRIES {
        let ty: [u8; 4] = data[pos..pos + 4].try_into().unwrap_or_default();
        let len = be32(data, pos + 4).unwrap_or(0) as usize;
        if len < 8 {
            return Err(DecodeError::malformed("icns: bad entry length"));
        }
        let Some(next) = pos.checked_add(len).filter(|&n| n <= end) else {
            break; // truncated final entry: keep what is complete
        };
        out.push((ty, &data[pos + 8..next]));
        pos = next;
    }
    Ok(out)
}

fn png_area(payload: &[u8]) -> Option<u64> {
    if payload.get(..8) != Some(PNG_MAGIC) {
        return None;
    }
    Some(u64::from(be32(payload, 16)?) * u64::from(be32(payload, 20)?))
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    let entries = entries(data)?;

    let mut pngs: Vec<(u64, &[u8])> = entries
        .iter()
        .filter(|(ty, _)| PNG_TYPES.contains(&ty))
        .filter_map(|(_, p)| Some((png_area(p)?, *p)))
        .collect();
    pngs.sort_by_key(|&(area, _)| std::cmp::Reverse(area));
    let mut last = DecodeError::Unsupported;
    for (_, payload) in pngs {
        match decode_bytes(payload) {
            Ok(img) => return Ok(img),
            Err(e) if e.falls_through() => last = e,
            Err(e) => return Err(e),
        }
    }

    let find = |ty: &[u8; 4]| entries.iter().find(|(t, _)| t == ty).map(|(_, p)| *p);
    for (color, mask, edge) in CLASSIC {
        let Some(payload) = find(color) else { continue };
        match classic(payload, find(mask), *edge, *color == b"it32") {
            Ok(img) => return Ok(img),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// One RLE-compressed plane of `n` bytes starting at `data[pos..]`: a control byte below
/// 0x80 copies `c + 1` literals, otherwise repeats the next byte `c - 0x80 + 3` times.
fn rle_plane(data: &[u8], pos: &mut usize, n: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let c = *data
            .get(*pos)
            .ok_or_else(|| DecodeError::malformed("icns: truncated RLE data"))?;
        *pos += 1;
        if c < 0x80 {
            let count = usize::from(c) + 1;
            let lit = data
                .get(*pos..*pos + count)
                .ok_or_else(|| DecodeError::malformed("icns: truncated RLE literal"))?;
            *pos += count;
            out.extend_from_slice(lit);
        } else {
            let count = usize::from(c - 0x80) + 3;
            let v = *data
                .get(*pos)
                .ok_or_else(|| DecodeError::malformed("icns: truncated RLE run"))?;
            *pos += 1;
            out.resize(out.len() + count, v);
        }
    }
    out.truncate(n);
    Ok(out)
}

fn classic(
    payload: &[u8],
    mask: Option<&[u8]>,
    edge: u32,
    zero_prefix: bool,
) -> Result<DynamicImage> {
    check_dimensions(edge, edge)?;
    let n = (edge * edge) as usize;
    // `it32` carries four zero bytes before the planes.
    let mut pos = if zero_prefix && payload.get(..4) == Some(&[0, 0, 0, 0]) {
        4
    } else {
        0
    };
    let r = rle_plane(payload, &mut pos, n)?;
    let g = rle_plane(payload, &mut pos, n)?;
    let b = rle_plane(payload, &mut pos, n)?;
    let mask = mask.filter(|m| m.len() >= n);
    let mut px = Vec::with_capacity(n * 4);
    for i in 0..n {
        px.extend_from_slice(&[r[i], g[i], b[i], mask.map_or(255, |m| m[i])]);
    }
    RgbaImage::from_raw(edge, edge, px)
        .map(DynamicImage::ImageRgba8)
        .ok_or_else(|| DecodeError::malformed("icns: pixel buffer"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::design::testutil;

    fn entry(ty: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut e = ty.to_vec();
        e.extend((payload.len() as u32 + 8).to_be_bytes());
        e.extend(payload);
        e
    }

    fn icns(entries: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = entries.concat();
        let mut f = b"icns".to_vec();
        f.extend((body.len() as u32 + 8).to_be_bytes());
        f.extend(body);
        f
    }

    /// Run-length encodes a plane of one repeated value (runs of at most 130).
    fn rle_const(v: u8, mut n: usize) -> Vec<u8> {
        let mut out = Vec::new();
        while n > 0 {
            let run = n.min(130);
            if run >= 3 {
                out.extend([0x80 + (run - 3) as u8, v]);
            } else {
                out.push(run as u8 - 1);
                out.extend(std::iter::repeat_n(v, run));
            }
            n -= run;
        }
        out
    }

    #[test]
    fn prefers_the_largest_png_entry() {
        let f = icns(&[
            entry(b"icp4", &testutil::png(16, 16, [255, 0, 0, 255])),
            entry(b"ic08", &testutil::png(64, 64, [0, 255, 0, 255])),
            entry(b"ic07", &testutil::png(32, 32, [0, 0, 255, 255])),
        ]);
        let img = decode(&f).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (64, 64));
        assert_eq!(img.get_pixel(10, 10).0, [0, 255, 0, 255]);
    }

    #[test]
    fn classic_rle_icon_with_mask() {
        let mut planes = rle_const(200, 32 * 32);
        planes.extend(rle_const(100, 32 * 32));
        planes.extend(rle_const(50, 32 * 32));
        let mut mask = vec![255u8; 1024];
        mask[0] = 0;
        let f = icns(&[entry(b"il32", &planes), entry(b"l8mk", &mask)]);
        let img = decode(&f).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (32, 32));
        assert_eq!(img.get_pixel(5, 5).0, [200, 100, 50, 255]);
        assert_eq!(img.get_pixel(0, 0).0[3], 0);
    }

    #[test]
    fn classic_it32_zero_prefix_and_literals() {
        let n = 128 * 128;
        let mut p = vec![0, 0, 0, 0];
        for v in [1u8, 2, 3] {
            p.extend(rle_const(v, n));
        }
        let img = decode(&icns(&[entry(b"it32", &p)])).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (128, 128));
        assert_eq!(img.get_pixel(127, 127).0, [1, 2, 3, 255]);
        // Literal-run control byte.
        let mut lit = vec![15u8];
        lit.extend(0..16u8);
        let mut p = lit.clone();
        p.extend(rle_const(0, 240));
        p.extend(rle_const(7, 256));
        p.extend(rle_const(9, 256));
        let img = decode(&icns(&[entry(b"is32", &p)])).unwrap().to_rgba8();
        assert_eq!(img.get_pixel(3, 0).0, [3, 7, 9, 255]);
    }

    #[test]
    fn unusable_payloads_and_hostile_input() {
        // JPEG 2000 payload is skipped.
        let f = icns(&[entry(b"ic07", b"\0\0\0\x0cjP  \r\n\x87\n")]);
        assert_eq!(decode(&f), Err(DecodeError::Unsupported));
        assert_eq!(
            decode(b"icns"),
            Err(DecodeError::Malformed("icns: short header".into()))
        );
        assert_eq!(decode(b"junk"), Err(DecodeError::Unsupported));
        // Entry length below the header size.
        let mut bad = b"icns\0\0\0\x10ic07\0\0\0\x02".to_vec();
        assert!(matches!(decode(&bad), Err(DecodeError::Malformed(_))));
        // Entry longer than the file: ignored, nothing usable.
        bad = b"icns\0\0\xff\xffic07\0\xff\xff\xffxx".to_vec();
        assert_eq!(decode(&bad), Err(DecodeError::Unsupported));
        // Truncated RLE plane.
        let f = icns(&[entry(b"is32", &[0x80 + 100, 1])]);
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        // PNG header declaring 4G x 4G pixels.
        let mut png = testutil::png(2, 2, [0; 4]);
        png[16..24].copy_from_slice(&[0xFF; 8]);
        assert_eq!(
            decode(&icns(&[entry(b"ic07", &png)])),
            Err(DecodeError::TooLarge)
        );
    }
}
