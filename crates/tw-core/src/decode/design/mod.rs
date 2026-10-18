//! Embedded-preview extraction for design and package formats: InDesign (XMP thumbnail),
//! CorelDRAW (RIFF `DISP` DIB or ZIP package thumbnail), EPS (DOS TIFF preview, EPSI
//! bitmap, XMP thumbnail), Blender (`TEST` thumbnail block), Apple icons, Aseprite and
//! Paint.NET.
//!
//! Every parser is bounded: declared sizes are checked before allocation, nested
//! structures have entry/depth caps, and decompression reads through `take(limit)`.

mod aseprite;
mod b64;
mod blend;
mod corel;
mod eps;
mod icns;
mod pdn;
mod xmp;

use image::DynamicImage;

use super::{DecodeError, Result};

pub const EXTENSIONS: &[&str] = &[
    "indd", "indt", "cdr", "cdt", "eps", "icns", "ase", "aseprite", "pdn", "blend", "blend1",
    "blend2", "blend3", "blend4", "blend5", "blend6", "blend7", "blend8", "blend9", "blend10",
    "blend11", "blend12", "blend13", "blend14", "blend15", "blend16", "blend17", "blend18",
    "blend19", "blend20", "blend21", "blend22", "blend23", "blend24", "blend25", "blend26",
    "blend27", "blend28", "blend29", "blend30", "blend31", "blend32",
];

pub fn decode(data: &[u8], ext: &str) -> Result<DynamicImage> {
    let ext = ext.to_ascii_lowercase();
    match ext.as_str() {
        "indd" | "indt" => xmp::thumbnail(data),
        "cdr" | "cdt" => corel::extract(data),
        "eps" => eps::extract(data),
        "icns" => icns::decode(data),
        "ase" | "aseprite" => aseprite::decode(data),
        "pdn" => pdn::extract(data),
        e if e.starts_with("blend") => blend::extract(data),
        _ => Err(DecodeError::Unsupported),
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::io::Cursor;

    use image::{ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};

    pub fn jpeg(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        RgbImage::from_pixel(w, h, Rgb(rgb))
            .write_to(&mut out, ImageFormat::Jpeg)
            .unwrap();
        out.into_inner()
    }

    pub fn png(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        RgbaImage::from_pixel(w, h, Rgba(rgba))
            .write_to(&mut out, ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    pub fn bmp24(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        RgbImage::from_pixel(w, h, Rgb(rgb))
            .write_to(&mut out, ImageFormat::Bmp)
            .unwrap();
        out.into_inner()
    }
}
