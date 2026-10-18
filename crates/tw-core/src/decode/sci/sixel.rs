//! DEC Sixel (`.six`, `.sixel`): `DCS P1;P2;P3 q ... ST`. Raster attributes, RGB and HLS
//! colour definitions, repeat introducers, graphics carriage return and newline. The
//! canvas is sized from the raster attributes and the painted extent, within the budgets.

use image::DynamicImage;

use super::{DecodeError, Result, check_alloc, check_dimensions, pixel_buffer, rgba_image};

/// Control string introducers searched for at the start of the file.
const PREFIX_SEARCH: usize = 4096;
const REPEAT_CAP: u64 = 1 << 24;
/// Largest canvas edge (declared or painted); real sixel graphics are far smaller.
const MAX_EDGE: u64 = 32768;

/// VT340 default colour registers (percent RGB).
const DEFAULT_PALETTE: [[u8; 3]; 16] = [
    [0, 0, 0],
    [20, 20, 80],
    [80, 13, 13],
    [20, 80, 20],
    [80, 20, 80],
    [20, 80, 80],
    [80, 80, 20],
    [53, 53, 53],
    [26, 26, 26],
    [33, 33, 60],
    [60, 26, 26],
    [33, 60, 33],
    [60, 33, 60],
    [33, 60, 60],
    [60, 60, 33],
    [80, 80, 80],
];

#[derive(Clone, Copy)]
enum Op {
    Raster(u64, u64),
    Define(usize, [u8; 3]),
    Select(usize),
    Sixel { bits: u8, repeat: u64 },
    CarriageReturn,
    NewLine,
}

fn percent(v: u64) -> u8 {
    (v.min(100) * 255 / 100) as u8
}

/// HLS with DEC hue (0 = blue, 120 = red, 240 = green); H in degrees, L and S in percent.
fn hls_to_rgb(h: u64, l: u64, s: u64) -> [u8; 3] {
    let h = ((h % 360 + 240) % 360) as f32;
    let l = l.min(100) as f32 / 100.0;
    let s = s.min(100) as f32 / 100.0;
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let q = |v: f32| ((v + m) * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
    [q(r), q(g), q(b)]
}

/// Parses `;`-separated numeric parameters starting at `*i`, leaving `*i` on the first
/// byte that is neither a digit nor `;`.
fn params(body: &[u8], i: &mut usize) -> Vec<u64> {
    let mut out = Vec::new();
    let mut cur: Option<u64> = None;
    while let Some(&c) = body.get(*i) {
        match c {
            b'0'..=b'9' => {
                cur = Some(
                    cur.unwrap_or(0)
                        .saturating_mul(10)
                        .saturating_add(u64::from(c - b'0'))
                        .min(u64::from(u32::MAX)),
                );
            }
            b';' => out.push(cur.take().unwrap_or(0)),
            _ => break,
        }
        *i += 1;
        if out.len() > 16 {
            // More parameters than any command defines.
            while matches!(body.get(*i), Some(b'0'..=b'9' | b';')) {
                *i += 1;
            }
            break;
        }
    }
    if let Some(v) = cur {
        out.push(v);
    }
    out
}

fn walk(body: &[u8], mut f: impl FnMut(Op)) {
    let mut i = 0;
    while let Some(&c) = body.get(i) {
        i += 1;
        match c {
            b'"' => {
                let p = params(body, &mut i);
                if let [_, _, w, h, ..] = p[..] {
                    f(Op::Raster(w, h));
                }
            }
            b'#' => {
                let p = params(body, &mut i);
                let idx = (p.first().copied().unwrap_or(0) & 0xFF) as usize;
                if let [_, kind, a, b, c] = p[..] {
                    let rgb = if kind == 1 {
                        hls_to_rgb(a, b, c)
                    } else {
                        [percent(a), percent(b), percent(c)]
                    };
                    f(Op::Define(idx, rgb));
                } else {
                    f(Op::Select(idx));
                }
            }
            b'!' => {
                let p = params(body, &mut i);
                let repeat = p.first().copied().unwrap_or(1).max(1);
                if let Some(&s @ b'?'..=b'~') = body.get(i) {
                    i += 1;
                    f(Op::Sixel {
                        bits: s - b'?',
                        repeat: repeat.min(REPEAT_CAP),
                    });
                }
            }
            b'$' => f(Op::CarriageReturn),
            b'-' => f(Op::NewLine),
            b'?'..=b'~' => f(Op::Sixel {
                bits: c - b'?',
                repeat: 1,
            }),
            _ => {}
        }
    }
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    // DCS introducer: ESC P or the 8-bit C1 form.
    let head = &data[..data.len().min(PREFIX_SEARCH)];
    let dcs = head
        .iter()
        .enumerate()
        .find(|&(i, &b)| b == 0x90 || (b == 0x1B && head.get(i + 1) == Some(&b'P')))
        .ok_or(DecodeError::Unsupported)?;
    let mut i = dcs.0 + if *dcs.1 == 0x90 { 1 } else { 2 };
    let p = params(data, &mut i);
    if data.get(i) != Some(&b'q') {
        return Err(DecodeError::Unsupported);
    }
    let transparent = p.get(1) == Some(&1);
    let mut body = &data[i + 1..];
    // The string ends at ST (ESC \) or the 8-bit ST.
    if let Some(end) = body
        .iter()
        .enumerate()
        .position(|(k, &b)| b == 0x9C || (b == 0x1B && body.get(k + 1) == Some(&b'\\')))
    {
        body = &body[..end];
    }

    // Pass 1: extent.
    let (mut declared_w, mut declared_h) = (0u64, 0u64);
    let (mut x, mut band, mut max_w, mut last_band) = (0u64, 0u64, 0u64, None::<u64>);
    walk(body, |op| match op {
        Op::Raster(w, h) => (declared_w, declared_h) = (w, h),
        Op::Sixel { repeat, .. } => {
            x = x.saturating_add(repeat);
            max_w = max_w.max(x);
            last_band = Some(band);
        }
        Op::CarriageReturn => x = 0,
        Op::NewLine => {
            x = 0;
            band = band.saturating_add(1);
        }
        Op::Define(..) | Op::Select(_) => {}
    });
    let content_h = last_band.map_or(0, |b| (b + 1).saturating_mul(6));
    // Content that stays within one band of the declared height is clipped to it.
    let height = if declared_h > 0 && declared_h.saturating_add(6) > content_h {
        declared_h
    } else {
        content_h.max(declared_h)
    };
    let width = declared_w.max(max_w);
    if width > MAX_EDGE || height > MAX_EDGE {
        return Err(DecodeError::TooLarge);
    }
    let (width, height) = (width as u32, height as u32);
    check_dimensions(width, height)?;
    check_alloc(u64::from(width) * u64::from(height) * 4)?;

    // Pass 2: paint.
    let (w, h) = (width as usize, height as usize);
    let mut out = pixel_buffer(width, height, 4)?;
    if !transparent {
        for px in out.as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
    }
    let mut palette = vec![[0u8, 0, 0]; 256];
    for (slot, p) in palette.iter_mut().zip(DEFAULT_PALETTE) {
        *slot = [
            percent(u64::from(p[0])),
            percent(u64::from(p[1])),
            percent(u64::from(p[2])),
        ];
    }
    let mut color = 0usize;
    let (mut x, mut band) = (0usize, 0usize);
    let mut budget = w * h * 64 + (1 << 20);
    let mut over_budget = false;
    walk(body, |op| match op {
        Op::Define(idx, rgb) => {
            palette[idx] = rgb;
            color = idx;
        }
        Op::Select(idx) => color = idx,
        Op::Sixel { bits, repeat } => {
            let repeat = repeat as usize;
            let run = repeat.min(w.saturating_sub(x));
            let cost = run * bits.count_ones() as usize;
            match budget.checked_sub(cost) {
                Some(b) => budget = b,
                None => over_budget = true,
            }
            if run > 0 && !over_budget {
                let [r, g, b] = palette[color];
                for k in 0..6usize {
                    let y = band * 6 + k;
                    if bits >> k & 1 == 0 || y >= h {
                        continue;
                    }
                    for px in out[(y * w + x) * 4..(y * w + x + run) * 4]
                        .as_chunks_mut::<4>()
                        .0
                    {
                        px.copy_from_slice(&[r, g, b, 255]);
                    }
                }
            }
            x = x.saturating_add(repeat);
        }
        Op::CarriageReturn => x = 0,
        Op::NewLine => {
            x = 0;
            band = band.saturating_add(1);
        }
        Op::Raster(..) => {}
    });
    if over_budget {
        return Err(DecodeError::malformed(
            "sixel paints beyond its pixel budget",
        ));
    }
    rgba_image(width, height, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sixel(p2: u8, body: &str) -> Vec<u8> {
        format!("\x1bP0;{p2};0q{body}\x1b\\").into_bytes()
    }

    #[test]
    fn colours_repeat_and_newline() {
        // Register 1 = pure red (RGB 100,0,0), register 2 = pure green. First band: 3 columns
        // of all six rows red (~ = 63 = 0b111111), a carriage return then green over the
        // first two columns only. Second band: one column of the top pixel in red.
        let f = sixel(0, "\"1;1;3;7#1;2;100;0;0#2;2;0;100;0#1!3~$#2!2~-#1@");
        let img = decode(&f).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (3, 7));
        assert_eq!(img.get_pixel(2, 5).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(0, 0).0, [0, 255, 0, 255]);
        assert_eq!(img.get_pixel(1, 3).0, [0, 255, 0, 255]);
        assert_eq!(img.get_pixel(0, 6).0, [255, 0, 0, 255]);
        // Unpainted pixels are opaque black unless P2 requests transparency.
        assert_eq!(img.get_pixel(1, 6).0, [0, 0, 0, 255]);
        let t = decode(&sixel(1, "\"1;1;3;6#1;2;100;0;0!2~"))
            .unwrap()
            .to_rgba8();
        assert_eq!(t.dimensions(), (3, 6));
        assert_eq!(t.get_pixel(0, 3).0, [255, 0, 0, 255]);
        assert_eq!(t.get_pixel(2, 3).0[3], 0);
    }

    #[test]
    fn hls_blue_at_hue_zero_and_extent_without_raster_attributes() {
        // DEC hue 0 is blue; L=50 S=100 gives saturated blue.
        let f = sixel(0, "#0;1;0;50;100~~");
        let img = decode(&f).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (2, 6));
        assert_eq!(img.get_pixel(1, 5).0, [0, 0, 255, 255]);
    }

    #[test]
    fn hostile_inputs_are_rejected() {
        assert_eq!(decode(b"plain text"), Err(DecodeError::Unsupported));
        assert_eq!(decode(b"\x1bP0;0;0"), Err(DecodeError::Unsupported));
        // Raster attributes declaring 65535 x 65535 and 20000 x 20000.
        assert_eq!(
            decode(&sixel(0, "\"1;1;65535;65535~")),
            Err(DecodeError::TooLarge)
        );
        assert_eq!(
            decode(&sixel(0, "\"1;1;20000;20000~")),
            Err(DecodeError::TooLarge)
        );
        // A repeat introducer cannot declare an enormous canvas.
        assert_eq!(
            decode(&sixel(0, "!99999999999~")),
            Err(DecodeError::TooLarge)
        );
        // Empty graphics.
        assert!(matches!(
            decode(&sixel(0, "")),
            Err(DecodeError::Malformed(_))
        ));
    }
}
