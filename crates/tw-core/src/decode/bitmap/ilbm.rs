//! Amiga IFF ILBM (`.iff`, `.ilbm`, `.lbm`) and the chunky `PBM ` form used by Deluxe Paint
//! PC: BMHD, CMAP, CAMG and BODY; ByteRun1 or uncompressed; 1-8 planes, EHB, HAM6/HAM8,
//! 24/32-plane deep images and the mask plane / transparent colour.

use image::DynamicImage;

use super::{DecodeError, Result, be16, be32, check_alloc, pixel_bytes, rgba_image, unpack_bits};

const CAMG_HAM: u32 = 0x0800;
const CAMG_EHB: u32 = 0x0080;
const MASK_PLANE: u8 = 1;
const MASK_TRANSPARENT_COLOR: u8 = 2;

struct Header {
    width: u32,
    height: u32,
    planes: u8,
    masking: u8,
    compression: u8,
    transparent: u16,
}

fn parse_bmhd(b: &[u8]) -> Result<Header> {
    let bad = || DecodeError::malformed("IFF BMHD truncated");
    if b.len() < 20 {
        return Err(bad());
    }
    Ok(Header {
        width: u32::from(be16(b, 0).ok_or_else(bad)?),
        height: u32::from(be16(b, 2).ok_or_else(bad)?),
        planes: b[8],
        masking: b[9],
        compression: b[10],
        transparent: be16(b, 12).ok_or_else(bad)?,
    })
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if data.get(..4) != Some(b"FORM") {
        return Err(DecodeError::Unsupported);
    }
    let chunky = match data.get(8..12) {
        Some(b"ILBM") => false,
        Some(b"PBM ") => true,
        _ => return Err(DecodeError::Unsupported),
    };
    let form_len = be32(data, 4).unwrap_or(0) as usize;
    let end = form_len.saturating_add(8).min(data.len());

    let mut header = None;
    let mut cmap: &[u8] = &[];
    let mut camg = None;
    let mut body = None;
    let mut pos = 12usize;
    while let (Some(id), Some(size)) = (data.get(pos..pos + 4), be32(data, pos + 4)) {
        let start = pos + 8;
        let stop = start.saturating_add(size as usize).min(end);
        let chunk = data.get(start..stop).unwrap_or(&[]);
        match id {
            b"BMHD" => header = Some(parse_bmhd(chunk)?),
            b"CMAP" => cmap = chunk,
            b"CAMG" => camg = be32(chunk, 0),
            b"BODY" => {
                body = Some(chunk);
                break;
            }
            _ => {}
        }
        pos = start
            .saturating_add(size as usize)
            .saturating_add(size as usize & 1);
        if pos >= end {
            break;
        }
    }
    let hd = header.ok_or_else(|| DecodeError::malformed("IFF without BMHD"))?;
    let body = body.ok_or_else(|| DecodeError::malformed("IFF without BODY"))?;

    if hd.compression > 1 {
        return Err(DecodeError::malformed("unsupported IFF compression"));
    }
    let planes = usize::from(hd.planes);
    if chunky {
        if planes != 8 {
            return Err(DecodeError::malformed("unsupported PBM depth"));
        }
    } else if !matches!(planes, 1..=8 | 24 | 32) {
        return Err(DecodeError::malformed("unsupported ILBM plane count"));
    }
    pixel_bytes(hd.width, hd.height, 4)?;
    let (w, h) = (hd.width as usize, hd.height as usize);

    // Bytes per scan line, summed over all stored planes.
    let has_mask_plane = !chunky && hd.masking == MASK_PLANE;
    let stored_planes = if chunky {
        1
    } else {
        planes + usize::from(has_mask_plane)
    };
    let plane_row = if chunky {
        w + w % 2
    } else {
        w.div_ceil(16) * 2
    };
    let line_len = plane_row * stored_planes;
    let body_len = line_len.checked_mul(h).ok_or(DecodeError::TooLarge)?;
    check_alloc(body_len as u64)?;

    let mut unpacked;
    let raster: &[u8] = if hd.compression == 1 {
        // Two input bytes expand to at most 128 output bytes.
        if body_len as u64 > body.len() as u64 * 64 + 128 {
            return Err(DecodeError::malformed("IFF BODY too short for its size"));
        }
        unpacked = vec![0u8; body_len];
        unpack_bits(body, &mut unpacked)?;
        &unpacked
    } else {
        body.get(..body_len)
            .ok_or_else(|| DecodeError::malformed("IFF BODY truncated"))?
    };

    let ncolors = cmap.len() / 3;
    let ham = !chunky && camg.is_some_and(|c| c & CAMG_HAM != 0) && matches!(planes, 6 | 8);
    let ehb = !chunky
        && !ham
        && planes == 6
        && match camg {
            Some(c) => c & CAMG_EHB != 0,
            None => ncolors == 32,
        };
    let palette = |i: usize| -> [u8; 3] {
        if ncolors == 0 {
            // No CMAP: a grey ramp over the available indices.
            let max = (1usize << planes.min(8)) - 1;
            let v = (i.min(max) * 255 / max.max(1)) as u8;
            return [v, v, v];
        }
        match cmap.get(i * 3..i * 3 + 3) {
            Some(c) => [c[0], c[1], c[2]],
            None => [0, 0, 0],
        }
    };

    let mut out = vec![0u8; w * h * 4];
    let mut value = vec![0u32; w];
    for y in 0..h {
        let line = &raster[y * line_len..(y + 1) * line_len];
        let mut alpha_row: Option<&[u8]> = None;
        value.fill(0);
        if chunky {
            for (v, &b) in value.iter_mut().zip(line) {
                *v = u32::from(b);
            }
        } else {
            for p in 0..planes {
                let row = &line[p * plane_row..(p + 1) * plane_row];
                for (x, v) in value.iter_mut().enumerate() {
                    *v |= u32::from(row[x / 8] >> (7 - x % 8) & 1) << p;
                }
            }
            if has_mask_plane {
                alpha_row = Some(&line[planes * plane_row..(planes + 1) * plane_row]);
            }
        }

        let dst = &mut out[y * w * 4..(y + 1) * w * 4];
        let mut held = palette(0);
        for (x, (px, &v)) in dst.chunks_exact_mut(4).zip(&value).enumerate() {
            let rgb = if planes >= 24 {
                [v as u8, (v >> 8) as u8, (v >> 16) as u8]
            } else if ham {
                let (ctl, val) = if planes == 6 {
                    (v >> 4 & 3, (v & 0xF) * 17)
                } else {
                    (v >> 6 & 3, (v & 0x3F) << 2 | (v & 0x3F) >> 4)
                };
                match ctl {
                    0 => held = palette((v & if planes == 6 { 0xF } else { 0x3F }) as usize),
                    1 => held[2] = val as u8,
                    2 => held[0] = val as u8,
                    _ => held[1] = val as u8,
                }
                held
            } else if ehb && v >= 32 {
                palette(v as usize - 32).map(|c| c >> 1)
            } else {
                palette(v as usize)
            };
            let opaque = match alpha_row {
                Some(m) => m[x / 8] >> (7 - x % 8) & 1 == 1,
                None => {
                    !(hd.masking == MASK_TRANSPARENT_COLOR
                        && !chunky
                        && v == u32::from(hd.transparent))
                }
            };
            px.copy_from_slice(&[rgb[0], rgb[1], rgb[2], if opaque { 255 } else { 0 }]);
        }
    }
    rgba_image(hd.width, hd.height, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = id.to_vec();
        v.extend_from_slice(&(body.len() as u32).to_be_bytes());
        v.extend_from_slice(body);
        if body.len() % 2 == 1 {
            v.push(0);
        }
        v
    }

    fn bmhd(w: u16, h: u16, planes: u8, masking: u8, compression: u8) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&w.to_be_bytes());
        b.extend_from_slice(&h.to_be_bytes());
        b.extend_from_slice(&[0; 4]);
        b.extend_from_slice(&[planes, masking, compression, 0]);
        b.extend_from_slice(&[0, 0, 1, 1, 0, 0, 0, 0]);
        b
    }

    fn form(kind: &[u8; 4], chunks: &[Vec<u8>]) -> Vec<u8> {
        let mut inner = kind.to_vec();
        for c in chunks {
            inner.extend_from_slice(c);
        }
        let mut f = b"FORM".to_vec();
        f.extend_from_slice(&(inner.len() as u32).to_be_bytes());
        f.extend(inner);
        f
    }

    #[test]
    fn indexed_planar_uncompressed() {
        // 4x1, 2 planes: pixel indices 0,1,2,3 -> plane0 = 0101, plane1 = 0011.
        let cmap = [0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255];
        let f = form(
            b"ILBM",
            &[
                chunk(b"BMHD", &bmhd(4, 1, 2, 0, 0)),
                chunk(b"CMAP", &cmap),
                chunk(b"BODY", &[0b0101_0000, 0, 0b0011_0000, 0]),
            ],
        );
        let img = decode(&f).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (4, 1));
        assert_eq!(img.get_pixel(1, 0).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(2, 0).0, [0, 255, 0, 255]);
        assert_eq!(img.get_pixel(3, 0).0, [0, 0, 255, 255]);
    }

    #[test]
    fn byterun1_and_mask_plane() {
        // 16x1, 1 plane + mask plane, each plane row 2 bytes: ByteRun1 literal of 4 bytes.
        let f = form(
            b"ILBM",
            &[
                chunk(b"BMHD", &bmhd(16, 1, 1, 1, 1)),
                chunk(b"CMAP", &[0, 0, 0, 200, 100, 50]),
                chunk(b"BODY", &[3, 0xFF, 0x00, 0xF0, 0x00]),
            ],
        );
        let img = decode(&f).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (16, 1));
        assert_eq!(img.get_pixel(0, 0).0, [200, 100, 50, 255]);
        assert_eq!(img.get_pixel(8, 0).0[3], 0);
        assert_eq!(img.get_pixel(3, 0).0, [200, 100, 50, 255]);
        assert_eq!(img.get_pixel(4, 0).0[3], 0);
        assert_eq!(img.get_pixel(12, 0).0[3], 0);
    }

    #[test]
    fn ehb_halves_the_palette() {
        // 1x1, 6 planes, index 32 -> half-brightness of colour 0.
        let mut body = vec![0u8; 12];
        body[10] = 0x80; // plane 5, bit set
        let mut cmap = vec![0u8; 96];
        cmap[..3].copy_from_slice(&[200, 100, 50]);
        let f = form(
            b"ILBM",
            &[
                chunk(b"BMHD", &bmhd(1, 1, 6, 0, 0)),
                chunk(b"CMAP", &cmap),
                chunk(b"CAMG", &CAMG_EHB.to_be_bytes()),
                chunk(b"BODY", &body),
            ],
        );
        let img = decode(&f).unwrap().to_rgba8();
        assert_eq!(img.get_pixel(0, 0).0, [100, 50, 25, 255]);
    }

    #[test]
    fn ham6_modifies_one_component() {
        // 2x1, 6 planes. Pixel 0: palette index 1 (control 00); pixel 1: control 01 (blue)=0xF.
        // Pixel value bits (plane 0..5): px0 = 0b000001, px1 = 0b01_1111.
        let mut planes = [[0u8; 2]; 6];
        let px = [0b00_0001u8, 0b01_1111];
        for (p, plane) in planes.iter_mut().enumerate() {
            for (x, &v) in px.iter().enumerate() {
                if v >> p & 1 == 1 {
                    plane[0] |= 0x80 >> x;
                }
            }
        }
        let body: Vec<u8> = planes.iter().flatten().copied().collect();
        let f = form(
            b"ILBM",
            &[
                chunk(b"BMHD", &bmhd(2, 1, 6, 0, 0)),
                chunk(b"CMAP", &[0, 0, 0, 10, 20, 30]),
                chunk(b"CAMG", &CAMG_HAM.to_be_bytes()),
                chunk(b"BODY", &body),
            ],
        );
        let img = decode(&f).unwrap().to_rgba8();
        assert_eq!(img.get_pixel(0, 0).0, [10, 20, 30, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [10, 20, 255, 255]);
    }

    #[test]
    fn deep_24_plane_and_pbm() {
        // 1x1, 24 planes: R = 1 (plane 0), G = 2 (plane 9), B = 4 (plane 18).
        let mut body = vec![0u8; 24 * 2];
        body[0] = 0x80;
        body[9 * 2] = 0x80;
        body[18 * 2] = 0x80;
        let f = form(
            b"ILBM",
            &[chunk(b"BMHD", &bmhd(1, 1, 24, 0, 0)), chunk(b"BODY", &body)],
        );
        assert_eq!(
            decode(&f).unwrap().to_rgba8().get_pixel(0, 0).0,
            [1, 2, 4, 255]
        );

        // PBM: 3x1, rows padded to 4 bytes, ByteRun1: run of 4 x index 1.
        let f = form(
            b"PBM ",
            &[
                chunk(b"BMHD", &bmhd(3, 1, 8, 0, 1)),
                chunk(b"CMAP", &[0, 0, 0, 9, 8, 7]),
                chunk(b"BODY", &[(1i16 - 4) as u8, 1]),
            ],
        );
        let img = decode(&f).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (3, 1));
        assert_eq!(img.get_pixel(2, 0).0, [9, 8, 7, 255]);
    }

    #[test]
    fn hostile_inputs() {
        assert_eq!(decode(b"RIFF....").unwrap_err(), DecodeError::Unsupported);
        assert_eq!(
            decode(b"FORM\0\0\0\x04ANIM").unwrap_err(),
            DecodeError::Unsupported
        );
        let trunc = form(b"ILBM", &[chunk(b"BMHD", &bmhd(4, 4, 2, 0, 0))]);
        assert!(matches!(decode(&trunc), Err(DecodeError::Malformed(_))));
        let huge = form(
            b"ILBM",
            &[
                chunk(b"BMHD", &bmhd(65535, 65535, 8, 0, 1)),
                chunk(b"BODY", &[0, 0]),
            ],
        );
        assert_eq!(decode(&huge).unwrap_err(), DecodeError::TooLarge);
        // Fits the budgets but the BODY cannot possibly expand to the declared size.
        let lie = form(
            b"ILBM",
            &[
                chunk(b"BMHD", &bmhd(8000, 8000, 8, 0, 1)),
                chunk(b"BODY", &[0, 0]),
            ],
        );
        assert!(matches!(decode(&lie), Err(DecodeError::Malformed(_))));
        // Chunk size that points far past the end must not loop or panic.
        let mut bad = b"FORM\xFF\xFF\xFF\xFFILBM".to_vec();
        bad.extend_from_slice(b"BMHD\xFF\xFF\xFF\xF0");
        assert!(decode(&bad).is_err());
    }
}
