//! Video thumbnails: one representative frame, taken at a configurable percentage of the
//! duration, decoded by Media Foundation's source reader (FUNCTIONAL_SPEC.md 5.6).
//!
//! Media Foundation loads OS (C++) demuxers and codecs, so this module only ever runs in the
//! worker process (REQ-ISO-2); the caller must have initialised COM. The file is exposed to
//! Media Foundation through a borrowed in-memory `IStream` ([`super::memstream`]), and the
//! source resolver picks the container from a synthetic `file.<ext>` origin name.

use image::{DynamicImage, RgbaImage, imageops};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFAttributes, IMFMediaType, IMFSample, IMFSourceReader,
    MF_BYTESTREAM_ORIGIN_NAME, MF_E_INVALIDSTREAMNUMBER, MF_E_UNSUPPORTED_BYTESTREAM_TYPE,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_MINIMUM_DISPLAY_APERTURE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_VIDEO_ROTATION, MF_PD_DURATION,
    MF_SOURCE_READER_ALL_STREAMS, MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_SOURCE_READER_MEDIASOURCE,
    MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED, MF_SOURCE_READERF_ENDOFSTREAM,
    MF_SOURCE_READERF_ERROR, MF_VERSION, MFCreateAttributes, MFCreateMFByteStreamOnStream,
    MFCreateMediaType, MFCreateSourceReaderFromByteStream, MFMediaType_Video, MFSTARTUP_NOSOCKET,
    MFShutdown, MFStartup, MFVideoFormat_RGB32,
};
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PropVariantClear};
use windows::Win32::System::Variant::{VT_I8, VT_UI8};
use windows::core::{GUID, HSTRING, Interface};

use super::memstream::slice_stream;
use super::{DecodeError, Result, check_alloc, check_dimensions};

/// `ReadSample` calls allowed per seek target (stream ticks and gaps count).
const MAX_READS: u32 = 240;
/// Wall-clock allowance for advancing from the seek keyframe towards the target time.
const SKIP_BUDGET: std::time::Duration = std::time::Duration::from_secs(6);
/// 100 ns units per second.
const HNS: i64 = 10_000_000;

/// Starts Media Foundation for the lifetime of the value.
struct Session;

impl Session {
    fn start() -> Result<Self> {
        // SAFETY: plain startup call; balanced by `MFShutdown` in `Drop` only on success.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }.map_err(|e| mf_err("startup", &e))?;
        Ok(Self)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: balances the successful `MFStartup` in `start`.
        let _ = unsafe { MFShutdown() };
    }
}

fn mf_err(what: &str, e: &windows::core::Error) -> DecodeError {
    let code = e.code();
    if code == MF_E_UNSUPPORTED_BYTESTREAM_TYPE || code == MF_E_INVALIDSTREAMNUMBER {
        return DecodeError::Unsupported;
    }
    DecodeError::malformed(format!("Media Foundation {what}: {e}"))
}

/// Output frame layout reported by the source reader.
#[derive(Clone, Copy)]
struct Format {
    width: u32,
    height: u32,
    /// Row pitch in bytes; negative means bottom-up memory order.
    stride: i32,
    /// Pixel aspect ratio numerator / denominator (both non-zero).
    par: (u32, u32),
    /// Visible area `(x, y, width, height)` inside the coded frame (e.g. 1080 of 1088 rows).
    crop: Option<(u32, u32, u32, u32)>,
    /// Clockwise rotation in degrees (0, 90, 180, 270) the player must apply, from the
    /// container (phone video).
    rotation: u32,
}

/// A decoded sample with its presentation time and the layout it was produced in. Only the
/// frame finally chosen is converted to RGBA; skipped frames cost no pixel copies.
struct Frame {
    sample: IMFSample,
    time: i64,
    /// Half the frame duration: a frame this close before the target counts as reaching it.
    slack: i64,
    format: Format,
}

/// Extracts the frame at `percent` (0-100) of the duration of the first video stream.
pub fn extract(data: &[u8], ext: &str, percent: u8) -> Result<DynamicImage> {
    // Same ceiling as the worker request protocol: the stream and the OS decoders address
    // the file with 32-bit lengths in places.
    if u32::try_from(data.len()).is_err() {
        return Err(DecodeError::TooLarge);
    }
    let _session = Session::start()?;
    let (stream, _detach) = slice_stream(data);
    // SAFETY: COM calls on interfaces we own; the borrowed bytes outlive every object below
    // (`_detach` is dropped after the reader and the byte stream, and it severs the stream
    // from the slice, so a lingering OS thread cannot read freed memory).
    unsafe {
        let bytestream = MFCreateMFByteStreamOnStream(&stream).map_err(|e| mf_err("stream", &e))?;
        let attrs: IMFAttributes = bytestream.cast().map_err(|e| mf_err("stream attrs", &e))?;
        attrs
            .SetString(
                &MF_BYTESTREAM_ORIGIN_NAME,
                &HSTRING::from(format!("file.{}", sanitize_ext(ext))),
            )
            .map_err(|e| mf_err("origin name", &e))?;

        let mut config: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut config, 1).map_err(|e| mf_err("attributes", &e))?;
        let config = config.ok_or_else(|| DecodeError::malformed("Media Foundation attributes"))?;
        config
            .SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)
            .map_err(|e| mf_err("video processing", &e))?;
        let reader = MFCreateSourceReaderFromByteStream(&bytestream, &config)
            .map_err(|e| mf_err("source", &e))?;
        frame_from_reader(&reader, percent)
    }
}

/// Keeps the origin name to a plain short extension so the source resolver cannot be
/// steered by file-controlled punctuation.
fn sanitize_ext(ext: &str) -> String {
    let e: String = ext
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect();
    if e.is_empty() { "bin".into() } else { e }
}

const VIDEO_STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

unsafe fn frame_from_reader(reader: &IMFSourceReader, percent: u8) -> Result<DynamicImage> {
    // SAFETY: COM calls on a live reader; every out pointer is a local.
    unsafe {
        reader
            .SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)
            .map_err(|e| mf_err("stream select", &e))?;
        reader
            .SetStreamSelection(VIDEO_STREAM, true)
            .map_err(|e| mf_err("video stream select", &e))?;

        // Declared size of the native stream, checked before any decoder is built.
        let native = reader
            .GetNativeMediaType(VIDEO_STREAM, 0)
            .map_err(|e| mf_err("native type", &e))?;
        if let Ok(packed) = native.GetUINT64(&MF_MT_FRAME_SIZE) {
            check_dimensions((packed >> 32) as u32, packed as u32)?;
        }

        let want = MFCreateMediaType().map_err(|e| mf_err("media type", &e))?;
        want.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| mf_err("media type", &e))?;
        want.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
            .map_err(|e| mf_err("media type", &e))?;
        reader
            .SetCurrentMediaType(VIDEO_STREAM, None, &want)
            .map_err(|e| mf_err("RGB32 output (no decoder)", &e))?;
        let rotation = native.GetUINT32(&MF_MT_VIDEO_ROTATION).unwrap_or(0);
        let mut format = current_format(reader, rotation)?;

        let duration = duration_hns(reader).unwrap_or(0);
        let target = duration.saturating_mul(i64::from(percent)) / 100;
        let mut tried = Vec::with_capacity(3);
        for position in [target, target - 3 * HNS, 0] {
            let position = position.max(0);
            if tried.contains(&position) {
                continue;
            }
            tried.push(position);
            if let Some(frame) = read_near(reader, &mut format, position)? {
                let mut image = copy_sample(&frame.sample, &frame.format)?;
                if let Some((x, y, w, h)) = frame.format.crop {
                    image = imageops::crop_imm(&image, x, y, w, h).to_image();
                }
                return finish(image, frame.format.par)
                    .map(|img| rotate(img, frame.format.rotation));
            }
        }
        Err(DecodeError::malformed("no decodable video frame"))
    }
}

/// Seeks to `position` (a failed seek on a non-seekable source reads from the start) and
/// returns the decoded frame nearest to it: the seek lands on the keyframe at or before the
/// position, so frames are read forward while they are earlier than the target, bounded by
/// the sample count and `SKIP_BUDGET`. `None` means the stream ended without a frame.
unsafe fn read_near(
    reader: &IMFSourceReader,
    format: &mut Format,
    position: i64,
) -> Result<Option<Frame>> {
    // SAFETY: see callers; PROPVARIANT is cleared on every path.
    unsafe {
        let mut pv = PROPVARIANT::default();
        {
            let inner = &mut *pv.Anonymous.Anonymous;
            inner.vt = VT_I8;
            inner.Anonymous.hVal = position;
        }
        let seek = reader.SetCurrentPosition(&GUID::zeroed(), &pv);
        let _ = PropVariantClear(&mut pv);
        // A source that cannot seek is read from its start: the first frame is the answer.
        let seekable = seek.is_ok();

        let started = std::time::Instant::now();
        let mut best: Option<Frame> = None;
        for _ in 0..MAX_READS {
            let mut flags = 0u32;
            let mut time = 0i64;
            let mut sample: Option<IMFSample> = None;
            reader
                .ReadSample(
                    VIDEO_STREAM,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut time),
                    Some(&mut sample),
                )
                .map_err(|e| mf_err("read sample", &e))?;
            if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
                return Err(DecodeError::malformed("Media Foundation read error"));
            }
            if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
                *format = current_format(reader, format.rotation)?;
            }
            if let Some(sample) = sample {
                let slack = sample.GetSampleDuration().unwrap_or(0).max(0) / 2;
                best = Some(Frame {
                    sample,
                    time,
                    slack,
                    format: *format,
                });
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
            if let Some(f) = &best
                && (!seekable
                    || f.time.saturating_add(f.slack) >= position
                    || started.elapsed() > SKIP_BUDGET)
            {
                break;
            }
        }
        Ok(best)
    }
}

unsafe fn current_format(reader: &IMFSourceReader, native_rotation: u32) -> Result<Format> {
    // SAFETY: COM calls on a live reader.
    unsafe {
        let mt = reader
            .GetCurrentMediaType(VIDEO_STREAM)
            .map_err(|e| mf_err("current type", &e))?;
        let packed = mt
            .GetUINT64(&MF_MT_FRAME_SIZE)
            .map_err(|e| mf_err("frame size", &e))?;
        let (width, height) = ((packed >> 32) as u32, packed as u32);
        check_dimensions(width, height)?;
        let row = u64::from(width) * 4;
        check_alloc(row * u64::from(height))?;
        // Absent attribute: RGB32 defaults to a bottom-up DIB layout. The stride is a signed
        // 32-bit value stored in a UINT32.
        let stride = mt
            .GetUINT32(&MF_MT_DEFAULT_STRIDE)
            .map_or(-(row as i32), |s| s as i32);
        let par = mt
            .GetUINT64(&MF_MT_PIXEL_ASPECT_RATIO)
            .map(|p| ((p >> 32) as u32, p as u32))
            .ok()
            .filter(|&(n, d)| n > 0 && d > 0)
            .unwrap_or((1, 1));
        let rotation = mt
            .GetUINT32(&MF_MT_VIDEO_ROTATION)
            .unwrap_or(native_rotation);
        Ok(Format {
            width,
            height,
            stride,
            par,
            crop: display_aperture(&mt, width, height),
            rotation: if matches!(rotation, 90 | 180 | 270) {
                rotation
            } else {
                0
            },
        })
    }
}

/// The minimum display aperture (`MFVideoArea`: two fixed-point offsets and a size) when it
/// is a proper sub-rectangle of the coded frame.
unsafe fn display_aperture(
    mt: &IMFMediaType,
    width: u32,
    height: u32,
) -> Option<(u32, u32, u32, u32)> {
    let mut blob = [0u8; 16];
    let mut got = 0u32;
    // SAFETY: COM call with a local buffer of the stated size.
    unsafe { mt.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, &mut blob, Some(&mut got)) }.ok()?;
    if got as usize != blob.len() {
        return None;
    }
    let i16_at = |at: usize| i16::from_le_bytes([blob[at], blob[at + 1]]);
    let i32_at =
        |at: usize| i32::from_le_bytes([blob[at], blob[at + 1], blob[at + 2], blob[at + 3]]);
    let (x, y) = (
        u32::try_from(i16_at(2)).ok()?,
        u32::try_from(i16_at(6)).ok()?,
    );
    let (w, h) = (
        u32::try_from(i32_at(8)).ok()?,
        u32::try_from(i32_at(12)).ok()?,
    );
    let fits = w > 0 && h > 0 && x.checked_add(w)? <= width && y.checked_add(h)? <= height;
    (fits && (w, h) != (width, height)).then_some((x, y, w, h))
}

unsafe fn duration_hns(reader: &IMFSourceReader) -> Option<i64> {
    // SAFETY: COM call; the PROPVARIANT is read by its tag and then cleared.
    unsafe {
        let mut pv = reader
            .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
            .ok()?;
        let inner = &*pv.Anonymous.Anonymous;
        let value = if inner.vt == VT_UI8 {
            i64::try_from(inner.Anonymous.uhVal).ok()
        } else if inner.vt == VT_I8 {
            Some(inner.Anonymous.hVal)
        } else {
            None
        };
        let _ = PropVariantClear(&mut pv);
        value.filter(|d| *d > 0)
    }
}

/// Copies a sample's pixels into a top-down RGBA image (opaque), honouring the pitch sign.
unsafe fn copy_sample(sample: &IMFSample, format: &Format) -> Result<RgbaImage> {
    let (w, h) = (format.width as usize, format.height as usize);
    let row_bytes = w * 4;
    // SAFETY: COM call on a live sample.
    let buffer =
        unsafe { sample.ConvertToContiguousBuffer() }.map_err(|e| mf_err("sample buffer", &e))?;
    let mut out = vec![0u8; row_bytes * h];

    // Rows are addressed as `base + r * pitch` for top-down output row `r`; `base` is the
    // address of the top row. A 2-D buffer reports that directly; for a plain buffer a
    // negative stride puts the top row last in memory.
    let mut copy_rows = |top: *const u8, pitch: isize, available: usize| -> Result<()> {
        let span = (pitch.unsigned_abs())
            .checked_mul(h - 1)
            .and_then(|s| s.checked_add(row_bytes))
            .ok_or(DecodeError::TooLarge)?;
        if pitch.unsigned_abs() < row_bytes || span > available {
            return Err(DecodeError::malformed("video frame buffer too small"));
        }
        for (r, dst) in out.chunks_exact_mut(row_bytes).enumerate() {
            // SAFETY: row `r` lies inside the `span` bytes validated against the buffer
            // length above (for negative pitch `top` is the highest-addressed row start).
            let src =
                unsafe { std::slice::from_raw_parts(top.offset(pitch * r as isize), row_bytes) };
            dst.copy_from_slice(src);
        }
        Ok(())
    };

    if let Ok(buf2d) = buffer.cast::<IMF2DBuffer>() {
        let mut top: *mut u8 = std::ptr::null_mut();
        let mut pitch = 0i32;
        // SAFETY: Lock2D/Unlock2D pair on a live buffer; the pointer is only used between.
        unsafe {
            buf2d
                .Lock2D(&mut top, &mut pitch)
                .map_err(|e| mf_err("lock frame", &e))?;
            let available = buf2d.GetContiguousLength().unwrap_or(0) as usize;
            let r = if top.is_null() {
                Err(DecodeError::malformed("null frame buffer"))
            } else {
                copy_rows(top, pitch as isize, available)
            };
            let _ = buf2d.Unlock2D();
            r?;
        }
    } else {
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut len = 0u32;
        // SAFETY: Lock/Unlock pair on a live buffer; the pointer is only used between.
        unsafe {
            buffer
                .Lock(&mut ptr, None, Some(&mut len))
                .map_err(|e| mf_err("lock frame", &e))?;
            let pitch = format.stride as isize;
            let r = if ptr.is_null() || pitch == 0 {
                Err(DecodeError::malformed("null frame buffer"))
            } else {
                // Bottom-up: the top image row starts at the last row slot of the buffer.
                // The address is only formed here; `copy_rows` validates the whole span
                // against `len` before reading.
                let top = if pitch > 0 {
                    ptr.cast_const()
                } else {
                    let last = pitch.unsigned_abs().saturating_mul(h - 1);
                    ptr.cast_const().wrapping_add(last)
                };
                copy_rows(top, pitch, len as usize)
            };
            let _ = buffer.Unlock();
            r?;
        }
    }

    // RGB32 carries no alpha: the fourth byte is unspecified, so force opaque; swap B and R.
    for px in out.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
        px[3] = 255;
    }
    RgbaImage::from_raw(format.width, format.height, out)
        .ok_or_else(|| DecodeError::malformed("frame buffer size"))
}

/// Applies the pixel aspect ratio so non-square-pixel video (e.g. anamorphic DVD) has the
/// proportions it is displayed with.
fn finish(image: RgbaImage, par: (u32, u32)) -> Result<DynamicImage> {
    let (n, d) = par;
    if n == d || n > d.saturating_mul(8) || d > n.saturating_mul(8) {
        return Ok(DynamicImage::ImageRgba8(image));
    }
    let (w, h) = (u64::from(image.width()), u64::from(image.height()));
    let (nw, nh) = if n > d {
        (w * u64::from(n) / u64::from(d), h)
    } else {
        (w, h * u64::from(d) / u64::from(n))
    };
    let (nw, nh) = (
        u32::try_from(nw.max(1)).map_err(|_| DecodeError::TooLarge)?,
        u32::try_from(nh.max(1)).map_err(|_| DecodeError::TooLarge)?,
    );
    check_dimensions(nw, nh)?;
    check_alloc(u64::from(nw) * u64::from(nh) * 4)?;
    Ok(DynamicImage::ImageRgba8(imageops::resize(
        &image,
        nw,
        nh,
        imageops::FilterType::Triangle,
    )))
}

/// Applies the container's clockwise display rotation.
fn rotate(img: DynamicImage, degrees: u32) -> DynamicImage {
    match degrees {
        90 => img.rotate90(),
        180 => img.rotate180(),
        270 => img.rotate270(),
        _ => img,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

    fn com() {
        // SAFETY: per-thread COM initialisation; the worker uses the same apartment model.
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    }

    fn chunk(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
        out.extend_from_slice(id);
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(body);
        if body.len() % 2 == 1 {
            out.push(0);
        }
    }

    fn list(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
        out.extend_from_slice(b"LIST");
        out.extend_from_slice(&(body.len() as u32 + 4).to_le_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
    }

    fn u32s(vals: &[u32]) -> Vec<u8> {
        vals.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    /// Uncompressed 24-bit AVI, 10 fps. Frame `i` has a top half of RGB `(i * 10, 0, 200)`
    /// and a bottom half of RGB `(0, 200, 0)`; rows are stored bottom-up as DIBs are.
    fn avi_rgb24(width: u32, height: u32, frames: u32) -> Vec<u8> {
        let row = (width * 3).next_multiple_of(4) as usize;
        let image_size = (row as u32).wrapping_mul(height);
        let mut avih = u32s(&[100_000, 0, 0, 0x10, frames, 0, 1, image_size, width, height]);
        avih.extend_from_slice(&[0u8; 16]);
        let mut strh = b"vidsDIB ".to_vec();
        strh.extend_from_slice(&u32s(&[0, 0, 0, 1, 10, 0, frames, image_size, u32::MAX, 0]));
        strh.extend_from_slice(&[0u8; 8]); // rcFrame
        let mut strf = u32s(&[40, width, height]);
        strf.extend_from_slice(&1u16.to_le_bytes());
        strf.extend_from_slice(&24u16.to_le_bytes());
        strf.extend_from_slice(&u32s(&[0, image_size, 0, 0, 0, 0]));
        let mut strl = Vec::new();
        chunk(&mut strl, b"strh", &strh);
        chunk(&mut strl, b"strf", &strf);
        let mut hdrl = Vec::new();
        chunk(&mut hdrl, b"avih", &avih);
        list(&mut hdrl, b"strl", &strl);

        let mut movi = Vec::new();
        let mut index = Vec::new();
        for i in 0..frames {
            let mut data = Vec::with_capacity(image_size as usize);
            for y in 0..height {
                // DIB row 0 is the bottom of the picture.
                let top_half = y >= height / 2;
                let (r, g, b) = if top_half {
                    (i * 10, 0, 200)
                } else {
                    (0, 200, 0)
                };
                for _ in 0..width {
                    data.extend_from_slice(&[b as u8, g as u8, r as u8]);
                }
                data.resize(data.len() + row - (width * 3) as usize, 0);
            }
            index.extend_from_slice(b"00db");
            index.extend_from_slice(&u32s(&[0x10, 4 + movi.len() as u32, image_size]));
            chunk(&mut movi, b"00db", &data);
        }
        let mut body = Vec::new();
        list(&mut body, b"hdrl", &hdrl);
        list(&mut body, b"movi", &movi);
        chunk(&mut body, b"idx1", &index);
        let mut avi = b"RIFF".to_vec();
        avi.extend_from_slice(&(body.len() as u32 + 4).to_le_bytes());
        avi.extend_from_slice(b"AVI ");
        avi.extend_from_slice(&body);
        avi
    }

    fn red_of_top_left(img: &DynamicImage) -> u8 {
        img.to_rgba8().get_pixel(1, 1).0[0]
    }

    #[test]
    fn frame_comes_from_the_requested_offset_with_upright_orientation() {
        com();
        let avi = avi_rgb24(64, 48, 20);
        let first = extract(&avi, "avi", 0).unwrap();
        assert_eq!((first.width(), first.height()), (64, 48));
        let px = first.to_rgba8();
        // Top half is the varying colour (red = frame * 10), bottom half the green: the image
        // is upright, not bottom-up, and fully opaque.
        assert_eq!(px.get_pixel(5, 5).0, [0, 0, 200, 255]);
        assert_eq!(px.get_pixel(5, 42).0, [0, 200, 0, 255]);

        let mid = red_of_top_left(&extract(&avi, "avi", 50).unwrap());
        let last = red_of_top_left(&extract(&avi, "avi", 100).unwrap());
        assert!((80..=110).contains(&mid), "50% -> red {mid}");
        assert!((150..=190).contains(&last), "100% -> red {last}");
        // Larger values are clamped by the caller; the decoder itself never reads past the end.
        assert!(red_of_top_left(&extract(&avi, "avi", 30).unwrap()) < mid);
    }

    #[test]
    fn hostile_video_input_fails_without_panic_or_big_allocation() {
        com();
        // Garbage with a video extension.
        assert!(extract(&[0u8; 64], "mp4", 30).is_err());
        assert!(extract(b"", "mkv", 30).is_err());
        // Truncated container: header only.
        let avi = avi_rgb24(64, 48, 20);
        assert!(extract(&avi[..200], "avi", 30).is_err());
        // Declared 65535 x 65535 frame (4.3 Gpx) with no payload.
        let huge = avi_rgb24(65535, 65535, 0);
        assert!(matches!(
            extract(&huge[..huge.len().min(4096)], "avi", 30),
            Err(DecodeError::TooLarge | DecodeError::Malformed(_) | DecodeError::Unsupported)
        ));
    }

    #[test]
    fn origin_name_extension_is_sanitised() {
        assert_eq!(sanitize_ext("mp4"), "mp4");
        assert_eq!(sanitize_ext("a/b\\c.d"), "abcd");
        assert_eq!(sanitize_ext(""), "bin");
        assert_eq!(sanitize_ext("0123456789abc"), "01234567");
    }

    #[test]
    fn rotation_is_clockwise_and_swaps_dimensions() {
        let mut img = RgbaImage::new(3, 2);
        img.put_pixel(0, 0, image::Rgba([9, 0, 0, 255])); // top-left marker
        let img = DynamicImage::ImageRgba8(img);
        let r = rotate(img.clone(), 90).to_rgba8();
        assert_eq!((r.width(), r.height()), (2, 3));
        // Clockwise: the top-left corner moves to the top-right.
        assert_eq!(r.get_pixel(1, 0).0, [9, 0, 0, 255]);
        let r = rotate(img.clone(), 180).to_rgba8();
        assert_eq!(r.get_pixel(2, 1).0, [9, 0, 0, 255]);
        assert_eq!(rotate(img, 0).width(), 3);
    }

    #[test]
    fn pixel_aspect_ratio_stretches_the_short_axis() {
        let img = RgbaImage::from_pixel(4, 2, image::Rgba([1, 2, 3, 255]));
        assert_eq!(finish(img.clone(), (1, 1)).unwrap().width(), 4);
        let wide = finish(img.clone(), (2, 1)).unwrap();
        assert_eq!((wide.width(), wide.height()), (8, 2));
        let tall = finish(img.clone(), (1, 2)).unwrap();
        assert_eq!((tall.width(), tall.height()), (4, 4));
        // Absurd ratios are ignored rather than allocating huge frames.
        assert_eq!(finish(img, (1000, 1)).unwrap().width(), 4);
    }
}
