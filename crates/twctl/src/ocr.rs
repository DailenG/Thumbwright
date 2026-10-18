//! Text recognition through the Windows built-in OCR engine (`Windows.Media.Ocr`). The
//! engine is an OS component, so it is used here in `twctl` only, never in the shell DLL.

use image::DynamicImage;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};

use crate::ops::Result;

/// One recognised word with its box in the coordinates of the image given to [`recognize`].
#[derive(Clone, Debug)]
pub struct Word {
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Debug)]
pub struct Recognized {
    pub text: String,
    pub words: Vec<Word>,
}

fn err(e: windows::core::Error) -> String {
    format!("OCR: {e}")
}

/// Composites onto white and returns straight BGRA bytes (OCR reads opaque pixels).
fn bgra_on_white(img: &DynamicImage) -> (Vec<u8>, u32, u32) {
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut out = Vec::with_capacity(rgba.len());
    for px in rgba.pixels() {
        let a = u32::from(px[3]);
        let blend = |c: u8| ((u32::from(c) * a + 255 * (255 - a) + 127) / 255) as u8;
        out.extend_from_slice(&[blend(px[2]), blend(px[1]), blend(px[0]), 255]);
    }
    (out, w, h)
}

fn recognize_here(img: &DynamicImage) -> Result<Recognized> {
    let engine = OcrEngine::TryCreateFromUserProfileLanguages().map_err(|_| {
        "OCR: no Windows OCR language pack is installed for your languages \
         (Settings > Time & language > Language & region)"
            .to_string()
    })?;
    let max = OcrEngine::MaxImageDimension().map_err(err)?;
    let (orig_w, orig_h) = (img.width(), img.height());
    let img = if img.width().max(img.height()) > max {
        img.resize(max, max, image::imageops::FilterType::Triangle)
    } else {
        img.clone()
    };
    let (bytes, w, h) = bgra_on_white(&img);
    let writer = DataWriter::new().map_err(err)?;
    writer.WriteBytes(&bytes).map_err(err)?;
    let buffer = writer.DetachBuffer().map_err(err)?;
    let bitmap = SoftwareBitmap::CreateCopyFromBuffer(
        &buffer,
        BitmapPixelFormat::Bgra8,
        i32::try_from(w).map_err(|e| e.to_string())?,
        i32::try_from(h).map_err(|e| e.to_string())?,
    )
    .map_err(err)?;
    let result = engine
        .RecognizeAsync(&bitmap)
        .map_err(err)?
        .join()
        .map_err(err)?;
    // Boxes come back in the (possibly shrunk) image; report them in the caller's pixels.
    let (kx, ky) = (
        f64::from(orig_w) / f64::from(w),
        f64::from(orig_h) / f64::from(h),
    );
    let mut words = Vec::new();
    for line in result.Lines().map_err(err)? {
        for word in line.Words().map_err(err)? {
            let r = word.BoundingRect().map_err(err)?;
            words.push(Word {
                text: word.Text().map_err(err)?.to_string(),
                x: f64::from(r.X) * kx,
                y: f64::from(r.Y) * ky,
                w: f64::from(r.Width) * kx,
                h: f64::from(r.Height) * ky,
            });
        }
    }
    Ok(Recognized {
        text: result.Text().map_err(err)?.to_string(),
        words,
    })
}

/// Recognises text in `img`. The engine runs on a dedicated MTA thread, because its async
/// operations are awaited synchronously.
pub fn recognize(img: &DynamicImage) -> Result<Recognized> {
    std::thread::scope(|s| {
        s.spawn(|| {
            // SAFETY: initialises WinRT for this thread only; it ends with the thread.
            let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
            recognize_here(img)
        })
        .join()
        .unwrap_or_else(|_| Err("OCR: the engine panicked".into()))
    })
}
