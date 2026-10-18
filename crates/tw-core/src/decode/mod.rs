//! Decoding pipeline: format routing (`route`), the individual decoders, and the
//! conversion of a decoded image into the premultiplied BGRA thumbnail the shell needs.
//!
//! Every decoder takes the whole file as a byte slice (the shell only has an `IStream`)
//! and returns an [`image::DynamicImage`]. Decoders MUST:
//! - check declared dimensions against [`check_dimensions`] before allocating pixels,
//! - cap decompressed sizes of embedded entries ([`crate::limits`]),
//! - return [`DecodeError::Unsupported`] when the input is simply not theirs, so the
//!   next tier can try.

pub mod android;
pub mod archive;
pub mod audio;
pub mod bitmap;
pub mod cad;
pub mod design;
pub mod djvu;
pub mod ebook;
pub mod exif;
pub mod j2k;
pub mod jxl;
pub mod livp;
#[cfg(windows)]
pub mod memstream;
pub mod mesh;
#[cfg(windows)]
pub mod metafile;
pub mod ole;
pub mod pcx;
#[cfg(windows)]
pub mod pdf;
pub mod probe;
pub mod psd;
pub mod raster;
pub mod raw;
pub mod route;
pub mod sci;
pub mod sgi;
pub mod svg;
#[cfg(windows)]
pub mod video;
#[cfg(windows)]
pub mod wic;
pub mod zip_preview;

use image::DynamicImage;

use crate::limits;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// No decoder in this tier accepts the input; the next tier may try.
    Unsupported,
    /// The input is damaged or uses an unsupported variant; the next tier may try.
    Malformed(String),
    /// Declared dimensions or expansion exceed the budgets. Never retried (hostile input).
    TooLarge,
    /// A time budget expired. Never retried (hostile input).
    Timeout,
}

impl DecodeError {
    /// REQ-TMO-1: only plain decode failures fall through to the next tier.
    pub fn falls_through(&self) -> bool {
        matches!(self, Self::Unsupported | Self::Malformed(_))
    }

    pub fn malformed(msg: impl std::fmt::Display) -> Self {
        Self::Malformed(msg.to_string())
    }
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => f.write_str("unsupported input"),
            Self::Malformed(m) => write!(f, "malformed input: {m}"),
            Self::TooLarge => f.write_str("input exceeds resource budget"),
            Self::Timeout => f.write_str("decode time budget exceeded"),
        }
    }
}

impl std::error::Error for DecodeError {}

pub type Result<T, E = DecodeError> = std::result::Result<T, E>;

impl From<image::ImageError> for DecodeError {
    fn from(e: image::ImageError) -> Self {
        match e {
            image::ImageError::Limits(_) => Self::TooLarge,
            image::ImageError::Unsupported(_) => Self::Unsupported,
            other => Self::Malformed(other.to_string()),
        }
    }
}

/// Rejects zero or over-budget dimensions before any pixel allocation.
pub fn check_dimensions(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        return Err(DecodeError::malformed("zero dimension"));
    }
    if u64::from(width) * u64::from(height) > limits::MAX_PIXEL_AREA {
        return Err(DecodeError::TooLarge);
    }
    Ok(())
}

/// Allocation limits applied to every `image` decoder.
pub fn image_limits() -> image::Limits {
    let mut l = image::Limits::default();
    l.max_alloc = Some(limits::DECODE_ALLOC_BYTES);
    l
}

/// Rejects a single decode buffer larger than the decode byte budget, before allocating.
pub fn check_alloc(bytes: u64) -> Result<()> {
    if bytes > limits::DECODE_ALLOC_BYTES {
        return Err(DecodeError::TooLarge);
    }
    Ok(())
}

/// Decodes `data` as `format` with [`image_limits`], reserving the whole output buffer
/// against the budget before it is allocated (as `ImageReader::decode` does, which
/// `DynamicImage::from_decoder` alone does not). Returns the decoder's EXIF orientation.
pub fn decode_limited(
    data: &[u8],
    format: image::ImageFormat,
) -> Result<(DynamicImage, Option<image::metadata::Orientation>)> {
    use image::ImageDecoder;

    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(data), format);
    reader.limits(image_limits());
    let mut decoder = reader.into_decoder()?;
    let (w, h) = decoder.dimensions();
    check_dimensions(w, h)?;
    let orientation = decoder.orientation().ok();
    let mut budget = image_limits();
    budget.reserve(decoder.total_bytes())?;
    decoder.set_limits(budget)?;
    Ok((DynamicImage::from_decoder(decoder)?, orientation))
}

/// Decodes any raster format the `image` crate recognises by content (used both for
/// standalone files and for previews embedded in containers).
pub fn decode_bytes(data: &[u8]) -> Result<DynamicImage> {
    let format = image::guess_format(data).map_err(|_| DecodeError::Unsupported)?;
    raster::decode_as(data, format)
}

/// Thumbnail pixels: premultiplied BGRA8, top-down rows, stride `width * 4`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// Target size when shrinking `w` x `h` to fit `edge` x `edge`: aspect kept, each side
/// rounded and at least 1 px. Exactly what `DynamicImage::thumbnail` produces, so both
/// resize paths in [`to_thumbnail`] agree on the output size.
fn shrunk_size(w: u32, h: u32, edge: u32) -> (u32, u32) {
    let ratio = f64::from(edge) / f64::from(w.max(h));
    let side = |n: u32| ((f64::from(n) * ratio).round() as u32).max(1);
    (side(w), side(h))
}

/// Box-filter shrink of an opaque 8-bit image (RGB or grey) straight to BGRA. `None` for
/// every other layout (alpha, 16-bit, float; see [`shrink_premultiplied`]), and if the
/// resizer rejects its input. Same quality as `DynamicImage::thumbnail` (measured
/// against a Lanczos-3 reference) and 3-8x faster at 24 MP: docs/performance.md.
fn shrink_opaque_8bit(img: &DynamicImage, edge: u32) -> Option<Thumbnail> {
    use fast_image_resize::images::{Image, ImageRef};
    use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

    let (pixel, raw, channels) = match img {
        DynamicImage::ImageRgb8(i) => (PixelType::U8x3, i.as_raw(), 3),
        DynamicImage::ImageLuma8(i) => (PixelType::U8, i.as_raw(), 1),
        _ => return None,
    };
    let (width, height) = shrunk_size(img.width(), img.height(), edge);
    let src = ImageRef::new(img.width(), img.height(), raw, pixel).ok()?;
    let mut dst = Image::new(width, height, pixel);
    let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Box));
    Resizer::new().resize(&src, &mut dst, &options).ok()?;
    let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
    for px in dst.buffer().chunks_exact(channels) {
        let [b, g, r] = if channels == 3 {
            [px[2], px[1], px[0]]
        } else {
            [px[0]; 3]
        };
        bgra.extend_from_slice(&[b, g, r, 255]);
    }
    Some(Thumbnail {
        width,
        height,
        bgra,
    })
}

/// Premultiplied BGRA from any decoded image, at its own size.
fn to_bgra(img: &DynamicImage) -> Thumbnail {
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    let mut bgra = rgba.into_raw();
    let (pixels, _) = bgra.as_chunks_mut::<4>();
    for px in pixels {
        let a = u16::from(px[3]);
        let premul = |c: u8| ((u16::from(c) * a + 127) / 255) as u8;
        *px = [premul(px[2]), premul(px[1]), premul(px[0]), px[3]];
    }
    Thumbnail {
        width,
        height,
        bgra,
    }
}

/// Box-filter shrink of an 8-bit image with alpha: premultiplied BGRA first, then the average
/// is taken over premultiplied pixels, which is how alpha blends correctly. Resizing straight
/// (non-premultiplied) colour lets the hidden colour under transparent pixels bleed into
/// edges (a green fringe around a logo whose transparent area happens to be green). `None`
/// for every other layout (opaque 8-bit goes through [`shrink_opaque_8bit`]; 16-bit and float
/// keep `DynamicImage::thumbnail`, because converting them to 8-bit first costs several
/// times the resize itself, measured in docs/performance.md).
///
/// Costs one full-size 4-bytes-per-pixel copy of the source, which the decode budget already
/// bounds ([`limits::DECODE_ALLOC_BYTES`]).
fn shrink_premultiplied(img: &DynamicImage, edge: u32) -> Option<Thumbnail> {
    use fast_image_resize::images::{Image, ImageRef};
    use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

    match img {
        DynamicImage::ImageRgba8(_) | DynamicImage::ImageLumaA8(_) => {}
        _ => return None,
    }
    let full = to_bgra(img);
    let (width, height) = shrunk_size(full.width, full.height, edge);
    let src = ImageRef::new(full.width, full.height, &full.bgra, PixelType::U8x4).ok()?;
    let mut dst = Image::new(width, height, PixelType::U8x4);
    // `use_alpha(false)`: the data is already premultiplied; the resizer must not do it again.
    let options = ResizeOptions::new()
        .resize_alg(ResizeAlg::Convolution(FilterType::Box))
        .use_alpha(false);
    Resizer::new().resize(&src, &mut dst, &options).ok()?;
    Some(Thumbnail {
        width,
        height,
        bgra: dst.into_vec(),
    })
}

/// Shrinks (never enlarges) so the longer edge is at most `edge`, then converts to
/// premultiplied BGRA.
///
/// Paths, fastest first: opaque 8-bit straight to BGRA ([`shrink_opaque_8bit`]); 8-bit with
/// alpha, premultiplied first ([`shrink_premultiplied`]); float, converted to 8-bit before
/// resizing because `DynamicImage::thumbnail` averages f32 samples unclamped and produces
/// wrong colours on them; 16-bit, resized as-is and converted afterwards (HDR photo sources;
/// the conversion would cost more than the resize, docs/performance.md section 4).
pub fn to_thumbnail(img: &DynamicImage, edge: u32) -> Thumbnail {
    let edge = edge.max(1);
    if img.width() <= edge && img.height() <= edge {
        return to_bgra(img);
    }
    if let Some(t) = shrink_opaque_8bit(img, edge) {
        return t;
    }
    if let Some(t) = shrink_premultiplied(img, edge) {
        return t;
    }
    // 16-bit and float layouts.
    if let DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_) = img {
        return to_bgra(&DynamicImage::ImageRgba8(img.to_rgba8()).thumbnail(edge, edge));
    }
    to_bgra(&img.thumbnail(edge, edge))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Luma, Rgb, RgbImage, Rgba, RgbaImage};

    #[test]
    fn thumbnail_premultiplies_and_swaps_channels() {
        let img = DynamicImage::ImageRgba8(RgbaImage::from_pixel(2, 1, Rgba([255, 128, 0, 128])));
        let t = to_thumbnail(&img, 256);
        assert_eq!((t.width, t.height), (2, 1));
        assert_eq!(&t.bgra[..4], &[0, 64, 128, 128]);
    }

    #[test]
    fn thumbnail_shrinks_long_edge_and_keeps_aspect() {
        let img = DynamicImage::ImageRgba8(RgbaImage::new(1000, 250));
        let t = to_thumbnail(&img, 100);
        assert_eq!((t.width, t.height), (100, 25));
        assert_eq!(t.bgra.len(), 100 * 25 * 4);
    }

    #[test]
    fn dimension_guard() {
        assert_eq!(check_dimensions(65535, 65535), Err(DecodeError::TooLarge));
        assert!(matches!(
            check_dimensions(0, 5),
            Err(DecodeError::Malformed(_))
        ));
        assert!(check_dimensions(20000, 20000).is_ok());
    }

    #[test]
    fn shrunk_size_matches_dynamic_image_thumbnail() {
        let cases = [
            (1000, 250, 100),
            (250, 1000, 100),
            (4000, 3, 256),
            (3, 4000, 256),
            (257, 256, 256),
            (256, 257, 256),
            (1025, 769, 256),
            (20000, 2, 1),
            (7, 5, 4),
        ];
        for (w, h, edge) in cases {
            let thumb = DynamicImage::ImageRgb8(RgbImage::new(w, h)).thumbnail(edge, edge);
            assert_eq!(
                shrunk_size(w, h, edge),
                (thumb.width(), thumb.height()),
                "{w}x{h} -> {edge}"
            );
        }
    }

    #[test]
    fn opaque_rgb_shrink_keeps_flat_colour_exact_and_the_aspect() {
        let img = DynamicImage::ImageRgb8(RgbImage::from_pixel(1000, 700, Rgb([200, 100, 50])));
        let t = to_thumbnail(&img, 64);
        assert_eq!((t.width, t.height), (64, 45));
        assert_eq!(t.bgra.len(), 64 * 45 * 4);
        assert!(
            t.bgra
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| *p == [50, 100, 200, 255])
        );
    }

    #[test]
    fn opaque_rgb_shrink_averages_fine_detail_instead_of_aliasing_it() {
        // 1 px black/white stripes shrunk 8:1 must come out as mid grey everywhere; a
        // point-sampling resize would return pure black or white.
        let stripes =
            RgbImage::from_fn(800, 800, |x, _| Rgb([if x % 2 == 0 { 0 } else { 255 }; 3]));
        let t = to_thumbnail(&DynamicImage::ImageRgb8(stripes), 100);
        assert_eq!((t.width, t.height), (100, 100));
        assert!(
            t.bgra
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| p[..3].iter().all(|c| c.abs_diff(127) <= 2) && p[3] == 255)
        );
    }

    #[test]
    fn greyscale_shrinks_to_opaque_grey_bgra() {
        let img = DynamicImage::ImageLuma8(ImageBuffer::from_pixel(1000, 500, Luma([77])));
        let t = to_thumbnail(&img, 100);
        assert_eq!((t.width, t.height), (100, 50));
        assert!(
            t.bgra
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| *p == [77, 77, 77, 255])
        );
    }

    #[test]
    fn transparent_pixels_do_not_bleed_their_hidden_colour_into_edges() {
        // Left half: opaque red. Right half: fully transparent, hidden colour pure green (what
        // an exporter leaves under a cut-out). 2000x2 shrunk to 100 puts the boundary inside a
        // box, so the one mixed pixel must be red at partial alpha with no green at all. A
        // straight (non-premultiplied) average would give a yellow-green fringe.
        let img = RgbaImage::from_fn(2000, 2, |x, _| {
            if x < 990 {
                Rgba([255, 0, 0, 255])
            } else {
                Rgba([0, 255, 0, 0])
            }
        });
        let t = to_thumbnail(&DynamicImage::ImageRgba8(img), 100);
        assert_eq!((t.width, t.height), (100, 1));
        let px: Vec<[u8; 4]> = t.bgra.as_chunks::<4>().0.to_vec();
        assert!(px.iter().all(|p| p[1] == 0), "green leaked: {px:?}");
        assert_eq!(px[0], [0, 0, 255, 255], "opaque red stays exact");
        assert_eq!(px[99], [0, 0, 0, 0], "transparent stays transparent");
        let mixed = px
            .iter()
            .find(|p| p[3] != 0 && p[3] != 255)
            .expect("one boundary pixel at partial alpha");
        assert!(
            mixed[2].abs_diff(mixed[3]) <= 1,
            "premultiplied red: {mixed:?}"
        );
    }

    #[test]
    fn grey_alpha_shrinks_premultiplied_and_float_keeps_the_generic_shrink() {
        // LumaA8 carries alpha, so it takes the premultiplied Box path like RGBA8.
        let la =
            DynamicImage::ImageLumaA8(ImageBuffer::from_pixel(1000, 500, image::LumaA([200, 100])));
        let t = to_thumbnail(&la, 100);
        assert_eq!((t.width, t.height), (100, 50));
        let want = ((200u16 * 100 + 127) / 255) as u8;
        assert!(
            t.bgra
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| p[..3] == [want; 3] && p[3] == 100)
        );

        // 16-bit and float layouts keep `DynamicImage::thumbnail` and are converted afterwards:
        // converting them to 8-bit first costs several times the resize itself (measured in
        // docs/performance.md section 6), and without alpha there is nothing to premultiply
        // before averaging.
        let float = DynamicImage::ImageRgba32F(ImageBuffer::from_pixel(
            1000,
            500,
            Rgba([1.0f32, 0.0, 0.0, 1.0]),
        ));
        let t = to_thumbnail(&float, 100);
        assert_eq!((t.width, t.height), (100, 50));
        assert!(
            t.bgra
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| *p == [0, 0, 255, 255])
        );
    }

    #[test]
    fn alpha_and_deep_layouts_keep_the_generic_shrink() {
        let half =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(1000, 500, Rgba([255, 128, 0, 128])));
        let t = to_thumbnail(&half, 100);
        assert_eq!((t.width, t.height), (100, 50));
        assert!(
            t.bgra
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| *p == [0, 64, 128, 128])
        );

        let deep =
            DynamicImage::ImageRgb16(ImageBuffer::from_pixel(1000, 500, Rgb([65535u16, 0, 0])));
        let t = to_thumbnail(&deep, 100);
        assert_eq!((t.width, t.height), (100, 50));
        assert!(
            t.bgra
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| *p == [0, 0, 255, 255])
        );
    }
}
