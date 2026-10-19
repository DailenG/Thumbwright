//! Apple Live Photo bundles (`.livp`): a ZIP holding the still image (HEIC, or JPEG) and a
//! QuickTime movie.
//!
//! The still is pulled out of the ZIP under the usual entry/total budgets
//! ([`super::zip_preview::livp_still`]). A JPEG/PNG still is decoded in Rust; a HEIC/HEIF
//! still goes to `open-heic` ([`super::heif`]) and, when that cannot decode it, to the OS
//! codecs; WIC is only allowed in the worker process, so this decoder runs there
//! (`Placement::Worker`, REQ-ISO-2).

use image::DynamicImage;

use super::zip_preview::livp_still;
use super::{DecodeError, Result, decode_bytes};

pub const EXTENSIONS: &[&str] = &["livp"];

pub fn extract(data: &[u8], edge: u32) -> Result<DynamicImage> {
    let (still, ext) = livp_still(data)?;
    if matches!(ext, "heic" | "heif") {
        return heif_then_wic(&still, edge);
    }
    match decode_bytes(&still) {
        // A mislabelled HEIC inside a ".jpg" entry.
        Err(DecodeError::Unsupported) => heif_then_wic(&still, edge),
        other => other,
    }
}

/// `open-heic`, then the OS codec for what it declines or cannot decode. A budget failure
/// (`TooLarge`, `Timeout`) is final (REQ-TMO-1).
fn heif_then_wic(still: &[u8], edge: u32) -> Result<DynamicImage> {
    match super::heif::decode(still, edge) {
        Err(e) if e.falls_through() => wic(still, edge),
        other => other,
    }
}

#[cfg(windows)]
fn wic(still: &[u8], edge: u32) -> Result<DynamicImage> {
    super::wic::decode(still, edge)
}

#[cfg(not(windows))]
fn wic(_still: &[u8], _edge: u32) -> Result<DynamicImage> {
    Err(DecodeError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GenericImageView, ImageFormat, Rgb, RgbImage};
    use std::io::{Cursor, Write};

    fn jpeg(w: u32, h: u32, color: [u8; 3]) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        RgbImage::from_pixel(w, h, Rgb(color))
            .write_to(&mut out, ImageFormat::Jpeg)
            .unwrap();
        out.into_inner()
    }

    fn zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default();
        for (name, data) in files {
            w.start_file(*name, opts).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    #[test]
    fn livp_with_jpeg_still_decodes_in_rust() {
        let z = zip(&[
            ("IMG_0042.MOV", b"\0\0\0\x14ftypqt  movie bytes"),
            ("IMG_0042.JPG", &jpeg(40, 30, [200, 100, 50])),
        ]);
        let img = extract(&z, 256).unwrap();
        assert_eq!(img.dimensions(), (40, 30));
        let p = img.get_pixel(20, 15).0;
        assert!(p[0] > 180 && p[1] > 80 && p[1] < 120 && p[2] < 80, "{p:?}");
    }

    /// The HEIC still is decoded by `open-heic`, with no OS codec involved.
    #[test]
    fn livp_with_heic_still_decodes_with_open_heic() {
        let heic = include_bytes!("../../tests/fixtures/heic/quad.heic");
        let z = zip(&[
            ("IMG_0001.MOV", b"\0\0\0\x14ftypqt  movie bytes"),
            ("IMG_0001.HEIC", heic),
        ]);
        assert_eq!(extract(&z, 256).unwrap().dimensions(), (160, 120));
        // A HEIC under a ".jpg" entry name is found by content.
        let z = zip(&[("IMG_0002.JPG", heic)]);
        assert_eq!(extract(&z, 256).unwrap().dimensions(), (160, 120));
    }

    #[test]
    fn livp_hostile_inputs_fail_cleanly() {
        assert_eq!(extract(b"", 256), Err(DecodeError::Unsupported));
        assert_eq!(extract(b"not a zip", 256), Err(DecodeError::Unsupported));
        // No still image inside.
        assert_eq!(
            extract(&zip(&[("IMG.MOV", b"movie")]), 256),
            Err(DecodeError::Unsupported)
        );
        // A damaged JPEG still.
        let z = zip(&[("IMG.JPG", b"\xFF\xD8\xFF\xE0 truncated garbage")]);
        assert!(matches!(extract(&z, 256), Err(DecodeError::Malformed(_))));
        // A truncated bundle.
        let z = zip(&[("IMG.JPG", &jpeg(8, 8, [1, 2, 3]))]);
        for cut in 0..z.len() {
            assert!(
                matches!(
                    extract(&z[..cut], 256),
                    Err(DecodeError::Unsupported | DecodeError::Malformed(_))
                ),
                "cut {cut}"
            );
        }
        // A still declaring 65535 x 65535 pixels.
        let mut hdr = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        hdr.extend(65535u32.to_be_bytes());
        hdr.extend(65535u32.to_be_bytes());
        hdr.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
        let z = zip(&[("IMG.PNG", &hdr)]);
        assert!(matches!(
            extract(&z, 256),
            Err(DecodeError::TooLarge | DecodeError::Malformed(_))
        ));
    }
}
