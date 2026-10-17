//! Windows Imaging Component decoder (OS codecs: HEIF/HEVC, AVIF/AV1, JPEG XR, RAW
//! extension, and anything else installed). These codecs are C++, so this module is only
//! ever called inside the worker process (REQ-ISO-2). The caller must have initialised COM.

use image::{DynamicImage, RgbaImage};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppRGBA, IWICBitmapSource, IWICImagingFactory,
    WICBitmapInterpolationModeFant, WICConvertBitmapSource, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};

use super::{DecodeError, Result, check_dimensions};

/// Decodes the first frame, scaling inside WIC so that the longer edge is at most `edge`
/// (full-resolution pixels are never materialised in one buffer).
pub fn decode(data: &[u8], edge: u32) -> Result<DynamicImage> {
    // SAFETY: plain COM calls on interfaces we own; `data` outlives the WIC stream, which
    // is dropped before this function returns.
    unsafe { decode_inner(data, edge.max(1)) }.map_err(|e| match e {
        Inner::Decode(d) => d,
        Inner::Com(c) => DecodeError::malformed(format!("WIC: {c}")),
    })
}

enum Inner {
    Decode(DecodeError),
    Com(windows::core::Error),
}

impl From<windows::core::Error> for Inner {
    fn from(e: windows::core::Error) -> Self {
        Self::Com(e)
    }
}

impl From<DecodeError> for Inner {
    fn from(e: DecodeError) -> Self {
        Self::Decode(e)
    }
}

unsafe fn decode_inner(data: &[u8], edge: u32) -> std::result::Result<DynamicImage, Inner> {
    // SAFETY: see `decode`.
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let stream = factory.CreateStream()?;
        stream.InitializeFromMemory(data)?;
        let decoder = factory
            .CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)
            .map_err(|_| DecodeError::Unsupported)?;
        let frame = decoder.GetFrame(0)?;
        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h)?;
        check_dimensions(w, h)?;

        let mut source: IWICBitmapSource = frame.into();
        if w > edge || h > edge {
            let scale = edge as f64 / w.max(h) as f64;
            let (sw, sh) = (
                ((w as f64 * scale).round() as u32).max(1),
                ((h as f64 * scale).round() as u32).max(1),
            );
            let scaler = factory.CreateBitmapScaler()?;
            scaler.Initialize(&source, sw, sh, WICBitmapInterpolationModeFant)?;
            source = scaler.into();
            (w, h) = (sw, sh);
        }
        let rgba = WICConvertBitmapSource(&GUID_WICPixelFormat32bppRGBA, &source)?;
        let stride = w * 4;
        let mut buf = vec![0u8; stride as usize * h as usize];
        rgba.CopyPixels(std::ptr::null(), stride, &mut buf)?;
        let img = RgbaImage::from_raw(w, h, buf).ok_or(DecodeError::malformed("buffer size"))?;
        Ok(DynamicImage::ImageRgba8(img))
    }
}
