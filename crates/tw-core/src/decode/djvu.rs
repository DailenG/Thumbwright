//! DjVu (`.djv`, `.djvu`): the first page rendered at thumbnail size by the pure-Rust,
//! MIT-licensed `djvu-rs` crate; the embedded `TH44` thumbnail is the fallback when the
//! page itself cannot be rendered. The crate's own resource limits (page pixels, estimated
//! decoded bytes, render pixels) are set from our budgets and checked from the INFO
//! headers before any layer is decoded. Wavelet and JB2 decoding cannot be cancelled and
//! the crate is young, so this decoder runs in the worker.

use djvu_rs::djvu_render::{RenderOptions, RenderRequest};
use djvu_rs::{DjVuDocument, DjVuPage, ParseOptions, Pixmap, ResourceLimits};
use image::{DynamicImage, RgbaImage};

use super::{DecodeError, Result, check_dimensions};
use crate::limits;

pub const EXTENSIONS: &[&str] = &["djv", "djvu"];

/// IFF magic every DjVu file starts with.
const MAGIC: &[u8; 4] = b"AT&T";

/// Largest document page count accepted (INFO-bearing components).
const MAX_PAGES: u64 = 100_000;

fn resource_limits(edge: u32) -> ResourceLimits {
    ResourceLimits {
        max_pages: Some(MAX_PAGES),
        max_page_pixels: Some(limits::MAX_PIXEL_AREA),
        max_decoded_bytes: Some(limits::DECODE_ALLOC_BYTES),
        max_render_pixels: Some(u64::from(edge) * u64::from(edge)),
        ..ResourceLimits::default()
    }
}

fn map_doc_error(e: djvu_rs::DocError) -> DecodeError {
    match e {
        djvu_rs::DocError::ResourceLimit(_) => DecodeError::TooLarge,
        other => DecodeError::malformed(other),
    }
}

fn pixmap_to_image(pm: Pixmap) -> Result<DynamicImage> {
    check_dimensions(pm.width, pm.height)?;
    RgbaImage::from_raw(pm.width, pm.height, pm.data)
        .map(DynamicImage::ImageRgba8)
        .ok_or_else(|| DecodeError::malformed("DjVu pixmap length mismatch"))
}

fn render_page(page: &DjVuPage, limits: ResourceLimits, edge: u32) -> Result<DynamicImage> {
    let (w, h) = page.dimensions();
    // Never render larger than the page itself; the thumbnail step shrinks to `edge`.
    let side = edge.min(u32::from(w.max(h))).max(1);
    let opts = RenderOptions::fit_to_box(page, side, side);
    let pm = RenderRequest::new(opts)
        .limits(Some(limits))
        .pixmap(page)
        .map_err(|e| match e {
            djvu_rs::djvu_render::RenderError::ResourceLimit(_) => DecodeError::TooLarge,
            other => DecodeError::malformed(other),
        })?;
    pixmap_to_image(pm)
}

pub fn render(data: &[u8], edge: u32) -> Result<DynamicImage> {
    if !data.starts_with(MAGIC) {
        return Err(DecodeError::Unsupported);
    }
    let edge = edge.clamp(1, limits::DEFAULT_MAX_THUMB_EDGE);
    let limits = resource_limits(edge);
    let doc = DjVuDocument::parse_with_options(
        data,
        &ParseOptions {
            limits: Some(limits),
        },
    )
    .map_err(map_doc_error)?;
    let page = doc.page(0).map_err(map_doc_error)?;
    let (w, h) = page.dimensions();
    check_dimensions(u32::from(w), u32::from(h))?;
    match render_page(page, limits, edge) {
        Ok(img) => Ok(img),
        Err(DecodeError::TooLarge) => Err(DecodeError::TooLarge),
        Err(render_err) => match page.thumbnail() {
            Ok(Some(pm)) => pixmap_to_image(pm),
            _ => Err(render_err),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djvu_rs::djvu_encode::{EncodeQuality, PageEncoder};
    use djvu_rs::iw44_encode::Iw44EncodeOptions;

    /// A one-page DjVu through the crate's encoder: a smooth red-to-blue horizontal
    /// gradient with a green vertical ramp, as a pure wavelet background whose first
    /// chunk already carries colour (like files from DjVuLibre; the renderer decodes
    /// only the first chunk for strong downscales).
    fn gradient_page(w: u32, h: u32) -> Vec<u8> {
        let mut pm = Pixmap::try_new(w, h, 0, 0, 0, 255).unwrap();
        for y in 0..h {
            for x in 0..w {
                let t = (x * 255 / (w - 1)) as u8;
                let g = (y * 255 / (h - 1)) as u8;
                pm.set_rgb(x, y, 255 - t, g, t);
            }
        }
        PageEncoder::from_pixmap(&pm)
            .with_quality(EncodeQuality::Photo)
            .with_iw44_options(Iw44EncodeOptions {
                slices_per_chunk: 99,
                chroma_delay: 0,
                ..Iw44EncodeOptions::default()
            })
            .encode()
            .unwrap()
    }

    #[test]
    fn first_page_is_rendered_at_the_requested_edge() {
        let file = gradient_page(256, 128);
        let img = render(&file, 64).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (64, 32));
        let left = img.get_pixel(2, 2).0;
        let right = img.get_pixel(61, 2).0;
        assert!(
            i32::from(left[0]) > i32::from(left[2]) + 150,
            "left was {left:?}"
        );
        assert!(
            i32::from(right[2]) > i32::from(right[0]) + 150,
            "right was {right:?}"
        );
    }

    #[test]
    fn page_is_not_enlarged_beyond_its_size() {
        let file = gradient_page(40, 20);
        let img = render(&file, 512).unwrap();
        assert_eq!((img.width(), img.height()), (40, 20));
    }

    #[test]
    fn foreign_and_truncated_input_is_rejected() {
        assert_eq!(render(b"GIF89a....", 64), Err(DecodeError::Unsupported));
        assert_eq!(render(b"", 64), Err(DecodeError::Unsupported));
        let file = gradient_page(64, 64);
        assert!(matches!(
            render(&file[..8], 64),
            Err(DecodeError::Malformed(_))
        ));
        assert!(matches!(
            render(&file[..file.len() / 2], 64),
            Err(DecodeError::Malformed(_) | DecodeError::TooLarge)
        ));
    }

    #[test]
    fn huge_declared_page_size_is_too_large() {
        let mut file = gradient_page(64, 64);
        // INFO chunk: "INFO" + u32 length + width/height (u16 big endian) first.
        let at = file
            .windows(4)
            .position(|w| w == b"INFO")
            .expect("INFO chunk");
        file[at + 8..at + 10].copy_from_slice(&65535u16.to_be_bytes());
        file[at + 10..at + 12].copy_from_slice(&65535u16.to_be_bytes());
        assert_eq!(render(&file, 64), Err(DecodeError::TooLarge));
    }

    /// `gradient_page` with its first `BG44` chunk corrupted (zero image size in the IW44
    /// header) and a `TH44` thumbnail of a solid green image added after INFO.
    fn page_with_only_a_thumbnail() -> Vec<u8> {
        let mut file = gradient_page(64, 64);
        let bg = file.windows(4).position(|w| w == b"BG44").unwrap();
        // Payload: serial, slices, version (2), width (2), height (2), delay.
        file[bg + 8 + 4..bg + 8 + 8].fill(0);
        let info = file.windows(4).position(|w| w == b"INFO").unwrap();
        let info_len = u32::from_be_bytes(file[info + 4..info + 8].try_into().unwrap()) as usize;
        let at = info + 8 + info_len + (info_len & 1);
        let green = Pixmap::try_new(32, 32, 0, 255, 0, 255).unwrap();
        let mut th44 = Vec::new();
        for payload in djvu_rs::thumbnail::encode_th44_color(&green) {
            th44.extend_from_slice(b"TH44");
            th44.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            th44.extend_from_slice(&payload);
            if payload.len() % 2 == 1 {
                th44.push(0);
            }
        }
        file.splice(at..at, th44);
        let form_len = (file.len() - 12) as u32;
        file[8..12].copy_from_slice(&form_len.to_be_bytes());
        file
    }

    #[test]
    fn embedded_thumbnail_is_used_when_the_page_cannot_be_rendered() {
        let img = render(&page_with_only_a_thumbnail(), 64)
            .unwrap()
            .to_rgba8();
        assert_eq!(img.dimensions(), (32, 32));
        let p = img.get_pixel(16, 16).0;
        assert!(p[1] > 200 && p[0] < 60 && p[2] < 60, "pixel was {p:?}");
    }
}
