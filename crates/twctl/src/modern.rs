//! AVIF and JPEG XL writers, both pure Rust (no assembler, no C): AVIF through `ravif`
//! (rav1e without its `asm` feature), JPEG XL through `zune-jpegxl` (lossless only).

use image::DynamicImage;

use crate::carry::Outgoing;
use crate::ops::Result;

/// rav1e speed preset (0 slowest .. 10 fastest). Without assembly, 8 keeps a photo-sized
/// conversion in seconds at a modest size cost.
const AVIF_SPEED: u8 = 8;
const AVIF_DEFAULT_QUALITY: u8 = 80;

/// Extensions written here instead of by the `image` crate.
pub fn handles(ext: &str) -> bool {
    matches!(ext, "avif" | "jxl")
}

fn has_alpha(img: &DynamicImage) -> bool {
    img.color().has_alpha() && img.to_rgba8().pixels().any(|p| p[3] != 255)
}

/// AVIF through ravif. The EXIF block becomes an `Exif` item (avif-serialize stores it with
/// the 4-byte TIFF-header offset HEIF expects); the writer has no ICC (`prof` colour
/// property) or XMP support, so those never get here (see `carry::prepare`).
fn avif(img: &DynamicImage, quality: Option<u8>, exif: Option<&[u8]>) -> Result<Vec<u8>> {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let q = f32::from(quality.unwrap_or(AVIF_DEFAULT_QUALITY).clamp(1, 100));
    let mut encoder = ravif::Encoder::new()
        .with_quality(q)
        .with_alpha_quality(q)
        .with_speed(AVIF_SPEED);
    if let Some(e) = exif {
        encoder = encoder.with_exif(e);
    }
    let encoded = if has_alpha(img) {
        let px: Vec<rgb::RGBA8> = img
            .to_rgba8()
            .pixels()
            .map(|p| rgb::RGBA8::new(p[0], p[1], p[2], p[3]))
            .collect();
        encoder.encode_rgba(imgref::Img::new(px.as_slice(), w, h))
    } else {
        let px: Vec<rgb::RGB8> = img
            .to_rgb8()
            .pixels()
            .map(|p| rgb::RGB8::new(p[0], p[1], p[2]))
            .collect();
        encoder.encode_rgb(imgref::Img::new(px.as_slice(), w, h))
    };
    encoded
        .map(|e| e.avif_file)
        .map_err(|e| format!("AVIF: {e}"))
}

/// Wraps a bare JPEG XL codestream in the ISO BMFF container so it can carry metadata
/// boxes: signature, `ftyp`, the codestream (`jxlc`), then `Exif` (4-byte TIFF-header offset
/// of 0, then the block) and `xml ` (the XMP packet).
fn jxl_container(codestream: &[u8], exif: Option<&[u8]>, xmp: Option<&[u8]>) -> Result<Vec<u8>> {
    fn put_box(out: &mut Vec<u8>, typ: &[u8; 4], parts: &[&[u8]]) -> Result<()> {
        let body: usize = parts.iter().map(|p| p.len()).sum();
        let size = u32::try_from(body + 8).map_err(|_| "JPEG XL box too large".to_string())?;
        out.extend_from_slice(&size.to_be_bytes());
        out.extend_from_slice(typ);
        parts.iter().for_each(|p| out.extend_from_slice(p));
        Ok(())
    }
    let mut out = Vec::with_capacity(
        codestream.len() + exif.map_or(0, <[u8]>::len) + xmp.map_or(0, <[u8]>::len) + 64,
    );
    put_box(&mut out, b"JXL ", &[&[0x0D, 0x0A, 0x87, 0x0A]])?;
    put_box(&mut out, b"ftyp", &[b"jxl ", &[0; 4], b"jxl "])?;
    put_box(&mut out, b"jxlc", &[codestream])?;
    if let Some(e) = exif {
        put_box(&mut out, b"Exif", &[&[0; 4], e])?;
    }
    if let Some(x) = xmp {
        put_box(&mut out, b"xml ", &[x])?;
    }
    Ok(out)
}

/// Lossless JPEG XL; `quality` does not apply. The writer emits a bare codestream with an
/// sRGB colour encoding, so an ICC profile cannot be embedded; EXIF and XMP go into
/// container boxes.
fn jxl(img: &DynamicImage, exif: Option<&[u8]>, xmp: Option<&[u8]>) -> Result<Vec<u8>> {
    use zune_core::bit_depth::BitDepth;
    use zune_core::colorspace::ColorSpace;
    use zune_core::options::EncoderOptions;

    let (w, h) = (img.width() as usize, img.height() as usize);
    if w < 2 || h < 2 {
        return Err("JPEG XL output needs an image of at least 2x2 pixels".into());
    }
    let (pixels, space) = if has_alpha(img) {
        (img.to_rgba8().into_raw(), ColorSpace::RGBA)
    } else {
        (img.to_rgb8().into_raw(), ColorSpace::RGB)
    };
    let mut out = Vec::new();
    zune_jpegxl::JxlSimpleEncoder::new(&pixels, EncoderOptions::new(w, h, space, BitDepth::Eight))
        .encode(&mut out)
        .map_err(|e| format!("JPEG XL: {e:?}"))?;
    if exif.is_none() && xmp.is_none() {
        return Ok(out);
    }
    jxl_container(&out, exif, xmp)
}

/// Encodes `img` as `ext` (`avif` or `jxl`) with the metadata `carried` (already fitted to
/// the target by `carry::prepare`).
pub fn encode(
    img: &DynamicImage,
    ext: &str,
    quality: Option<u8>,
    carried: &Outgoing,
) -> Result<Vec<u8>> {
    match ext {
        "avif" => avif(img, quality, carried.exif.as_deref()),
        "jxl" => jxl(img, carried.exif.as_deref(), carried.xmp.as_deref()),
        other => Err(format!("no writer for .{other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};
    use tw_core::decode::route::Decoder;

    fn gradient(alpha: bool) -> DynamicImage {
        RgbaImage::from_fn(33, 17, |x, y| {
            Rgba([
                (x * 7) as u8,
                (y * 13) as u8,
                ((x + y) * 5) as u8,
                if alpha && x < 10 { 90 } else { 255 },
            ])
        })
        .into()
    }

    #[test]
    fn jxl_is_lossless_and_keeps_alpha() {
        for alpha in [false, true] {
            let src = gradient(alpha);
            let bytes = encode(&src, "jxl", None, &Outgoing::default()).unwrap();
            let back = Decoder::Jxl.run(&bytes, "jxl", 0).unwrap();
            assert_eq!((back.width(), back.height()), (33, 17));
            let (a, b) = (src.to_rgba8(), back.to_rgba8());
            for (pa, pb) in a.pixels().zip(b.pixels()) {
                if pa[3] != 0 {
                    assert_eq!(pa.0, pb.0, "alpha={alpha}");
                }
            }
        }
    }

    #[test]
    fn jxl_rejects_degenerate_sizes() {
        assert!(
            encode(
                &DynamicImage::new_rgb8(1, 8),
                "jxl",
                None,
                &Outgoing::default()
            )
            .is_err()
        );
    }

    #[test]
    fn avif_has_the_ftyp_brand_and_a_decodable_size() {
        let bytes = encode(&gradient(true), "avif", Some(60), &Outgoing::default()).unwrap();
        assert_eq!(&bytes[4..8], b"ftyp");
        assert!(bytes[8..32].windows(4).any(|w| w == b"avif"), "avif brand");
        assert!(encode(&gradient(false), "avif", None, &Outgoing::default()).is_ok());
    }

    fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }

    /// Top-level ISO BMFF boxes of `d`: `(type, body)`.
    fn boxes(d: &[u8]) -> Vec<([u8; 4], &[u8])> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 8 <= d.len() {
            let size = u32::from_be_bytes(d[i..i + 4].try_into().unwrap()) as usize;
            assert!(
                size >= 8 && i + size <= d.len(),
                "box at {i} has size {size}"
            );
            out.push((d[i + 4..i + 8].try_into().unwrap(), &d[i + 8..i + size]));
            i += size;
        }
        assert_eq!(i, d.len(), "boxes cover the file exactly");
        out
    }

    #[test]
    fn avif_carries_exif_as_an_item() {
        let exif = crate::exif::testutil::camera_exif(1, (33, 17));
        let carried = Outgoing {
            exif: Some(exif.clone()),
            ..Outgoing::default()
        };
        let with = encode(&gradient(false), "avif", Some(60), &carried).unwrap();
        let without = encode(&gradient(false), "avif", Some(60), &Outgoing::default()).unwrap();
        assert!(find(&with, b"Exif").is_some(), "an Exif item is declared");
        assert!(find(&without, b"Exif").is_none());
        // The block itself sits in the file data (after `mdat`), preceded by the 4-byte
        // offset to the TIFF header HEIF expects (0: the header is right there).
        let at = find(&with, &exif).expect("the EXIF bytes are stored");
        assert_eq!(&with[at - 4..at], &[0, 0, 0, 0]);
        assert!(at > find(&with, b"mdat").unwrap());
        assert!(find(&without, &exif).is_none());
        assert_eq!(&with[4..8], b"ftyp");
    }

    #[test]
    fn jxl_container_holds_exif_and_xmp_and_the_image_still_decodes() {
        let exif = crate::exif::testutil::camera_exif(1, (33, 17));
        let xmp = br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"/>"#.to_vec();
        let carried = Outgoing {
            exif: Some(exif.clone()),
            xmp: Some(xmp.clone()),
            ..Outgoing::default()
        };
        let src = gradient(true);
        let bytes = encode(&src, "jxl", None, &carried).unwrap();
        let parts = boxes(&bytes);
        let kinds: Vec<&[u8; 4]> = parts.iter().map(|(t, _)| t).collect();
        assert_eq!(kinds, [b"JXL ", b"ftyp", b"jxlc", b"Exif", b"xml "]);
        assert_eq!(parts[0].1, [0x0D, 0x0A, 0x87, 0x0A]);
        assert_eq!(parts[1].1, *b"jxl \0\0\0\0jxl ");
        assert_eq!(&parts[2].1[..2], [0xFF, 0x0A], "a bare codestream inside");
        assert_eq!(parts[3].1, [&[0u8; 4][..], &exif[..]].concat());
        assert_eq!(parts[4].1, xmp);
        let back = Decoder::Jxl.run(&bytes, "jxl", 0).unwrap();
        assert_eq!((back.width(), back.height()), (33, 17));
        for (a, b) in src.to_rgba8().pixels().zip(back.to_rgba8().pixels()) {
            if a[3] != 0 {
                assert_eq!(a.0, b.0);
            }
        }
        // Without metadata the file stays a bare codestream.
        let plain = encode(&src, "jxl", None, &Outgoing::default()).unwrap();
        assert_eq!(&plain[..2], [0xFF, 0x0A]);
        // Only one of the two: only that box.
        let only_xmp = Outgoing {
            xmp: Some(xmp),
            ..Outgoing::default()
        };
        let b = encode(&src, "jxl", None, &only_xmp).unwrap();
        assert_eq!(boxes(&b).len(), 4);
    }
}
