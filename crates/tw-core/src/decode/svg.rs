//! SVG / SVGZ via resvg. Runs only in the worker: resvg cannot be cancelled, so its
//! time budget is enforced by the parent killing the worker process.

use std::sync::{Arc, LazyLock};

use image::{DynamicImage, RgbaImage};
use resvg::{tiny_skia, usvg};

use super::{DecodeError, Result, check_dimensions};

/// System fonts, scanned once per worker process and only for documents that need them
/// (the scan dominates the cost of rendering a text-free SVG).
static SYSTEM_FONTS: LazyLock<Arc<usvg::fontdb::Database>> = LazyLock::new(|| {
    let mut db = usvg::fontdb::Database::new();
    db.load_system_fonts();
    Arc::new(db)
});

/// Text needs fonts; gzip (SVGZ) can't be inspected cheaply, so it is assumed to.
fn may_contain_text(data: &[u8]) -> bool {
    data.starts_with(&[0x1f, 0x8b]) || data.windows(5).any(|w| w == b"<text")
}

/// Renders the document so its longer side equals `edge` pixels.
pub fn render(data: &[u8], edge: u32) -> Result<DynamicImage> {
    let fontdb = if may_contain_text(data) {
        SYSTEM_FONTS.clone()
    } else {
        Arc::new(usvg::fontdb::Database::new())
    };
    let opt = usvg::Options {
        fontdb,
        ..usvg::Options::default()
    };
    // `from_data_nested` ignores <image> elements that reference external files, so a
    // thumbnail can never pull other local files into its pixels. Inline data: images work.
    let tree = usvg::Tree::from_data_nested(data, &opt).map_err(|e| match e {
        usvg::Error::ElementsLimitReached => DecodeError::TooLarge,
        other => DecodeError::malformed(other),
    })?;
    let size = tree.size();
    let longest = size.width().max(size.height());
    if !(longest.is_finite() && longest > 0.0) {
        return Err(DecodeError::malformed("empty canvas"));
    }
    let scale = edge.max(1) as f32 / longest;
    let w = (size.width() * scale).round().max(1.0) as u32;
    let h = (size.height() * scale).round().max(1.0) as u32;
    check_dimensions(w, h)?;
    let mut pixmap = tiny_skia::Pixmap::new(w, h).ok_or(DecodeError::TooLarge)?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let img = RgbaImage::from_raw(w, h, pixmap.take_demultiplied())
        .ok_or_else(|| DecodeError::malformed("pixmap size"))?;
    Ok(DynamicImage::ImageRgba8(img))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_at_requested_edge_with_aspect() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="100">
            <rect width="200" height="100" fill="#ff0000"/></svg>"##;
        let img = render(svg, 64).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (64, 32));
        assert_eq!(img.get_pixel(10, 10).0, [255, 0, 0, 255]);
    }

    #[test]
    fn rejects_non_svg() {
        assert!(render(b"not svg at all", 64).is_err());
    }
}
