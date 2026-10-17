//! Lossless metadata removal (FUNCTIONAL_SPEC.md 6.4 "Strip metadata"): EXIF/GPS, XMP,
//! IPTC and comments are dropped; ICC colour profiles are kept. Pixel data is copied
//! byte-for-byte.

/// Returns the stripped file, or an error naming why the input can't be processed.
pub fn strip(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.starts_with(&[0xFF, 0xD8]) {
        strip_jpeg(data)
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        strip_png(data)
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        strip_webp(data)
    } else {
        Err("metadata stripping supports JPEG, PNG and WebP".into())
    }
}

fn strip_jpeg(d: &[u8]) -> Result<Vec<u8>, String> {
    let bad = || "malformed JPEG".to_string();
    let mut out = Vec::with_capacity(d.len());
    out.extend_from_slice(&d[..2]);
    let mut i = 2;
    loop {
        // Skip fill bytes between segments.
        while d.get(i) == Some(&0xFF) && d.get(i + 1) == Some(&0xFF) {
            i += 1;
        }
        let (&ff, &marker) = (d.get(i).ok_or_else(bad)?, d.get(i + 1).ok_or_else(bad)?);
        if ff != 0xFF {
            return Err(bad());
        }
        if marker == 0xDA {
            // Start of scan: everything after is entropy-coded data + trailer.
            out.extend_from_slice(&d[i..]);
            return Ok(out);
        }
        if (0xD0..=0xD7).contains(&marker) || marker == 0x01 {
            out.extend_from_slice(&d[i..i + 2]);
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes([
            *d.get(i + 2).ok_or_else(bad)?,
            *d.get(i + 3).ok_or_else(bad)?,
        ]) as usize;
        let end = i
            .checked_add(2 + len)
            .filter(|&e| e <= d.len() && len >= 2)
            .ok_or_else(bad)?;
        let payload = &d[i + 4..end];
        let drop = match marker {
            0xE1 => true,                                   // APP1: Exif, XMP
            0xED => true,                                   // APP13: Photoshop IRB / IPTC
            0xFE => true,                                   // COM
            0xE2 => !payload.starts_with(b"ICC_PROFILE\0"), // keep ICC, drop FlashPix/MPF
            0xE3..=0xEC | 0xEF => true,                     // other vendor APPn
            _ => false,                                     // APP0 JFIF, APP14 Adobe, tables, SOF
        };
        if !drop {
            out.extend_from_slice(&d[i..end]);
        }
        i = end;
    }
}

fn strip_png(d: &[u8]) -> Result<Vec<u8>, String> {
    let bad = || "malformed PNG".to_string();
    let mut out = d[..8].to_vec();
    let mut i = 8;
    while i < d.len() {
        let len = u32::from_be_bytes(d.get(i..i + 4).ok_or_else(bad)?.try_into().unwrap()) as usize;
        let end = i
            .checked_add(12 + len)
            .filter(|&e| e <= d.len())
            .ok_or_else(bad)?;
        let ty = &d[i + 4..i + 8];
        if !matches!(ty, b"eXIf" | b"tEXt" | b"zTXt" | b"iTXt" | b"tIME") {
            out.extend_from_slice(&d[i..end]);
        }
        i = end;
        if ty == b"IEND" {
            break;
        }
    }
    Ok(out)
}

fn strip_webp(d: &[u8]) -> Result<Vec<u8>, String> {
    let bad = || "malformed WebP".to_string();
    let mut chunks = Vec::new();
    let mut i = 12;
    while i + 8 <= d.len() {
        let len = u32::from_le_bytes(d[i + 4..i + 8].try_into().unwrap()) as usize;
        let padded = len + (len & 1);
        let end = i
            .checked_add(8 + padded)
            .filter(|&e| e <= d.len())
            .ok_or_else(bad)?;
        let ty: [u8; 4] = d[i..i + 4].try_into().unwrap();
        if &ty != b"EXIF" && &ty != b"XMP " {
            let mut c = d[i..end].to_vec();
            if &ty == b"VP8X" && c.len() > 8 {
                c[8] &= !(0x08 | 0x04); // clear EXIF and XMP presence flags
            }
            chunks.push(c);
        }
        i = end;
    }
    let body: usize = chunks.iter().map(Vec::len).sum::<usize>() + 4;
    let mut out = Vec::with_capacity(body + 8);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&u32::try_from(body).map_err(|_| bad())?.to_le_bytes());
    out.extend_from_slice(b"WEBP");
    for c in chunks {
        out.extend_from_slice(&c);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(marker: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0xFF, marker];
        v.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn jpeg_drops_exif_and_comments_keeps_icc_and_scan() {
        let mut j = vec![0xFF, 0xD8];
        j.extend(seg(0xE0, b"JFIF\0\x01\x02"));
        j.extend(seg(0xE1, b"Exif\0\0GPSDATA"));
        j.extend(seg(0xE2, b"ICC_PROFILE\0\x01\x01prof"));
        j.extend(seg(0xFE, b"comment"));
        j.extend(seg(0xDB, &[0u8; 65]));
        j.extend([0xFF, 0xDA, 0, 2, 0x12, 0x34, 0xFF, 0xD9]);
        let out = strip(&j).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(!s.contains("GPSDATA") && !s.contains("comment"));
        assert!(s.contains("ICC_PROFILE") && s.contains("JFIF"));
        assert!(out.ends_with(&[0xFF, 0xDA, 0, 2, 0x12, 0x34, 0xFF, 0xD9]));
    }

    #[test]
    fn png_drops_text_and_exif_chunks() {
        let mut png = Vec::new();
        image::RgbaImage::new(2, 2)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        // Insert a tEXt chunk after IHDR (8 + 25 bytes).
        let mut with_text = png[..33].to_vec();
        with_text.extend_from_slice(&6u32.to_be_bytes());
        with_text.extend_from_slice(b"tEXtsecret");
        with_text.extend_from_slice(&[0, 0, 0, 0]);
        with_text.extend_from_slice(&png[33..]);
        let out = strip(&with_text).unwrap();
        assert_eq!(out, png);
        assert!(image::load_from_memory(&out).is_ok());
    }

    #[test]
    fn webp_drops_exif_chunk_and_flag() {
        let mut w = b"RIFF\0\0\0\0WEBP".to_vec();
        w.extend_from_slice(b"VP8X");
        w.extend_from_slice(&10u32.to_le_bytes());
        w.extend_from_slice(&[0x08, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        w.extend_from_slice(b"EXIF");
        w.extend_from_slice(&3u32.to_le_bytes());
        w.extend_from_slice(b"GPS\0");
        let out = strip(&w).unwrap();
        assert_eq!(&out[12..16], b"VP8X");
        assert_eq!(out[20], 0, "EXIF flag cleared");
        assert!(!out.windows(4).any(|x| x == b"EXIF"));
        assert_eq!(
            u32::from_le_bytes(out[4..8].try_into().unwrap()) as usize,
            out.len() - 8
        );
    }

    #[test]
    fn rejects_truncated_and_unknown() {
        assert!(strip(&[0xFF, 0xD8, 0xFF, 0xE1, 0xFF, 0xFF]).is_err());
        assert!(strip(b"GIF89a").is_err());
    }
}
