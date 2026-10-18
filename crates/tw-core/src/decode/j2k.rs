//! JPEG 2000 (`.j2c`, `.j2k`, `.jp2`, `.jpc`, `.jpm`, `.jpf`, `.jpx`) through the pure-Rust,
//! `forbid(unsafe_code)` `hayro-jpeg2000` decoder.
//!
//! The decoder cannot bound its internal allocations or be cancelled, so this tier runs in
//! the worker process (`Placement::Worker`). The declared size is checked from the header
//! before decoding, and the codestream is decoded at the lowest resolution level that still
//! covers the requested thumbnail edge, which also keeps the work and memory small.

use hayro_jpeg2000::{
    ColorSpace, DecodeError as J2kError, DecodeSettings, DecoderContext, FormatError, Image,
};
use image::{DynamicImage, GrayAlphaImage};

use super::{DecodeError, Result, check_alloc, check_dimensions};

pub const EXTENSIONS: &[&str] = &["j2c", "j2k", "jp2", "jpc", "jpm", "jpf", "jpx"];

/// Bytes of working memory per decoded sample: the decoder keeps `f32` samples for every
/// component plus transform scratch.
const BYTES_PER_SAMPLE: u64 = 12;

fn map_err(e: J2kError) -> DecodeError {
    match e {
        J2kError::Format(FormatError::InvalidSignature | FormatError::InvalidFileType) => {
            DecodeError::Unsupported
        }
        other => DecodeError::malformed(format!("JPEG 2000: {other}")),
    }
}

pub fn decode(data: &[u8], edge: u32) -> Result<DynamicImage> {
    // First parse: the full-resolution size, validated before any decoding.
    let probe = Image::new(data, &DecodeSettings::default()).map_err(map_err)?;
    let (full_w, full_h) = (probe.width(), probe.height());
    check_dimensions(full_w, full_h)?;

    // Resolution reduction: smallest level whose size still covers the thumbnail.
    let edge = u64::from(edge.max(1));
    let long = u64::from(full_w.max(full_h));
    let target = |side: u32| (u64::from(side) * edge).div_ceil(long).max(1) as u32;
    let settings = DecodeSettings {
        target_resolution: Some((target(full_w), target(full_h))),
        ..DecodeSettings::default()
    };
    let image = Image::new(data, &settings).map_err(map_err)?;
    let (w, h) = (image.width(), image.height());
    check_dimensions(w, h)?;
    let components = u64::from(image.color_space().num_channels()) + u64::from(image.has_alpha());
    check_alloc(u64::from(w) * u64::from(h) * components.max(1) * BYTES_PER_SAMPLE)?;

    let mut ctx = DecoderContext::default();
    let decoded = image.decode(&mut ctx).map_err(map_err)?;
    let channels = decoded.components().len();
    let bytes = decoded.data_u8();
    if bytes.len() as u64 != u64::from(w) * u64::from(h) * channels as u64 {
        return Err(DecodeError::malformed("JPEG 2000 sample count mismatch"));
    }
    let mismatch = || DecodeError::malformed("pixel buffer size mismatch");
    match channels {
        1 => image::GrayImage::from_raw(w, h, bytes)
            .map(DynamicImage::ImageLuma8)
            .ok_or_else(mismatch),
        2 => GrayAlphaImage::from_raw(w, h, bytes)
            .map(DynamicImage::ImageLumaA8)
            .ok_or_else(mismatch),
        3 => image::RgbImage::from_raw(w, h, bytes)
            .map(DynamicImage::ImageRgb8)
            .ok_or_else(mismatch),
        4 if image.has_alpha() => image::RgbaImage::from_raw(w, h, bytes)
            .map(DynamicImage::ImageRgba8)
            .ok_or_else(mismatch),
        4 if matches!(image.color_space(), ColorSpace::CMYK) => {
            let rgb = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|p| {
                    let k = u32::from(p[3]);
                    let ink = |v: u8| ((255 - u32::from(v)) * (255 - k) / 255) as u8;
                    [ink(p[0]), ink(p[1]), ink(p[2])]
                })
                .collect();
            image::RgbImage::from_raw(w, h, rgb)
                .map(DynamicImage::ImageRgb8)
                .ok_or_else(mismatch)
        }
        _ => Err(DecodeError::Unsupported),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 32x32 fixtures written with OpenJPEG through Pillow: quadrants red, green, blue and
    // white (the RGBA one has alpha 255 except 128 in the white quadrant); 3 resolution levels.
    const J2K_RGB_HEX: &str = concat!(
        "ff4fff51002f000000000020000000200000000000000000000000200000002000000000000000000003070101070101",
        "070101ff52000c00000001000204040001ff5c000a4040484850484850ff640025000143726561746564206279204f70",
        "656e4a5045472076657273696f6e20322e352e34ff90000a0000000001600001ff93df81f003688aed4c3be494d53894",
        "1af1d2e3d6dd091d036d33672d6f49749b6888475ba5f1d0c563106395c4f9d768ecc8fa9dd2e9b1641364229f6952c3",
        "cad327df81c0115041d64575b35b61797fc0300d40a61386f7b1ff3cfcdda28360e9cbeee0e8bd0246761c571da167fb",
        "11cd87e6c3f361f9b0fcd87e6c3fdf818811505318dbf0d32a5b4269ce87a59d8225145d734023f7ea90a128e569a53e",
        "56f11194b79dcea002a023ee53447dca688fcfc0467e0231f804802218da7ac37d7949e1f9fcaf99287f8b2f03782507",
        "62ff440ea39aaca774a42bebbf242c2b9c0fdc3583bfcfc040216e0d18be7a484ae7010f70b9da1c3fa7e01e00d4c78b",
        "f700702124981eb1eb03cfcfc0527e02b1f80200369e399d2d2a43ccda805f2ed21eff5de53a03f366adf29595361a2c",
        "65a8f4163c9b21f1df5402813f8314f879c7da0a0035f23faabda3ed0600676aaae53f03ffd9"
    );

    const JP2_RGBA_HEX: &str = concat!(
        "0000000c6a5020200d0a870a00000014667479706a703220000000006a7032200000004f6a7032680000001669686472",
        "00000020000000200004070700000000000f636f6c720100000000001000000022636465660004000000000001000100",
        "000002000200000003000300010000000002596a703263ff4fff51003200000000002000000020000000000000000000",
        "0000200000002000000000000000000004070101070101070101070101ff52000c00000001000204040001ff5c000a40",
        "40484850484850ff640025000143726561746564206279204f70656e4a5045472076657273696f6e20322e352e34ff90",
        "000a0000000001d80001ff93df81f003688aed4c3be494d538941af1d2e3d6dd091d036d33672d6f49749b6888475ba5",
        "f1d0c563106395c4f9d768ecc8fa9dd2e9b1641364229f6952c3cad327df81c0115041d64575b35b61797fc0300d40a6",
        "1386f7b1ff3cfcdda28360e9cbeee0e8bd0246761c571da167fb11cd87e6c3f361f9b0fcd87e6c3fdf818811505318db",
        "f0d32a5b4269ce87a59d8225145d734023f7ea90a128e569a53e56f11194b79dcea002a023ee53447dca688fdf818029",
        "1fb004578b202af011f3c6882b7845292154f842c80f8ed4ccce5774b97f547a163cadd62cc8ebb48f35a9e220c00fcf",
        "c0467e0231f804802218da7ac37d7949e1f9fcaf99287f8b2f0378250762ff440ea39aaca774a42bebbf242c2b9c0fdc",
        "3583bfcfc040216e0d18be7a484ae7010f70b9da1c3fa7e01e00d4c78bf700702124981eb1eb03cfc7da191f685c1f50",
        "3849cddb2ec6ed8280611a077f2e4231e76d9df82ac64cd624dd5059723f5fcfc0527e02b1f80200369e399d2d2a43cc",
        "da805f2ed21eff5de53a03f366adf29595361a2c65a8f4163c9b21f1df5402813f8314f879c7da0a0035f23faabda3ed",
        "0600676aaae53f03c7da131f68441f5020938ffecd54440fce9f8530a8645aea4b5b82ff3c1bffd9"
    );

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn codestream_decodes_at_full_size() {
        let img = decode(&unhex(J2K_RGB_HEX), 64).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (32, 32));
        assert_eq!(img.get_pixel(4, 4).0, [255, 0, 0]);
        assert_eq!(img.get_pixel(20, 4).0, [0, 255, 0]);
        assert_eq!(img.get_pixel(4, 20).0, [0, 0, 255]);
        assert_eq!(img.get_pixel(20, 20).0, [255, 255, 255]);
    }

    #[test]
    fn small_thumbnail_uses_a_reduced_resolution_level() {
        let img = decode(&unhex(J2K_RGB_HEX), 8).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (8, 8));
        assert_eq!(img.get_pixel(1, 1).0, [255, 0, 0]);
        assert_eq!(img.get_pixel(6, 1).0, [0, 255, 0]);
        assert_eq!(img.get_pixel(1, 6).0, [0, 0, 255]);
    }

    #[test]
    fn jp2_container_keeps_alpha() {
        let img = decode(&unhex(JP2_RGBA_HEX), 64).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (32, 32));
        assert_eq!(img.get_pixel(4, 4).0, [255, 0, 0, 255]);
        let p = img.get_pixel(20, 20).0;
        assert!(
            p[..3].iter().all(|c| *c == 255) && (127..=129).contains(&p[3]),
            "{p:?}"
        );
    }

    #[test]
    fn hostile_inputs_are_rejected() {
        assert_eq!(decode(b"not jpeg 2000", 256), Err(DecodeError::Unsupported));
        assert!(matches!(
            decode(&[0xFF, 0x4F, 0xFF, 0x51, 0x00], 256),
            Err(DecodeError::Malformed(_))
        ));
        // SIZ declaring 65535 x 65535 (image and tile size) on a tiny file.
        let mut f = unhex(J2K_RGB_HEX);
        for at in [8usize, 12, 24, 28] {
            f[at..at + 4].copy_from_slice(&65535u32.to_be_bytes());
        }
        assert_eq!(decode(&f, 256), Err(DecodeError::TooLarge));
        // u32::MAX x u32::MAX.
        for at in [8usize, 12, 24, 28] {
            f[at..at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        }
        assert!(matches!(
            decode(&f, 256),
            Err(DecodeError::TooLarge | DecodeError::Malformed(_))
        ));
        // 20000 x 20000: under the pixel budget but over the working-memory budget.
        for at in [8usize, 12, 24, 28] {
            f[at..at + 4].copy_from_slice(&20000u32.to_be_bytes());
        }
        assert_eq!(decode(&f, 2), Err(DecodeError::TooLarge));
        // Truncated codestream.
        let f = unhex(J2K_RGB_HEX);
        assert!(decode(&f[..f.len() / 2], 256).is_err());
    }
}
