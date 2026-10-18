//! Windows metafile thumbnails (EMF, WMF, placeable WMF, and their gzip-compressed `.emz` /
//! `.wmz` forms): played back through GDI onto a white 32-bit DIB section whose longer edge
//! is `edge`.
//!
//! GDI metafile playback is C++ and can be made to burn CPU or memory by hostile record
//! streams, so this module only ever runs in the worker process under the Job Object
//! (REQ-ISO-2). The file kind is decided from its content, never from the extension.

use std::io::Read;

use flate2::read::GzDecoder;
use image::{DynamicImage, RgbaImage};
use windows::Win32::Foundation::{HANDLE, RECT};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
    DeleteDC, DeleteEnhMetaFile, DeleteObject, ENHMETAHEADER, GdiFlush, GetEnhMetaFileHeader,
    HBITMAP, HDC, HENHMETAFILE, HGDIOBJ, PlayEnhMetaFile, SelectObject, SetEnhMetaFileBits,
};
use windows::Win32::System::DataExchange::{METAFILEPICT, SetWinMetaFileBits};

use super::{DecodeError, Result, check_alloc, check_dimensions};

/// Inflated size cap for `.emz`/`.wmz`. Real metafiles are kilobytes to a few megabytes;
/// `ARCHIVE_ENTRY_MAX_BYTES` would not fit the worker's memory ceiling once the growing
/// buffer's reallocation is counted, so the cap is lower.
const MAX_INFLATED_BYTES: u64 = 64 << 20;

const WMF_PLACEABLE_KEY: u32 = 0x9AC6_CDD7;
const EMF_SIGNATURE: u32 = 0x464D_4520;
const PLACEABLE_HEADER_LEN: usize = 22;
const MM_ANISOTROPIC: i32 = 8;

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    Emf,
    /// Placeable WMF: the size/scale header precedes the standard METAHEADER.
    WmfPlaceable,
    /// Plain WMF (no bounds in the file).
    Wmf,
}

fn u16_at(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        d.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn u32_at(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        d.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn i16_at(d: &[u8], at: usize) -> Option<i16> {
    u16_at(d, at).map(|v| v as i16)
}

fn classify(d: &[u8]) -> Option<Kind> {
    if u32_at(d, 0) == Some(1) && u32_at(d, 40) == Some(EMF_SIGNATURE) {
        return Some(Kind::Emf);
    }
    if u32_at(d, 0) == Some(WMF_PLACEABLE_KEY) {
        return Some(Kind::WmfPlaceable);
    }
    // METAHEADER: type (1 memory / 2 disk), header size in words (9), version.
    if matches!(u16_at(d, 0), Some(1 | 2))
        && u16_at(d, 2) == Some(9)
        && matches!(u16_at(d, 4), Some(0x0100 | 0x0300))
    {
        return Some(Kind::Wmf);
    }
    None
}

/// Inflates a gzip stream, capped per entry; `None` when `data` is not gzip.
fn gunzip(data: &[u8]) -> Result<Option<Vec<u8>>> {
    if !data.starts_with(&[0x1F, 0x8B]) {
        return Ok(None);
    }
    let limit = MAX_INFLATED_BYTES;
    let mut out = Vec::new();
    GzDecoder::new(data)
        .take(limit + 1)
        .read_to_end(&mut out)
        .map_err(|e| DecodeError::malformed(format!("metafile gzip: {e}")))?;
    if out.len() as u64 > limit {
        return Err(DecodeError::TooLarge);
    }
    Ok(Some(out))
}

/// Plays the metafile onto a white bitmap whose longer edge is `edge`.
pub fn render(data: &[u8], edge: u32) -> Result<DynamicImage> {
    let inflated = gunzip(data)?;
    let data = inflated.as_deref().unwrap_or(data);
    let kind = classify(data).ok_or(DecodeError::Unsupported)?;
    if u32::try_from(data.len()).is_err() {
        return Err(DecodeError::TooLarge);
    }
    // SAFETY: GDI calls on handles created and owned below; every handle is released by its
    // guard on all paths, and pointers handed to GDI are valid for the stated sizes.
    unsafe { play(data, &kind, edge.max(1)) }
}

struct Dc(HDC);
impl Drop for Dc {
    fn drop(&mut self) {
        // SAFETY: handle created by `CreateCompatibleDC`, freed exactly once.
        let _ = unsafe { DeleteDC(self.0) };
    }
}

struct Bitmap(HBITMAP);
impl Drop for Bitmap {
    fn drop(&mut self) {
        // SAFETY: handle created by `CreateDIBSection`, freed exactly once (after it has been
        // deselected from its DC, which is dropped later in declaration order).
        let _ = unsafe { DeleteObject(HGDIOBJ(self.0.0)) };
    }
}

struct Emf(HENHMETAFILE);
impl Drop for Emf {
    fn drop(&mut self) {
        // SAFETY: handle created by `SetEnhMetaFileBits`/`SetWinMetaFileBits`, freed once.
        let _ = unsafe { DeleteEnhMetaFile(Some(self.0)) };
    }
}

unsafe fn play(data: &[u8], kind: &Kind, edge: u32) -> Result<DynamicImage> {
    // SAFETY: see `render`.
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return Err(DecodeError::malformed("GDI: no device context"));
        }
        let dc = Dc(dc);

        let emf = match kind {
            Kind::Emf => SetEnhMetaFileBits(data),
            Kind::Wmf => SetWinMetaFileBits(data, Some(dc.0), None),
            Kind::WmfPlaceable => {
                let body = data
                    .get(PLACEABLE_HEADER_LEN..)
                    .ok_or_else(|| DecodeError::malformed("truncated WMF"))?;
                let pict = placeable_pict(data)?;
                SetWinMetaFileBits(body, Some(dc.0), Some(&pict))
            }
        };
        if emf.is_invalid() {
            return Err(DecodeError::malformed("GDI rejected the metafile"));
        }
        let emf = Emf(emf);

        let mut header = ENHMETAHEADER::default();
        let size = size_of::<ENHMETAHEADER>() as u32;
        if GetEnhMetaFileHeader(emf.0, size, Some(&raw mut header)) == 0 {
            return Err(DecodeError::malformed("GDI: no metafile header"));
        }
        let (w, h) = target_size(&header, edge)?;
        check_dimensions(w, h)?;
        let bytes = u64::from(w) * u64::from(h) * 4;
        check_alloc(bytes)?;

        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                // Negative: top-down rows.
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let section = CreateDIBSection(
            Some(dc.0),
            &info,
            DIB_RGB_COLORS,
            &mut bits,
            None::<HANDLE>,
            0,
        )
        .map_err(|e| DecodeError::malformed(format!("GDI bitmap: {e}")))?;
        let section = Bitmap(section);
        if bits.is_null() {
            return Err(DecodeError::malformed("GDI bitmap has no pixels"));
        }
        let len = bytes as usize;
        // Metafiles are documents drawn on an implicit white page.
        std::ptr::write_bytes(bits.cast::<u8>(), 0xFF, len);

        let previous = SelectObject(dc.0, HGDIOBJ(section.0.0));
        let frame = RECT {
            left: 0,
            top: 0,
            right: w as i32,
            bottom: h as i32,
        };
        let played = PlayEnhMetaFile(dc.0, emf.0, &frame);
        let _ = GdiFlush();
        SelectObject(dc.0, previous);
        if !played.as_bool() {
            return Err(DecodeError::malformed("GDI could not play the metafile"));
        }

        let mut pixels = std::slice::from_raw_parts(bits.cast::<u8>(), len).to_vec();
        for px in pixels.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
            px[3] = 255;
        }
        let img = RgbaImage::from_raw(w, h, pixels)
            .ok_or_else(|| DecodeError::malformed("metafile buffer size"))?;
        Ok(DynamicImage::ImageRgba8(img))
    }
}

/// `METAFILEPICT` for a placeable WMF: anisotropic mapping with the suggested size in
/// HIMETRIC (0.01 mm) derived from the header's bounding box and units per inch.
fn placeable_pict(d: &[u8]) -> Result<METAFILEPICT> {
    let (l, t, r, b) = (
        i16_at(d, 6).ok_or(DecodeError::Unsupported)?,
        i16_at(d, 8).ok_or(DecodeError::Unsupported)?,
        i16_at(d, 10).ok_or(DecodeError::Unsupported)?,
        i16_at(d, 12).ok_or(DecodeError::Unsupported)?,
    );
    // 0 is invalid; 1440 (twips) is the conventional default.
    let inch = match u16_at(d, 14) {
        Some(0) | None => 1440,
        Some(n) => i64::from(n),
    };
    let himetric = |a: i16, z: i16| {
        let units = i64::from(z).abs_diff(i64::from(a)) as i64;
        i32::try_from(units * 2540 / inch).unwrap_or(i32::MAX)
    };
    let (x, y) = (himetric(l, r), himetric(t, b));
    if x == 0 || y == 0 {
        return Err(DecodeError::malformed("WMF has an empty bounding box"));
    }
    Ok(METAFILEPICT {
        mm: MM_ANISOTROPIC,
        xExt: x,
        yExt: y,
        hMF: Default::default(),
    })
}

/// Output size: the metafile's frame (0.01 mm) decides the aspect ratio, falling back to the
/// device bounds; the longer side becomes `edge`.
fn target_size(h: &ENHMETAHEADER, edge: u32) -> Result<(u32, u32)> {
    let span = |a: i32, b: i32| i64::from(b).abs_diff(i64::from(a)) as i64 + 1;
    let (mut fw, mut fh) = (
        i64::from(h.rclFrame.right) - i64::from(h.rclFrame.left),
        i64::from(h.rclFrame.bottom) - i64::from(h.rclFrame.top),
    );
    if fw <= 0 || fh <= 0 {
        fw = span(h.rclBounds.left, h.rclBounds.right);
        fh = span(h.rclBounds.top, h.rclBounds.bottom);
    }
    if fw <= 0 || fh <= 0 {
        return Err(DecodeError::malformed("metafile has no extent"));
    }
    let longer = fw.max(fh) as f64;
    let px = |v: i64| (((v as f64) * f64::from(edge) / longer).round() as u32).clamp(1, edge);
    Ok((px(fw), px(fh)))
}

#[cfg(test)]
pub(in crate::decode) mod tests {
    use super::*;
    use std::io::Write;
    use windows::Win32::Foundation::COLORREF;
    use windows::Win32::Graphics::Gdi::{
        CloseEnhMetaFile, CreateEnhMetaFileW, CreateSolidBrush, GetEnhMetaFileBits, Rectangle,
    };

    /// 2000 x 1000 (0.01 mm) EMF with a red rectangle over the left half, recorded with GDI.
    pub(in crate::decode) fn emf_with_red_left_half() -> Vec<u8> {
        // SAFETY: GDI recording DC and brush created and released here.
        unsafe {
            let frame = RECT {
                left: 0,
                top: 0,
                right: 2000,
                bottom: 1000,
            };
            let dc = CreateEnhMetaFileW(None, None, Some(&frame), None);
            let brush = CreateSolidBrush(COLORREF(0x0000_00FF)); // BGR: red
            let old = SelectObject(dc, HGDIOBJ(brush.0));
            // Logical units are device pixels of the reference DC; draw in a unit-less way by
            // covering the metafile's full height and left half with a large rectangle.
            let _ = Rectangle(dc, -5, -5, 10_000, 100_000);
            SelectObject(dc, old);
            let _ = DeleteObject(HGDIOBJ(brush.0));
            let emf = CloseEnhMetaFile(dc);
            let n = GetEnhMetaFileBits(emf, None);
            let mut bytes = vec![0u8; n as usize];
            GetEnhMetaFileBits(emf, Some(&mut bytes));
            let _ = DeleteEnhMetaFile(Some(emf));
            bytes
        }
    }

    fn word(v: &mut Vec<u8>, w: u16) {
        v.extend_from_slice(&w.to_le_bytes());
    }

    fn record(v: &mut Vec<u8>, func: u16, params: &[u16]) {
        let size = 3 + params.len() as u32;
        v.extend_from_slice(&size.to_le_bytes());
        word(v, func);
        for p in params {
            word(v, *p);
        }
    }

    /// Placeable WMF, 100x50 units at 100 per inch, with a blue rectangle over the left half.
    pub(in crate::decode) fn placeable_wmf() -> Vec<u8> {
        let mut records = Vec::new();
        record(&mut records, 0x020B, &[0, 0]); // SetWindowOrg(y, x)
        record(&mut records, 0x020C, &[50, 100]); // SetWindowExt(y, x)
        // CreateBrushIndirect: style SOLID, COLORREF 0x00FF0000 (blue), hatch.
        record(&mut records, 0x02FC, &[0, 0x0000, 0x00FF, 0]);
        record(&mut records, 0x012D, &[0]); // SelectObject(0)
        record(&mut records, 0x041B, &[50, 50, 0, 0]); // Rectangle(bottom, right, top, left)
        record(&mut records, 0x0000, &[]); // EOF

        let total_words = (18 + records.len()) / 2;
        let mut head = Vec::new();
        word(&mut head, 1); // memory metafile
        word(&mut head, 9);
        word(&mut head, 0x0300);
        head.extend_from_slice(&(total_words as u32).to_le_bytes());
        word(&mut head, 1); // objects
        head.extend_from_slice(&6u32.to_le_bytes()); // largest record (words)
        word(&mut head, 0);

        let mut placeable = Vec::new();
        placeable.extend_from_slice(&WMF_PLACEABLE_KEY.to_le_bytes());
        word(&mut placeable, 0); // hmf
        for v in [0i16, 0, 100, 50] {
            word(&mut placeable, v as u16);
        }
        word(&mut placeable, 100); // units per inch
        placeable.extend_from_slice(&0u32.to_le_bytes());
        let checksum = placeable
            .as_chunks::<2>()
            .0
            .iter()
            .fold(0u16, |a, c| a ^ u16::from_le_bytes(*c));
        word(&mut placeable, checksum);
        assert_eq!(placeable.len(), PLACEABLE_HEADER_LEN);

        placeable.extend_from_slice(&head);
        placeable.extend_from_slice(&records);
        placeable
    }

    fn rgb(img: &DynamicImage, x: u32, y: u32) -> [u8; 3] {
        let p = img.to_rgba8().get_pixel(x, y).0;
        [p[0], p[1], p[2]]
    }

    #[test]
    fn emf_renders_at_the_requested_edge_with_correct_colours() {
        let img = render(&emf_with_red_left_half(), 64).unwrap();
        assert_eq!((img.width(), img.height()), (64, 32));
        assert_eq!(rgb(&img, 2, 16), [255, 0, 0]);
    }

    #[test]
    fn placeable_wmf_renders_with_its_bounding_box_aspect() {
        let img = render(&placeable_wmf(), 100).unwrap();
        assert_eq!((img.width(), img.height()), (100, 50));
        assert_eq!(rgb(&img, 10, 25), [0, 0, 255]);
        // The right half is the white page.
        assert_eq!(rgb(&img, 90, 25), [255, 255, 255]);
    }

    #[test]
    fn gzip_compressed_metafile_is_inflated_and_rendered() {
        let raw = emf_with_red_left_half();
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&raw).unwrap();
        let emz = enc.finish().unwrap();
        let img = render(&emz, 40).unwrap();
        assert_eq!((img.width(), img.height()), (40, 20));
    }

    #[test]
    fn content_not_extension_decides_and_hostile_input_fails_cleanly() {
        assert_eq!(render(b"", 64), Err(DecodeError::Unsupported));
        assert_eq!(
            render(b"MZ\x90\x00 not a metafile", 64),
            Err(DecodeError::Unsupported)
        );
        // Truncated EMF: header kind is recognised, GDI must reject or play it, never crash.
        let emf = emf_with_red_left_half();
        assert!(render(&emf[..60], 64).is_err());
        // Header declaring a huge frame still yields a bounded bitmap (longer edge = edge).
        let mut huge = emf.clone();
        huge[32..36].copy_from_slice(&i32::MAX.to_le_bytes());
        huge[36..40].copy_from_slice(&i32::MAX.to_le_bytes());
        if let Ok(img) = render(&huge, 64) {
            assert!(img.width() <= 64 && img.height() <= 64);
        }
        // Gzip bomb: inflated size beyond the entry cap is refused, not materialised.
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let chunk = vec![0u8; 1 << 20];
        for _ in 0..(MAX_INFLATED_BYTES >> 20) + 2 {
            enc.write_all(&chunk).unwrap();
        }
        let bomb = enc.finish().unwrap();
        assert_eq!(render(&bomb, 64), Err(DecodeError::TooLarge));
        // A corrupt gzip body is malformed.
        assert!(matches!(
            render(&[0x1F, 0x8B, 8, 0, 0, 0, 0, 0, 0, 0, 1, 2], 64),
            Err(DecodeError::Malformed(_))
        ));
    }

    #[test]
    fn target_size_follows_frame_then_bounds_and_never_collapses() {
        let mut h = ENHMETAHEADER::default();
        h.rclFrame.right = 20_000;
        h.rclFrame.bottom = 10_000;
        assert_eq!(target_size(&h, 200).unwrap(), (200, 100));
        h.rclFrame.right = 100_000_000;
        h.rclFrame.bottom = 1;
        assert_eq!(target_size(&h, 64).unwrap(), (64, 1));
        let mut h = ENHMETAHEADER::default();
        h.rclBounds.right = 9;
        h.rclBounds.bottom = 4;
        assert_eq!(target_size(&h, 100).unwrap(), (100, 50));
    }
}
