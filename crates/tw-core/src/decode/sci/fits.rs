//! FITS (`.fits`, `.fts`): 2880-byte blocks of 80-byte header cards followed by big-endian
//! image data. The first 2-D (or 3-plane) image HDU is stretched linearly between its
//! minimum and maximum to 8 bits; a 3-plane cube becomes RGB.

use image::DynamicImage;

use super::{DecodeError, Result, check_alloc, check_dimensions, flip_rows, pixel_buffer};

const BLOCK: usize = 2880;
const CARD: usize = 80;
/// Cards read per header before giving up (about 370 KiB).
const MAX_CARDS: usize = 4608;
/// HDUs walked while looking for an image.
const MAX_HDUS: usize = 256;
const MAX_AXES: usize = 999;

#[derive(Debug)]
struct Hdu {
    bitpix: i32,
    axes: Vec<u64>,
    bscale: f64,
    bzero: f64,
    is_image: bool,
    data_start: usize,
    /// Start of the next HDU, or `None` when it would lie beyond the address space.
    next: Option<usize>,
}

fn card_value(card: &[u8]) -> Option<&str> {
    if card.get(8..10) != Some(b"= ") {
        return None;
    }
    let text = std::str::from_utf8(&card[10..]).ok()?;
    let text = text.trim_start();
    if let Some(rest) = text.strip_prefix('\'') {
        // String value: up to the closing quote (doubled quotes escape).
        let mut end = None;
        let bytes = rest.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                end = Some(i);
                break;
            }
            i += 1;
        }
        return Some(rest[..end?].trim_end());
    }
    Some(text.split('/').next().unwrap_or("").trim())
}

fn parse_f64(v: &str) -> Option<f64> {
    v.replace(['D', 'd'], "E").parse().ok()
}

fn parse_header(data: &[u8], start: usize, primary: bool) -> Result<Hdu> {
    let first = data
        .get(start..start.saturating_add(CARD))
        .ok_or_else(|| DecodeError::malformed("FITS header truncated"))?;
    let want: &[u8] = if primary { b"SIMPLE  =" } else { b"XTENSION=" };
    if !first.starts_with(want) {
        return Err(DecodeError::malformed(
            "FITS HDU does not start with its keyword",
        ));
    }

    let mut bitpix = None;
    let mut naxis = None;
    let mut axes: Vec<Option<u64>> = Vec::new();
    let (mut bscale, mut bzero) = (1.0, 0.0);
    let (mut pcount, mut gcount) = (0u64, 1u64);
    let mut is_image = primary;
    let mut cards = 0usize;
    let mut ended = false;

    for i in 0..MAX_CARDS {
        let at = start + i * CARD;
        let card = data
            .get(at..at + CARD)
            .ok_or_else(|| DecodeError::malformed("FITS header has no END card"))?;
        cards = i + 1;
        let key = std::str::from_utf8(&card[..8]).unwrap_or("").trim_end();
        if key == "END" {
            ended = true;
            break;
        }
        let Some(value) = card_value(card) else {
            continue;
        };
        match key {
            "XTENSION" => is_image = value == "IMAGE",
            "BITPIX" => bitpix = value.parse::<i32>().ok(),
            "NAXIS" => {
                let n = value.parse::<usize>().ok().filter(|n| *n <= MAX_AXES);
                naxis = n;
                axes = vec![None; n.unwrap_or(0)];
            }
            "BSCALE" => bscale = parse_f64(value).unwrap_or(1.0),
            "BZERO" => bzero = parse_f64(value).unwrap_or(0.0),
            "PCOUNT" => pcount = value.parse().unwrap_or(0),
            "GCOUNT" => gcount = value.parse().unwrap_or(1),
            _ => {
                if let Some(n) = key
                    .strip_prefix("NAXIS")
                    .and_then(|n| n.parse::<usize>().ok())
                    && (1..=axes.len()).contains(&n)
                {
                    axes[n - 1] = value.parse::<u64>().ok();
                }
            }
        }
    }
    if !ended {
        return Err(DecodeError::malformed("FITS header has no END card"));
    }
    let bitpix = bitpix.ok_or_else(|| DecodeError::malformed("FITS BITPIX missing"))?;
    if !matches!(bitpix, 8 | 16 | 32 | 64 | -32 | -64) {
        return Err(DecodeError::malformed("invalid FITS BITPIX"));
    }
    naxis.ok_or_else(|| DecodeError::malformed("FITS NAXIS missing"))?;
    let axes = axes
        .into_iter()
        .collect::<Option<Vec<u64>>>()
        .ok_or_else(|| DecodeError::malformed("FITS NAXISn missing"))?;

    // Data size: |BITPIX|/8 * GCOUNT * (PCOUNT + product of axes), padded to a block.
    let mut elements: Option<u64> = Some(if axes.is_empty() { 0 } else { 1 });
    for a in &axes {
        elements = elements.and_then(|e| e.checked_mul(*a));
    }
    let data_len = elements
        .and_then(|e| e.checked_add(pcount))
        .and_then(|e| e.checked_mul(gcount))
        .and_then(|e| e.checked_mul(u64::from(bitpix.unsigned_abs()) / 8));
    let header_len = cards.div_ceil(BLOCK / CARD) * BLOCK;
    let data_start = start + header_len;
    let next = data_len
        .and_then(|l| l.checked_add(BLOCK as u64 - 1))
        .map(|l| l / BLOCK as u64 * BLOCK as u64)
        .and_then(|l| usize::try_from(l).ok())
        .and_then(|l| data_start.checked_add(l));
    Ok(Hdu {
        bitpix,
        axes,
        bscale,
        bzero,
        is_image,
        data_start,
        next,
    })
}

fn sample(bitpix: i32, b: &[u8]) -> f64 {
    match bitpix {
        8 => f64::from(b[0]),
        16 => f64::from(i16::from_be_bytes([b[0], b[1]])),
        32 => f64::from(i32::from_be_bytes([b[0], b[1], b[2], b[3]])),
        64 => i64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) as f64,
        -32 => f64::from(f32::from_be_bytes([b[0], b[1], b[2], b[3]])),
        _ => f64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
    }
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if !data.starts_with(b"SIMPLE  =") {
        return Err(DecodeError::Unsupported);
    }
    let mut start = 0usize;
    let mut hdu = None;
    for n in 0..MAX_HDUS {
        let h = parse_header(data, start, n == 0)?;
        if h.is_image && h.axes.len() >= 2 {
            hdu = Some(h);
            break;
        }
        match h.next {
            Some(next) if next < data.len() => start = next,
            _ => break,
        }
    }
    let hdu = hdu.ok_or(DecodeError::Unsupported)?;

    let (w, h) = (hdu.axes[0], hdu.axes[1]);
    let (w, h) = match (u32::try_from(w), u32::try_from(h)) {
        (Ok(w), Ok(h)) => (w, h),
        _ => return Err(DecodeError::TooLarge),
    };
    check_dimensions(w, h)?;
    let planes: u64 = if hdu.axes.len() == 3 && hdu.axes[2] == 3 {
        3
    } else {
        1
    };
    let plane_samples = u64::from(w) * u64::from(h);
    let bpp = u64::from(hdu.bitpix.unsigned_abs()) / 8;
    check_alloc(plane_samples * planes)?;
    let needed = plane_samples * planes * bpp;
    let avail = (data.len() - hdu.data_start.min(data.len())) as u64;
    if needed > avail {
        return Err(DecodeError::malformed("FITS data truncated"));
    }
    let bpp = bpp as usize;
    let total = (plane_samples * planes) as usize;
    let raw = &data[hdu.data_start..hdu.data_start + total * bpp];

    let value = |chunk: &[u8]| hdu.bzero + hdu.bscale * sample(hdu.bitpix, chunk);
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for chunk in raw.chunks_exact(bpp) {
        let v = value(chunk);
        if v.is_finite() {
            lo = lo.min(v);
            hi = hi.max(v);
        }
    }
    if !lo.is_finite() {
        (lo, hi) = (0.0, 1.0);
    }
    let range = if hi > lo { hi - lo } else { 1.0 };
    let scale = |chunk: &[u8]| {
        let v = value(chunk);
        if v.is_finite() {
            (((v - lo) / range) * 255.0 + 0.5) as u8
        } else {
            0
        }
    };

    let (w_us, h_us) = (w as usize, h as usize);
    if planes == 3 {
        let mut out = pixel_buffer(w, h, 3)?;
        let plane = w_us * h_us;
        for (i, px) in out.as_chunks_mut::<3>().0.iter_mut().enumerate() {
            for (c, o) in px.iter_mut().enumerate() {
                let at = (c * plane + i) * bpp;
                *o = scale(&raw[at..at + bpp]);
            }
        }
        flip_rows(&mut out, w_us * 3);
        super::rgb_image(w, h, out)
    } else {
        let mut out = pixel_buffer(w, h, 1)?;
        for (o, chunk) in out.iter_mut().zip(raw.chunks_exact(bpp)) {
            *o = scale(chunk);
        }
        flip_rows(&mut out, w_us);
        super::gray_image(w, h, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(text: &str) -> Vec<u8> {
        let mut c = text.as_bytes().to_vec();
        c.resize(CARD, b' ');
        c
    }

    fn hdu(primary: bool, bitpix: i32, axes: &[u64], extra: &[&str], payload: &[u8]) -> Vec<u8> {
        let mut h = Vec::new();
        h.extend(card(if primary {
            "SIMPLE  =                    T"
        } else {
            "XTENSION= 'IMAGE   '"
        }));
        h.extend(card(&format!("BITPIX  = {bitpix:>20}")));
        h.extend(card(&format!("NAXIS   = {:>20}", axes.len())));
        for (i, a) in axes.iter().enumerate() {
            h.extend(card(&format!("NAXIS{}  = {a:>20}", i + 1)));
        }
        if !primary {
            h.extend(card("PCOUNT  =                    0"));
            h.extend(card("GCOUNT  =                    1"));
        }
        for e in extra {
            h.extend(card(e));
        }
        h.extend(card("END"));
        h.resize(h.len().div_ceil(BLOCK) * BLOCK, b' ');
        h.extend_from_slice(payload);
        h.resize(h.len().div_ceil(BLOCK) * BLOCK, 0);
        h
    }

    #[test]
    fn sixteen_bit_with_bzero_is_stretched_and_flipped() {
        // 3x2, rows bottom-up in the file: bottom row 0,100,200 ; top row 300,400,500
        // stored as value - 1000 with BZERO = 1000.
        let vals: [i16; 6] = [-1000, -900, -800, -700, -600, -500];
        let payload: Vec<u8> = vals.iter().flat_map(|v| v.to_be_bytes()).collect();
        let f = hdu(
            true,
            16,
            &[3, 2],
            &["BZERO   =               1000.0"],
            &payload,
        );
        let img = decode(&f).unwrap().to_luma8();
        assert_eq!(img.dimensions(), (3, 2));
        // Bottom file row becomes the bottom image row; min maps to 0, max to 255.
        assert_eq!(img.get_pixel(0, 1).0, [0]);
        assert_eq!(img.get_pixel(2, 0).0, [255]);
        assert_eq!(img.get_pixel(0, 0).0, [153]); // (300-0)/500*255 = 153
    }

    #[test]
    fn three_plane_cube_is_rgb() {
        // 2x1x3 8-bit: R = 255,0 ; G = 0,255 ; B = 0,0.
        let f = hdu(true, 8, &[2, 1, 3], &[], &[255, 0, 0, 255, 0, 0]);
        let img = decode(&f).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (2, 1));
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 255, 0]);
    }

    #[test]
    fn float_image_in_an_extension_after_an_empty_primary() {
        let mut f = hdu(true, 8, &[], &[], &[]);
        let payload: Vec<u8> = [0.0f32, 1.0, 2.0, f32::NAN]
            .iter()
            .flat_map(|v| v.to_be_bytes())
            .collect();
        f.extend(hdu(false, -32, &[2, 2], &[], &payload));
        let img = decode(&f).unwrap().to_luma8();
        assert_eq!(img.dimensions(), (2, 2));
        assert_eq!(img.get_pixel(1, 1).0, [255 / 2 + 1]); // value 1.0 of 0..2, row flipped
        assert_eq!(img.get_pixel(0, 0).0, [255]);
        assert_eq!(img.get_pixel(1, 0).0, [0]); // NaN
    }

    #[test]
    fn hostile_inputs_are_rejected() {
        assert_eq!(decode(b"not fits"), Err(DecodeError::Unsupported));
        assert!(matches!(
            decode(b"SIMPLE  =       T"),
            Err(DecodeError::Malformed(_))
        ));
        // 65535 x 65535 exceeds the pixel budget.
        let f = hdu(true, 8, &[65535, 65535], &[], &[]);
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
        // 20000 x 20000 doubles: header only, the declared data is missing.
        let f = hdu(true, -64, &[20000, 20000], &[], &[]);
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        // RGB cube needing over 512 MiB of samples.
        let f = hdu(true, 8, &[20000, 20000, 3], &[], &[]);
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
        // Dimensions beyond u32.
        let f = hdu(true, 8, &[u64::MAX, 2], &[], &[]);
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
    }
}
