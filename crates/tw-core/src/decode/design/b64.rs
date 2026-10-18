//! Lenient, size-capped base64 for XML payloads (XMP, Paint.NET headers).
//!
//! Whitespace and XML character references (`&#xA;`, `&#x0A;`, `&#10;`, `&#13;`...) between
//! the digits are skipped, as XMP serialisers wrap long values that way.

use crate::decode::{DecodeError, Result};

fn value(c: u8) -> Option<u32> {
    Some(match c {
        b'A'..=b'Z' => u32::from(c - b'A'),
        b'a'..=b'z' => u32::from(c - b'a') + 26,
        b'0'..=b'9' => u32::from(c - b'0') + 52,
        b'+' | b'-' => 62,
        b'/' | b'_' => 63,
        _ => return None,
    })
}

/// Decodes `text`, stopping at the first `=` pad. Output beyond `max_out` bytes is
/// `TooLarge` (checked while decoding, never by trusting the input length).
pub fn decode(text: &[u8], max_out: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity((text.len() / 4 * 3).min(max_out).min(1 << 20));
    let mut acc = 0u32;
    let mut bits = 0u32;
    let mut i = 0;
    while let Some(&c) = text.get(i) {
        i += 1;
        match c {
            b' ' | b'\t' | b'\r' | b'\n' => continue,
            b'&' => {
                // Character reference: skip to ';' (bounded so a stray '&' is an error).
                let end = text
                    .get(i..i + 8)
                    .or_else(|| text.get(i..))
                    .and_then(|w| w.iter().position(|&b| b == b';'))
                    .ok_or_else(|| DecodeError::malformed("base64: bad character reference"))?;
                i += end + 1;
                continue;
            }
            b'=' => break,
            _ => {}
        }
        let v = value(c).ok_or_else(|| DecodeError::malformed("base64: invalid digit"))?;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            if out.len() >= max_out {
                return Err(DecodeError::TooLarge);
            }
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    if out.is_empty() {
        return Err(DecodeError::malformed("base64: empty payload"));
    }
    Ok(out)
}

#[cfg(test)]
pub fn encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for ch in data.chunks(3) {
        let n = ch.iter().fold(0u32, |a, &b| a << 8 | u32::from(b)) << (8 * (3 - ch.len()));
        for k in 0..=ch.len() {
            s.push(T[(n >> (18 - 6 * k) & 63) as usize] as char);
        }
        for _ in ch.len()..3 {
            s.push('=');
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_with_entities_and_whitespace() {
        let data: Vec<u8> = (0..=255u8).collect();
        let enc = encode(&data);
        let wrapped: String = enc
            .as_bytes()
            .chunks(76)
            .map(|c| format!("{}&#xA;\n", std::str::from_utf8(c).unwrap()))
            .collect();
        assert_eq!(decode(wrapped.as_bytes(), 1024).unwrap(), data);
        assert_eq!(decode(enc.as_bytes(), 1024).unwrap(), data);
    }

    #[test]
    fn output_cap_and_garbage_are_rejected() {
        let enc = encode(&[7u8; 300]);
        assert_eq!(decode(enc.as_bytes(), 100), Err(DecodeError::TooLarge));
        assert!(matches!(
            decode(b"@@@@", 10),
            Err(DecodeError::Malformed(_))
        ));
        assert!(matches!(
            decode(b"AB&#x", 10),
            Err(DecodeError::Malformed(_))
        ));
    }
}
