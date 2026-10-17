//! JPEG XL via `jxl-oxide`: the image header is parsed first so the declared size can be
//! checked before any frame is decoded; the first keyframe is rendered to 8-bit samples.

use image::{DynamicImage, GrayAlphaImage, GrayImage, RgbImage, RgbaImage};
use jxl_oxide::{AllocTracker, InitializeResult, JxlImage, JxlThreadPool, PixelFormat};

use super::{DecodeError, Result, check_dimensions};
use crate::limits;

const CODESTREAM_SIGNATURE: &[u8] = &[0xFF, 0x0A];
const CONTAINER_SIGNATURE: &[u8] = &[
    0x00, 0x00, 0x00, 0x0C, b'J', b'X', b'L', b' ', 0x0D, 0x0A, 0x87, 0x0A,
];
/// Bytes handed to the parser per step while looking for the image header.
const HEADER_CHUNK: usize = 4096;

fn malformed(e: impl std::fmt::Display) -> DecodeError {
    DecodeError::malformed(format!("JPEG XL: {e}"))
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if !data.starts_with(CODESTREAM_SIGNATURE) && !data.starts_with(CONTAINER_SIGNATURE) {
        return Err(DecodeError::Unsupported);
    }
    let tracker =
        AllocTracker::with_limit(usize::try_from(limits::DECODE_ALLOC_BYTES).unwrap_or(usize::MAX));
    let mut uninit = JxlImage::builder()
        .pool(JxlThreadPool::none())
        .alloc_tracker(tracker)
        .build_uninit();

    // Feed growing windows until the header is known; frames are not touched yet beyond
    // what arrived with the header bytes.
    let mut pos = 0usize;
    let mut end = 0usize;
    let mut image = loop {
        end = end.saturating_add(HEADER_CHUNK).min(data.len());
        pos += uninit.feed_bytes(&data[pos..end]).map_err(malformed)?;
        match uninit.try_init().map_err(malformed)? {
            InitializeResult::Initialized(image) => break image,
            InitializeResult::NeedMoreData(more) if end < data.len() => uninit = more,
            InitializeResult::NeedMoreData(_) => {
                return Err(DecodeError::malformed("JPEG XL header truncated"));
            }
        }
    };
    check_dimensions(image.width(), image.height())?;

    while pos < data.len() {
        let consumed = image.feed_bytes(&data[pos..]).map_err(malformed)?;
        if consumed == 0 {
            break;
        }
        pos += consumed;
    }
    image.finalize().map_err(malformed)?;
    if image.num_loaded_keyframes() == 0 {
        return Err(DecodeError::malformed("JPEG XL has no complete frame"));
    }

    let format = image.pixel_format();
    let render = image.render_frame(0).map_err(malformed)?;
    let mut stream = render.stream();
    let (w, h) = (stream.width(), stream.height());
    check_dimensions(w, h)?;
    let channels = stream.channels() as usize;
    if channels != format.channels() {
        return Err(DecodeError::malformed("JPEG XL channel layout mismatch"));
    }
    let mut buf = vec![0u8; w as usize * h as usize * channels];
    if stream.write_to_buffer(&mut buf) != buf.len() {
        return Err(DecodeError::malformed("JPEG XL render incomplete"));
    }
    to_dynamic(format, w, h, buf)
}

fn to_dynamic(format: PixelFormat, w: u32, h: u32, buf: Vec<u8>) -> Result<DynamicImage> {
    let err = || DecodeError::malformed("JPEG XL buffer size mismatch");
    Ok(match format {
        PixelFormat::Gray => {
            DynamicImage::ImageLuma8(GrayImage::from_raw(w, h, buf).ok_or_else(err)?)
        }
        PixelFormat::Graya => {
            DynamicImage::ImageLumaA8(GrayAlphaImage::from_raw(w, h, buf).ok_or_else(err)?)
        }
        PixelFormat::Rgb => DynamicImage::ImageRgb8(RgbImage::from_raw(w, h, buf).ok_or_else(err)?),
        PixelFormat::Rgba => {
            DynamicImage::ImageRgba8(RgbaImage::from_raw(w, h, buf).ok_or_else(err)?)
        }
        // CMYK samples are stored as "1 - ink"; approximate RGB without a CMS.
        PixelFormat::Cmyk | PixelFormat::Cmyka => {
            let stride = format.channels();
            let mut out = Vec::with_capacity(w as usize * h as usize * 4);
            for px in buf.chunks_exact(stride) {
                let k = u16::from(px[3]);
                let ink = |c: u8| ((u16::from(c) * k + 127) / 255) as u8;
                out.extend_from_slice(&[ink(px[0]), ink(px[1]), ink(px[2])]);
                out.push(if stride == 5 { px[4] } else { 255 });
            }
            DynamicImage::ImageRgba8(RgbaImage::from_raw(w, h, out).ok_or_else(err)?)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LSB-first bit writer (JPEG XL bit order).
    #[derive(Default)]
    struct Bits {
        out: Vec<u8>,
        used: u32,
    }

    impl Bits {
        fn put(&mut self, value: u32, n: u32) {
            for i in 0..n {
                if self.used.is_multiple_of(8) {
                    self.out.push(0);
                }
                let bit = ((value >> i) & 1) as u8;
                *self.out.last_mut().unwrap() |= bit << (self.used % 8);
                self.used += 1;
            }
        }
    }

    /// Codestream holding only an image header (no frames).
    fn header_only(width: u32, height: u32) -> Vec<u8> {
        let mut b = Bits::default();
        b.put(0x0AFF, 16); // signature FF 0A
        b.put(0, 1); // div8 = false
        b.put(3, 2); // height: 1 + u(30)
        b.put(height - 1, 30);
        b.put(0, 3); // ratio = 0: explicit width
        b.put(3, 2); // width: 1 + u(30)
        b.put(width - 1, 30);
        b.put(1, 1); // metadata all_default
        b.put(1, 1); // default_m (default opsin/upsampling)
        b.put(0, 6); // padding
        b.out
    }

    #[test]
    fn declared_size_is_checked_before_frames() {
        assert_eq!(
            decode(&header_only(100_000, 100_000)),
            Err(DecodeError::TooLarge)
        );
    }

    #[test]
    fn header_without_frames_is_malformed() {
        assert!(matches!(
            decode(&header_only(16, 16)),
            Err(DecodeError::Malformed(_))
        ));
    }

    #[test]
    fn foreign_and_truncated_inputs_are_rejected() {
        assert_eq!(decode(b"\x89PNG\r\n\x1a\n"), Err(DecodeError::Unsupported));
        assert_eq!(decode(&[]), Err(DecodeError::Unsupported));
        assert!(matches!(
            decode(&[0xFF, 0x0A, 0x00]),
            Err(DecodeError::Malformed(_))
        ));
        let mut boxed = CONTAINER_SIGNATURE.to_vec();
        boxed.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        assert!(matches!(decode(&boxed), Err(DecodeError::Malformed(_))));
    }

    #[test]
    fn cmyk_conversion_uses_inverted_ink() {
        let img = to_dynamic(PixelFormat::Cmyk, 1, 1, vec![255, 0, 255, 128]).unwrap();
        assert_eq!(img.to_rgba8().get_pixel(0, 0).0, [128, 0, 128, 255]);
    }
}
