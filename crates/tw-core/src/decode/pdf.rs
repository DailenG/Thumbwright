//! PDF (and PDF-compatible Illustrator) thumbnails: page 1 rendered by the OS PDF engine
//! (`Windows.Data.Pdf`), white background, longer edge `edge`.
//!
//! The OS renderer is C++ and cannot be cancelled, so this module only ever runs in the
//! worker process (REQ-ISO-2); the caller must have initialised COM (MTA), which is what
//! WinRT activation and the blocking `get()` on async operations need.

use image::DynamicImage;
use windows::Data::Pdf::{PdfDocument, PdfPageRenderOptions};
use windows::Storage::Streams::{DataReader, IRandomAccessStream, InMemoryRandomAccessStream};
use windows::UI::Color;
use windows::Win32::System::WinRT::{BSOS_DEFAULT, CreateRandomAccessStreamOverStream};

use super::memstream::slice_stream;
use super::{DecodeError, Result, check_alloc, check_dimensions, decode_bytes};

// `.ai` files written since Illustrator CS are PDF documents; older PostScript-based ones
// fail the `%PDF-` check below and are rejected as unsupported.

/// Readers accept the header anywhere in the first KiB (junk before `%PDF-`).
const HEADER_WINDOW: usize = 1024;
/// Largest PNG the renderer may hand back (the pixel area is bounded separately).
const MAX_PNG_BYTES: u32 = 256 << 20;

fn pdf_err(what: &str, e: &windows::core::Error) -> DecodeError {
    DecodeError::malformed(format!("PDF {what}: {e}"))
}

/// Renders page 1 so that its longer edge is `edge` pixels.
pub fn render(data: &[u8], edge: u32) -> Result<DynamicImage> {
    let head = &data[..data.len().min(HEADER_WINDOW)];
    if !head.windows(5).any(|w| w == b"%PDF-") {
        return Err(DecodeError::Unsupported);
    }
    let edge = edge.max(1);
    let (stream, _detach) = slice_stream(data);
    // SAFETY: WinRT/COM calls on interfaces we own. The borrowed bytes outlive the document
    // and the stream wrapper (`_detach` is dropped last and severs the stream from the
    // slice, so a lingering OS thread cannot read freed memory).
    let png = unsafe {
        let input: IRandomAccessStream = CreateRandomAccessStreamOverStream(&stream, BSOS_DEFAULT)
            .map_err(|e| pdf_err("stream", &e))?;
        let doc = PdfDocument::LoadFromStreamAsync(&input)
            .and_then(|op| op.join())
            .map_err(|e| pdf_err("load (damaged or password protected)", &e))?;
        render_page(&doc, edge)?
    };
    decode_bytes(&png)
}

fn render_page(doc: &PdfDocument, edge: u32) -> Result<Vec<u8>> {
    if doc.PageCount().map_err(|e| pdf_err("page count", &e))? == 0 {
        return Err(DecodeError::malformed("PDF has no pages"));
    }
    let page = doc.GetPage(0).map_err(|e| pdf_err("page 1", &e))?;
    let size = page.Size().map_err(|e| pdf_err("page size", &e))?;
    let (dw, dh) = fit(size.Width, size.Height, edge)?;
    check_dimensions(dw, dh)?;
    check_alloc(u64::from(dw) * u64::from(dh) * 4)?;

    let options = PdfPageRenderOptions::new().map_err(|e| pdf_err("options", &e))?;
    options
        .SetDestinationWidth(dw)
        .and_then(|()| options.SetDestinationHeight(dh))
        .and_then(|()| {
            options.SetBackgroundColor(Color {
                A: 255,
                R: 255,
                G: 255,
                B: 255,
            })
        })
        .map_err(|e| pdf_err("options", &e))?;

    let out = InMemoryRandomAccessStream::new().map_err(|e| pdf_err("output", &e))?;
    page.RenderWithOptionsToStreamAsync(&out, &options)
        .and_then(|a| a.join())
        .map_err(|e| pdf_err("render", &e))?;
    let size = out.Size().map_err(|e| pdf_err("output", &e))?;
    let len = u32::try_from(size)
        .ok()
        .filter(|n| *n > 0 && *n <= MAX_PNG_BYTES)
        .ok_or(DecodeError::TooLarge)?;
    let reader =
        DataReader::CreateDataReader(&out.GetInputStreamAt(0).map_err(|e| pdf_err("output", &e))?)
            .map_err(|e| pdf_err("output", &e))?;
    reader
        .LoadAsync(len)
        .and_then(|op| op.join())
        .map_err(|e| pdf_err("output", &e))?;
    let mut png = vec![0u8; len as usize];
    reader
        .ReadBytes(&mut png)
        .map_err(|e| pdf_err("output", &e))?;
    Ok(png)
}

/// Destination pixel size for a page of `w` x `h` points whose longer edge becomes `edge`.
fn fit(w: f32, h: f32, edge: u32) -> Result<(u32, u32)> {
    if !(w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0) {
        return Err(DecodeError::malformed("PDF page has no usable size"));
    }
    let scale = f64::from(edge) / f64::from(w.max(h));
    let px = |v: f32| ((f64::from(v) * scale).round() as u32).clamp(1, edge);
    Ok((px(w), px(h)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_scales_the_longer_edge_and_keeps_aspect() {
        assert_eq!(fit(612.0, 792.0, 256).unwrap(), (198, 256));
        assert_eq!(fit(792.0, 612.0, 256).unwrap(), (256, 198));
        // Extreme aspect ratios keep at least one pixel on the short side.
        assert_eq!(fit(100_000.0, 1.0, 64).unwrap(), (64, 1));
        assert!(fit(0.0, 10.0, 64).is_err());
        assert!(fit(f32::NAN, 10.0, 64).is_err());
        assert!(fit(f32::INFINITY, 10.0, 64).is_err());
    }

    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

    fn com() {
        // SAFETY: per-thread COM initialisation; the worker uses the same apartment model.
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    }

    /// One-page PDF with the given MediaBox: a red square over the left half of the page and
    /// the word "Hi" in Helvetica on the right half. Offsets in the xref table are computed.
    fn minimal_pdf(width: u32, height: u32) -> Vec<u8> {
        let content = format!(
            "1 0 0 rg 0 0 {} {} re f 0 g BT /F1 24 Tf {} {} Td (Hi) Tj ET",
            width / 2,
            height,
            width / 2 + 10,
            height / 2
        );
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {width} {height}] /Contents 4 0 R \
                 /Resources << /Font << /F1 5 0 R >> >> >>"
            ),
            format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            ),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for off in offsets {
            pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    #[test]
    fn first_page_renders_at_edge_with_content_on_white() {
        com();
        let img = render(&minimal_pdf(200, 100), 100).unwrap().to_rgba8();
        assert_eq!((img.width(), img.height()), (100, 50));
        assert_eq!(img.get_pixel(10, 25).0, [255, 0, 0, 255]);
        // Right half: white page with dark text pixels somewhere.
        assert_eq!(img.get_pixel(95, 2).0, [255, 255, 255, 255]);
        let dark = (50..100)
            .flat_map(|x| (0..50).map(move |y| (x, y)))
            .filter(|&(x, y)| img.get_pixel(x, y).0[0] < 128)
            .count();
        assert!(dark > 5, "expected glyph pixels, found {dark}");
    }

    #[test]
    fn tall_page_scales_the_height_to_the_edge() {
        com();
        let img = render(&minimal_pdf(100, 400), 80).unwrap();
        assert_eq!((img.width(), img.height()), (20, 80));
    }

    #[test]
    fn hostile_pdfs_fail_cleanly_and_stay_bounded() {
        com();
        let pdf = minimal_pdf(200, 100);
        // Truncated at several points (header only, mid-object, before the xref).
        for cut in [9, 40, pdf.len() / 2, pdf.len() - 30] {
            let _ = render(&pdf[..cut], 64);
        }
        assert!(render(&pdf[..9], 64).is_err());
        assert!(render(b"%PDF-1.4\n\x00\x01\x02garbage", 64).is_err());
        // A page declaring a 1,000,000-point square is rendered at the requested edge only.
        let huge = render(&minimal_pdf(1_000_000, 1_000_000), 64).unwrap();
        assert_eq!((huge.width(), huge.height()), (64, 64));
        // An absurd edge request is refused by the dimension budget before rendering.
        assert!(render(&pdf, u32::MAX).is_err());
    }

    #[test]
    fn non_pdf_input_is_unsupported_without_touching_the_os() {
        assert_eq!(
            render(b"%!PS-Adobe-3.0 EPSF", 64),
            Err(DecodeError::Unsupported)
        );
        assert_eq!(render(b"", 64), Err(DecodeError::Unsupported));
    }
}
