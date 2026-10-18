//! JPEG XL via `jxl-oxide`: the image header is parsed first so the declared size can be
//! checked before any frame is decoded; the first keyframe is rendered to 8-bit samples.

use image::{DynamicImage, GrayAlphaImage, GrayImage, RgbImage, RgbaImage};
use jxl_oxide::{
    AllocTracker, EnumColourEncoding, InitializeResult, JxlImage, JxlThreadPool, PixelFormat,
    RenderingIntent,
};

use super::{DecodeError, Result, check_alloc, check_dimensions};
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
    decode_with(data, true)
}

fn decode_with(data: &[u8], to_srgb: bool) -> Result<DynamicImage> {
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
    // Wide-gamut and ICC-described images are converted to sRGB (what Explorer assumes);
    // HDR transfer curves are left alone since clipping them to SDR would blow out.
    let to_srgb = to_srgb
        && matches!(image.pixel_format(), PixelFormat::Rgb | PixelFormat::Rgba)
        && !image.image_header().metadata.colour_encoding.is_srgb()
        && image.hdr_type().is_none();
    if to_srgb {
        image.request_color_encoding(EnumColourEncoding::srgb(RenderingIntent::Relative));
    }
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
    let render = match image.render_frame(0) {
        Ok(render) => render,
        // ICC profiles that cannot be reduced to an enumerated encoding need a CMS, which
        // this build does not have: render in the image's own colour space instead.
        Err(_) if to_srgb => return decode_with(data, false),
        Err(e) => return Err(malformed(e)),
    };
    let mut stream = render.stream();
    let (w, h) = (stream.width(), stream.height());
    check_dimensions(w, h)?;
    let channels = stream.channels() as usize;
    if channels != format.channels() {
        return Err(DecodeError::malformed("JPEG XL channel layout mismatch"));
    }
    let samples = w as usize * h as usize * channels;
    // jxl-oxide leaves associated (premultiplied) alpha premultiplied; `image` expects
    // straight alpha. Render at 16 bits so dividing the alpha back out keeps precision.
    let premultiplied = matches!(
        format,
        PixelFormat::Graya | PixelFormat::Rgba | PixelFormat::Cmyka
    ) && image
        .image_header()
        .metadata
        .ec_info
        .iter()
        .find_map(|ec| ec.alpha_associated())
        == Some(true);
    let buf = if premultiplied {
        check_alloc(samples as u64 * 2)?;
        let mut wide = vec![0u16; samples];
        if stream.write_to_buffer(&mut wide) != samples {
            return Err(DecodeError::malformed("JPEG XL render incomplete"));
        }
        unpremultiply(&wide, channels)
    } else {
        let mut buf = vec![0u8; samples];
        if stream.write_to_buffer(&mut buf) != samples {
            return Err(DecodeError::malformed("JPEG XL render incomplete"));
        }
        buf
    };
    to_dynamic(format, w, h, buf)
}

/// 16-bit premultiplied samples (alpha last) to 8-bit straight alpha.
fn unpremultiply(wide: &[u16], channels: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(wide.len());
    for px in wide.chunks_exact(channels) {
        let (colour, alpha) = px.split_at(channels - 1);
        let a = u32::from(alpha[0]);
        for &c in colour {
            out.push(
                (u32::from(c) * 255 + a / 2)
                    .checked_div(a)
                    .map_or(0, |v| v.min(255) as u8),
            );
        }
        out.push(((a * 255 + 32767) / 65535) as u8);
    }
    out
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

    #[test]
    fn unpremultiply_divides_alpha_out_without_overflow() {
        // Half-covered full red, fully transparent (colour dropped), opaque, and a
        // rounding-error pixel whose colour slightly exceeds its alpha.
        let wide = [
            32768, 0, 0, 32768, 9, 9, 9, 0, 0, 65535, 0, 65535, 70, 0, 0, 65,
        ];
        assert_eq!(
            unpremultiply(&wide, 4),
            [255, 0, 0, 128, 0, 0, 0, 0, 0, 255, 0, 255, 255, 0, 0, 0]
        );
    }
}
