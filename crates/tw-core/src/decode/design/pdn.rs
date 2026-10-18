//! Paint.NET (`.pdn`): `PDN3`, a 3-byte little-endian header length, then a UTF-8 XML
//! header (`<pdnImage ...><custom><thumb png="BASE64" /></custom>...`) before the
//! gzip-compressed layer data. The thumbnail is the base64 PNG in the `thumb` element.

use image::DynamicImage;
use memchr::memmem;

use super::b64;
use crate::decode::{DecodeError, Result, decode_bytes};

pub fn extract(data: &[u8]) -> Result<DynamicImage> {
    if data.get(..4) != Some(b"PDN3") {
        return Err(DecodeError::Unsupported);
    }
    let len = data
        .get(4..7)
        .map(|b| usize::from(b[0]) | usize::from(b[1]) << 8 | usize::from(b[2]) << 16)
        .ok_or_else(|| DecodeError::malformed("pdn: truncated header"))?;
    // The header is at most 16 MiB by construction; a length past the file is clamped so a
    // truncated file still yields a thumbnail when the element is complete.
    let header = data
        .get(7..)
        .map(|rest| &rest[..rest.len().min(len)])
        .unwrap_or_default();
    let thumb = memmem::find(header, b"<thumb").ok_or(DecodeError::Unsupported)?;
    let tag = &header[thumb..];
    let tag = &tag[..memchr::memchr(b'>', tag)
        .ok_or_else(|| DecodeError::malformed("pdn: open thumb tag"))?];
    let key = memmem::find(tag, b"png=\"").ok_or(DecodeError::Unsupported)?;
    let value = &tag[key + 5..];
    let value = &value[..memchr::memchr(b'"', value)
        .ok_or_else(|| DecodeError::malformed("pdn: open thumb attribute"))?];
    let png = b64::decode(value, header.len())?;
    decode_bytes(&png)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::design::testutil;

    fn file(xml: &str, tail: &[u8]) -> Vec<u8> {
        let mut f = b"PDN3".to_vec();
        f.extend(&(xml.len() as u32).to_le_bytes()[..3]);
        f.extend(xml.as_bytes());
        f.extend([0, 1]);
        f.extend(tail);
        f
    }

    fn xml(b64: &str) -> String {
        format!(
            "<pdnImage width=\"6\" height=\"4\" layers=\"1\" savedWith=\"4.3\">\
             <custom><thumb png=\"{b64}\" /></custom><userMetadata /></pdnImage>"
        )
    }

    #[test]
    fn extracts_thumbnail() {
        let png = testutil::png(6, 4, [10, 200, 30, 255]);
        let f = file(&xml(&super::b64::encode(&png)), &[0x1F, 0x8B, 8, 0]);
        let img = decode_ok(&f);
        assert_eq!(img.dimensions(), (6, 4));
        assert_eq!(img.get_pixel(2, 2).0, [10, 200, 30, 255]);
    }

    fn decode_ok(f: &[u8]) -> image::RgbaImage {
        extract(f).unwrap().to_rgba8()
    }

    #[test]
    fn hostile_inputs() {
        assert_eq!(extract(b"PDN"), Err(DecodeError::Unsupported));
        assert!(matches!(
            extract(b"PDN3\x05"),
            Err(DecodeError::Malformed(_))
        ));
        assert_eq!(
            extract(&file("<pdnImage/>", &[])),
            Err(DecodeError::Unsupported)
        );
        // Header length lies past the end of the file.
        let png = testutil::png(2, 2, [1, 2, 3, 255]);
        let mut f = file(&xml(&super::b64::encode(&png)), &[]);
        f[4..7].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
        assert!(extract(&f).is_ok());
        // Unterminated attribute / garbage payload.
        assert!(extract(&file("<thumb png=\"AAAA", &[])).is_err());
        assert!(extract(&file("<thumb png=\"QUJD\" />", &[])).is_err());
        // PNG whose IHDR claims 4G x 4G.
        let mut big = png.clone();
        big[16..24].copy_from_slice(&[0xFF; 8]);
        let f = file(&xml(&super::b64::encode(&big)), &[]);
        assert_eq!(extract(&f), Err(DecodeError::TooLarge));
    }
}
