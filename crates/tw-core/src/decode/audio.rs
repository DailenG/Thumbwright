//! Embedded cover art from audio tags (ID3v2, Vorbis comments / FLAC pictures, MP4
//! `covr`, APE), read with `lofty`. The front cover wins; otherwise the first picture.

use std::io::Cursor;

use image::DynamicImage;
use lofty::config::ParseOptions;
use lofty::file::{FileType, TaggedFileExt};
use lofty::picture::{Picture, PictureType};
use lofty::probe::Probe;

use super::{DecodeError, Result, decode_bytes};

/// Formats `lofty` parses (by content, with the extension as a hint) that the format
/// matrix lists under audio. WMA (ASF) and DSF are not readable by `lofty`.
pub const EXTENSIONS: &[&str] = &[
    "mp3", "flac", "ogg", "oga", "opus", "spx", "m4a", "m4b", "aac", "ape", "wv", "mpc", "wav",
    "aiff", "aif", "aifc",
];

pub fn extract(data: &[u8], ext: &str) -> Result<DynamicImage> {
    let options = ParseOptions::new().read_properties(false);
    let probe = Probe::new(Cursor::new(data))
        .options(options)
        .guess_file_type()
        .map_err(DecodeError::malformed)?;
    let probe = match probe.file_type() {
        Some(_) => probe,
        None => match FileType::from_ext(ext) {
            Some(hint) => probe.set_file_type(hint),
            None => return Err(DecodeError::Unsupported),
        },
    };
    let tagged = probe.read().map_err(DecodeError::malformed)?;
    let pictures: Vec<&Picture> = tagged.tags().iter().flat_map(|t| t.pictures()).collect();
    decode_cover(&pictures)
}

/// Front cover first, then the remaining pictures in tag order; the first that decodes
/// wins.
fn decode_cover(pictures: &[&Picture]) -> Result<DynamicImage> {
    let is_front = |p: &&&Picture| p.pic_type() == PictureType::CoverFront;
    let ordered = pictures
        .iter()
        .filter(is_front)
        .chain(pictures.iter().filter(|p| !is_front(p)));
    let mut last = DecodeError::Unsupported;
    for picture in ordered {
        match decode_bytes(picture.data()) {
            Ok(img) => return Ok(img),
            Err(e) if e.falls_through() => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgba, RgbaImage};

    fn png(color: [u8; 4], w: u32, h: u32) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        RgbaImage::from_pixel(w, h, Rgba(color))
            .write_to(&mut out, ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    fn apic(pic_type: u8, data: &[u8]) -> Vec<u8> {
        let mut body = vec![0u8]; // ISO-8859-1
        body.extend_from_slice(b"image/png\0");
        body.push(pic_type);
        body.push(0); // empty description
        body.extend_from_slice(data);
        let mut frame = b"APIC".to_vec();
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frame.extend_from_slice(&[0, 0]);
        frame.extend_from_slice(&body);
        frame
    }

    fn synchsafe(n: u32) -> [u8; 4] {
        [
            ((n >> 21) & 0x7F) as u8,
            ((n >> 14) & 0x7F) as u8,
            ((n >> 7) & 0x7F) as u8,
            (n & 0x7F) as u8,
        ]
    }

    /// ID3v2.3 tag with the given frames, followed by silent MPEG-1 layer III frames.
    fn mp3(frames: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = frames.concat();
        let mut f = b"ID3\x03\x00\x00".to_vec();
        f.extend_from_slice(&synchsafe(body.len() as u32));
        f.extend_from_slice(&body);
        for _ in 0..4 {
            // 128 kbit/s, 44.1 kHz, no padding: 417-byte frames.
            f.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
            f.extend_from_slice(&[0u8; 413]);
        }
        f
    }

    #[test]
    fn front_cover_is_preferred() {
        let other = apic(0, &png([255, 0, 0, 255], 4, 4));
        let front = apic(3, &png([0, 0, 255, 255], 6, 3));
        let img = extract(&mp3(&[other, front]), "mp3").unwrap();
        assert_eq!((img.width(), img.height()), (6, 3));
        assert_eq!(img.to_rgba8().get_pixel(0, 0).0, [0, 0, 255, 255]);
    }

    #[test]
    fn first_picture_when_no_front_cover() {
        let back = apic(4, &png([0, 255, 0, 255], 2, 5));
        let img = extract(&mp3(&[back]), "mp3").unwrap();
        assert_eq!((img.width(), img.height()), (2, 5));
        assert_eq!(img.to_rgba8().get_pixel(1, 4).0, [0, 255, 0, 255]);
    }

    #[test]
    fn undecodable_front_cover_falls_back_to_next_picture() {
        let broken = apic(3, b"\x89PNG\r\n\x1a\nbroken");
        let good = apic(0, &png([9, 9, 9, 255], 3, 3));
        let img = extract(&mp3(&[broken, good]), "mp3").unwrap();
        assert_eq!(img.to_rgba8().get_pixel(0, 0).0, [9, 9, 9, 255]);
    }

    #[test]
    fn no_art_or_foreign_data_is_rejected() {
        assert_eq!(extract(&mp3(&[]), "mp3"), Err(DecodeError::Unsupported));
        assert_eq!(
            extract(b"definitely not audio", "xyz"),
            Err(DecodeError::Unsupported)
        );
        // Tag declaring far more bytes than the file holds.
        let mut lying = b"ID3\x03\x00\x00".to_vec();
        lying.extend_from_slice(&synchsafe(0x0FFF_FFFF));
        lying.extend_from_slice(&apic(3, &png([1, 2, 3, 255], 2, 2))[..20]);
        assert!(matches!(
            extract(&lying, "mp3"),
            Err(DecodeError::Malformed(_))
        ));
    }
}
