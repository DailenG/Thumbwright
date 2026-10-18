//! AVIF and JPEG XL writers, both pure Rust (no assembler, no C): AVIF through `ravif`
//! (rav1e without its `asm` feature), JPEG XL through `zune-jpegxl` (lossless only).

use image::DynamicImage;

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

fn avif(img: &DynamicImage, quality: Option<u8>) -> Result<Vec<u8>> {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let q = f32::from(quality.unwrap_or(AVIF_DEFAULT_QUALITY).clamp(1, 100));
    let encoder = ravif::Encoder::new()
        .with_quality(q)
        .with_alpha_quality(q)
        .with_speed(AVIF_SPEED);
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

/// Lossless JPEG XL; `quality` does not apply.
fn jxl(img: &DynamicImage) -> Result<Vec<u8>> {
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
    Ok(out)
}

/// Encodes `img` as `ext` (`avif` or `jxl`).
pub fn encode(img: &DynamicImage, ext: &str, quality: Option<u8>) -> Result<Vec<u8>> {
    match ext {
        "avif" => avif(img, quality),
        "jxl" => jxl(img),
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
            let bytes = encode(&src, "jxl", None).unwrap();
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
        assert!(encode(&DynamicImage::new_rgb8(1, 8), "jxl", None).is_err());
    }

    #[test]
    fn avif_has_the_ftyp_brand_and_a_decodable_size() {
        let bytes = encode(&gradient(true), "avif", Some(60)).unwrap();
        assert_eq!(&bytes[4..8], b"ftyp");
        assert!(bytes[8..32].windows(4).any(|w| w == b"avif"), "avif brand");
        assert!(encode(&gradient(false), "avif", None).is_ok());
    }
}
