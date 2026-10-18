//! VICAR (`.vicar`): an ASCII `LBLSIZE=` label followed by binary label records and the
//! samples in band-sequential, line-interleaved or pixel-interleaved order. BYTE, HALF,
//! FULL and IEEE REAL/DOUB samples; one band as gray, three bands as RGB, with a linear
//! min/max stretch for anything wider than a byte.

use image::DynamicImage;

use super::{
    DecodeError, Order, Result, check_alloc, check_dimensions, gray_image, pixel_buffer, rgb_image,
};

const MAX_LABEL: usize = 1 << 20;

#[derive(Default)]
struct Label {
    lblsize: usize,
    format: String,
    org: String,
    ty: String,
    intfmt: String,
    realfmt: String,
    nl: u64,
    ns: u64,
    nb: u64,
    nbb: u64,
    nlb: u64,
    recsize: u64,
}

fn parse_label(text: &[u8]) -> Label {
    let mut l = Label {
        nb: 1,
        org: "BSQ".into(),
        ty: "IMAGE".into(),
        intfmt: "LOW".into(),
        realfmt: "VAX".into(),
        ..Label::default()
    };
    let s = String::from_utf8_lossy(text);
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_whitespace() || b[i] == 0 {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && b[i] != b'=' && !b[i].is_ascii_whitespace() && b[i] != 0 {
            i += 1;
        }
        let key = s[start..i].to_ascii_uppercase();
        if b.get(i) != Some(&b'=') {
            continue;
        }
        i += 1;
        let value = if b.get(i) == Some(&b'\'') {
            i += 1;
            let mut v = String::new();
            while i < b.len() {
                if b[i] == b'\'' {
                    if b.get(i + 1) == Some(&b'\'') {
                        v.push('\'');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                v.push(char::from(b[i]));
                i += 1;
            }
            v
        } else {
            let start = i;
            while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != 0 {
                i += 1;
            }
            s[start..i].to_string()
        };
        let num = value.trim().parse::<u64>().unwrap_or(0);
        match key.as_str() {
            "FORMAT" => l.format = value.to_ascii_uppercase(),
            "ORG" => l.org = value.to_ascii_uppercase(),
            "TYPE" => l.ty = value.to_ascii_uppercase(),
            "INTFMT" => l.intfmt = value.to_ascii_uppercase(),
            "REALFMT" => l.realfmt = value.to_ascii_uppercase(),
            "NL" => l.nl = num,
            "NS" => l.ns = num,
            "NB" => l.nb = num,
            "NBB" => l.nbb = num,
            "NLB" => l.nlb = num,
            "RECSIZE" => l.recsize = num,
            _ => {}
        }
    }
    l
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if !data.starts_with(b"LBLSIZE=") {
        return Err(DecodeError::Unsupported);
    }
    let digits: String = data[8..]
        .iter()
        .take(12)
        .take_while(|b| b.is_ascii_digit())
        .map(|b| char::from(*b))
        .collect();
    let lblsize: usize = digits
        .parse()
        .map_err(|_| DecodeError::malformed("VICAR LBLSIZE invalid"))?;
    if lblsize > MAX_LABEL || lblsize > data.len() {
        return Err(DecodeError::malformed("VICAR label truncated"));
    }
    let mut label = parse_label(&data[..lblsize]);
    label.lblsize = lblsize;

    if label.ty != "IMAGE" {
        return Err(DecodeError::Unsupported);
    }
    let (bytes, real) = match label.format.as_str() {
        "BYTE" => (1u64, false),
        "HALF" | "WORD" => (2, false),
        "FULL" | "LONG" => (4, false),
        "REAL" => (4, true),
        "DOUB" => (8, true),
        _ => return Err(DecodeError::Unsupported),
    };
    let order = if real {
        match label.realfmt.as_str() {
            "IEEE" => Order::Big,
            "RIEEE" => Order::Little,
            _ => return Err(DecodeError::Unsupported),
        }
    } else if label.intfmt == "HIGH" {
        Order::Big
    } else {
        Order::Little
    };
    let org = match label.org.as_str() {
        "BSQ" => 0,
        "BIL" => 1,
        "BIP" => 2,
        _ => return Err(DecodeError::Unsupported),
    };
    if !matches!(label.nb, 1 | 3) {
        return Err(DecodeError::Unsupported);
    }
    let (Ok(width), Ok(height)) = (u32::try_from(label.ns), u32::try_from(label.nl)) else {
        return Err(DecodeError::TooLarge);
    };
    check_dimensions(width, height)?;
    let bands = label.nb;
    check_alloc(u64::from(width) * u64::from(height) * bands)?;

    // Record geometry: each line record holds `nbb` prefix bytes then the samples.
    let samples_per_record = if org == 2 { label.ns * bands } else { label.ns };
    let natural = label.nbb.saturating_add(samples_per_record * bytes);
    let recsize = if label.recsize == 0 {
        natural
    } else {
        label.recsize
    };
    if recsize < natural {
        return Err(DecodeError::malformed("VICAR RECSIZE smaller than a line"));
    }
    let records = if org == 2 {
        u64::from(height)
    } else {
        u64::from(height) * bands
    };
    let data_start = (lblsize as u64)
        .checked_add(
            label
                .nlb
                .checked_mul(recsize)
                .ok_or(DecodeError::TooLarge)?,
        )
        .ok_or(DecodeError::TooLarge)?;
    let end = records
        .checked_mul(recsize)
        .and_then(|v| v.checked_add(data_start))
        .ok_or(DecodeError::TooLarge)?;
    if end > data.len() as u64 {
        return Err(DecodeError::malformed("VICAR data truncated"));
    }

    let offset = |band: u64, line: u64, sample: u64| -> usize {
        let (record, index) = match org {
            0 => (band * u64::from(height) + line, sample),
            1 => (line * bands + band, sample),
            _ => (line, sample * bands + band),
        };
        (data_start + record * recsize + label.nbb + index * bytes) as usize
    };
    let value = |at: usize| -> f64 {
        let b = &data[at..at + bytes as usize];
        match (bytes, real) {
            (1, _) => f64::from(b[0]),
            (2, _) => {
                let v = order.u16(b, 0).unwrap_or(0) as i16;
                f64::from(v)
            }
            (4, false) => f64::from(order.u32(b, 0).unwrap_or(0) as i32),
            (4, true) => f64::from(order.f32(b, 0).unwrap_or(0.0)),
            _ => {
                let hi = u64::from(order.u32(b, 0).unwrap_or(0));
                let lo = u64::from(order.u32(b, 4).unwrap_or(0));
                f64::from_bits(if order == Order::Big {
                    hi << 32 | lo
                } else {
                    lo << 32 | hi
                })
            }
        }
    };

    let (w, h) = (u64::from(width), u64::from(height));
    let (mut lo, mut hi) = (0.0, 255.0);
    if bytes > 1 {
        (lo, hi) = (f64::INFINITY, f64::NEG_INFINITY);
        for band in 0..bands {
            for line in 0..h {
                for sample in 0..w {
                    let v = value(offset(band, line, sample));
                    if v.is_finite() {
                        lo = lo.min(v);
                        hi = hi.max(v);
                    }
                }
            }
        }
        if !lo.is_finite() {
            (lo, hi) = (0.0, 1.0);
        }
    }
    let range = if hi > lo { hi - lo } else { 1.0 };
    let scale = |v: f64| -> u8 {
        if v.is_finite() {
            (((v - lo) / range).clamp(0.0, 1.0) * 255.0 + 0.5) as u8
        } else {
            0
        }
    };

    let mut out = pixel_buffer(width, height, bands)?;
    let channels = bands as usize;
    for line in 0..h {
        for sample in 0..w {
            let px = (line * w + sample) as usize * channels;
            for band in 0..bands {
                out[px + band as usize] = scale(value(offset(band, line, sample)));
            }
        }
    }
    if bands == 1 {
        gray_image(width, height, out)
    } else {
        rgb_image(width, height, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LABEL_SIZE: usize = 256;

    fn file(attrs: &str, body: &[u8]) -> Vec<u8> {
        let text = format!("LBLSIZE={LABEL_SIZE:<4} {attrs}");
        let mut f = text.into_bytes();
        assert!(f.len() <= LABEL_SIZE);
        f.resize(LABEL_SIZE, b' ');
        f.extend_from_slice(body);
        f
    }

    #[test]
    fn byte_gray_with_binary_prefix() {
        // 3x2, 2 prefix bytes per record, one binary header line of RECSIZE bytes.
        let mut body = vec![9u8; 5]; // NLB = 1 record of 5 bytes
        body.extend([7, 7, 0, 100, 255]);
        body.extend([7, 7, 50, 60, 70]);
        let f = file(
            "FORMAT='BYTE' TYPE='IMAGE' ORG='BSQ' NL=2 NS=3 NB=1 NBB=2 NLB=1 RECSIZE=5",
            &body,
        );
        let img = decode(&f).unwrap().to_luma8();
        assert_eq!(img.dimensions(), (3, 2));
        assert_eq!(img.as_raw(), &[0, 100, 255, 50, 60, 70]);
    }

    #[test]
    fn half_bip_rgb_big_endian_is_stretched() {
        let mut body = Vec::new();
        for v in [0i16, 1000, 0, 500, 0, 1000] {
            body.extend(v.to_be_bytes());
        }
        let f = file(
            "FORMAT='HALF' TYPE='IMAGE' INTFMT='HIGH' ORG='BIP' NL=1 NS=2 NB=3 NBB=0 NLB=0",
            &body,
        );
        let img = decode(&f).unwrap().to_rgb8();
        assert_eq!(img.get_pixel(0, 0).0, [0, 255, 0]);
        assert_eq!(img.get_pixel(1, 0).0, [128, 0, 255]);
    }

    #[test]
    fn hostile_inputs_are_rejected() {
        assert_eq!(decode(b"NOT VICAR"), Err(DecodeError::Unsupported));
        assert!(matches!(
            decode(b"LBLSIZE=500 NL=1"),
            Err(DecodeError::Malformed(_))
        ));
        let f = file("FORMAT='BYTE' NL=65535 NS=65535 NB=1", &[]);
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
        let f = file("FORMAT='BYTE' NL=20000 NS=20000 NB=3", &[]);
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
        let f = file("FORMAT='BYTE' NL=2000 NS=2000 NB=1", &[1, 2, 3]);
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        let f = file("FORMAT='BYTE' NL=18446744073709551615 NS=2 NB=1", &[]);
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
        // Absurd record size must not overflow the offset arithmetic.
        let f = file(
            "FORMAT='BYTE' NL=4 NS=4 NB=1 RECSIZE=18446744073709551615 NLB=9",
            &[],
        );
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
    }
}
