//! Photoshop documents (`.psd`, `.psb` large document, `.pdd`, `.psdt` template).
//!
//! The image-resources thumbnail (resource 1036, or the legacy BGR resource 1033) is
//! preferred because it is tiny and fast; otherwise the merged composite in the image
//! data section is decoded (raw or PackBits, 1/8/16-bit bitmap, grey, duotone,
//! multichannel, indexed, RGB and CMYK). Lab and 32-bit composites are not supported.

use image::{DynamicImage, GrayImage, RgbImage};

use super::{DecodeError, Result, check_alloc, check_dimensions, decode_bytes};

pub const EXTENSIONS: &[&str] = &["psd", "psb", "pdd", "psdt"];

const SIGNATURE: &[u8] = b"8BPS";
const HEADER_LEN: usize = 26;
const RES_THUMBNAIL: u16 = 1036;
const RES_THUMBNAIL_LEGACY: u16 = 1033;
const THUMB_HEADER_LEN: usize = 28;
const THUMB_FORMAT_RAW: u32 = 0;
const THUMB_FORMAT_JPEG: u32 = 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Bitmap,
    Gray,
    Indexed,
    Rgb,
    Cmyk,
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: u64) -> Result<&'a [u8]> {
        let n = usize::try_from(n).map_err(|_| truncated())?;
        let end = self.pos.checked_add(n).ok_or_else(truncated)?;
        let s = self.data.get(self.pos..end).ok_or_else(truncated)?;
        self.pos = end;
        Ok(s)
    }

    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_be_bytes(a))
    }

    fn rest(&self) -> &'a [u8] {
        self.data.get(self.pos..).unwrap_or_default()
    }
}

fn truncated() -> DecodeError {
    DecodeError::malformed("PSD data truncated")
}

pub fn extract(data: &[u8]) -> Result<DynamicImage> {
    if !data.starts_with(SIGNATURE) {
        return Err(DecodeError::Unsupported);
    }
    let mut r = Reader { data, pos: 4 };
    let version = r.u16()?;
    if !matches!(version, 1 | 2) {
        return Err(DecodeError::malformed(format!(
            "unknown PSD version {version}"
        )));
    }
    let large = version == 2;
    r.take(6)?; // reserved
    let channels = r.u16()?;
    let height = r.u32()?;
    let width = r.u32()?;
    let depth = r.u16()?;
    let color_mode = r.u16()?;
    debug_assert_eq!(r.pos, HEADER_LEN);

    let len = r.u32()?;
    let color_data = r.take(u64::from(len))?;
    let len = r.u32()?;
    let resources = r.take(u64::from(len))?;

    if let Some(result) = thumbnail(resources) {
        match result {
            Ok(img) => return Ok(img),
            Err(e) if !e.falls_through() => return Err(e),
            Err(_) => {} // damaged thumbnail: fall back to the composite
        }
    }

    let len = if large { r.u64()? } else { u64::from(r.u32()?) };
    r.take(len)?; // layer and mask information

    let mode = match (color_mode, depth) {
        (0, 1) => Mode::Bitmap,
        (1 | 7 | 8, _) => Mode::Gray,
        (2, 8) => Mode::Indexed,
        (3, _) => Mode::Rgb,
        (4, _) => Mode::Cmyk,
        _ => {
            return Err(DecodeError::malformed(format!(
                "unsupported PSD colour mode {color_mode} at depth {depth}"
            )));
        }
    };
    if !matches!(depth, 1 | 8 | 16) || (depth == 1) != (mode == Mode::Bitmap) {
        return Err(DecodeError::malformed(format!(
            "unsupported PSD depth {depth}"
        )));
    }
    let palette = if mode == Mode::Indexed {
        Some(
            color_data
                .get(..768)
                .ok_or_else(|| DecodeError::malformed("PSD palette missing"))?,
        )
    } else {
        None
    };
    let planes = match mode {
        Mode::Bitmap | Mode::Gray | Mode::Indexed => 1,
        Mode::Rgb => 3,
        Mode::Cmyk => 4,
    };
    if usize::from(channels) < planes {
        return Err(DecodeError::malformed(
            "PSD has fewer channels than its colour mode needs",
        ));
    }
    check_dimensions(width, height)?;
    let mut composite = Composite::new(mode, width, height, depth, palette);
    composite.read(&mut r, large, usize::from(channels), planes)?;
    composite.finish()
}

/// Finds the preferred thumbnail resource and decodes it.
fn thumbnail(resources: &[u8]) -> Option<Result<DynamicImage>> {
    let mut r = Reader {
        data: resources,
        pos: 0,
    };
    let mut legacy = None;
    while r.rest().len() >= 12 {
        let Ok(sig) = r.take(4) else { break };
        if !matches!(sig, b"8BIM" | b"MeSa" | b"AgHg" | b"PHUT" | b"DCSR") {
            break;
        }
        let Ok(id) = r.u16() else { break };
        // Pascal name padded to an even total length.
        let Some(&name_len) = r.rest().first() else {
            break;
        };
        let name_total = (u64::from(name_len) + 1 + 1) & !1;
        if r.take(name_total).is_err() {
            break;
        }
        let Ok(size) = r.u32() else { break };
        let Ok(payload) = r.take(u64::from(size)) else {
            break;
        };
        if size % 2 == 1 {
            // Padding byte; may be missing at the very end of the section.
            let _ = r.take(1);
        }
        match id {
            RES_THUMBNAIL => return Some(decode_thumbnail(payload, false)),
            RES_THUMBNAIL_LEGACY => legacy = Some(payload),
            _ => {}
        }
    }
    legacy.map(|p| decode_thumbnail(p, true))
}

fn decode_thumbnail(payload: &[u8], bgr: bool) -> Result<DynamicImage> {
    let mut r = Reader {
        data: payload,
        pos: 0,
    };
    let format = r.u32()?;
    let width = r.u32()?;
    let height = r.u32()?;
    let width_bytes = r.u32()?;
    let body = payload.get(THUMB_HEADER_LEN..).ok_or_else(truncated)?;
    let img = match format {
        THUMB_FORMAT_JPEG => decode_bytes(body)?,
        THUMB_FORMAT_RAW => {
            check_dimensions(width, height)?;
            let stride = width_bytes as usize;
            let (w, h) = (width as usize, height as usize);
            if stride < w * 3 || body.len() / stride < h {
                return Err(truncated());
            }
            let mut out = Vec::with_capacity(w * h * 3);
            for row in body.chunks_exact(stride).take(h) {
                out.extend_from_slice(&row[..w * 3]);
            }
            DynamicImage::ImageRgb8(RgbImage::from_raw(width, height, out).ok_or_else(truncated)?)
        }
        other => {
            return Err(DecodeError::malformed(format!(
                "unknown PSD thumbnail format {other}"
            )));
        }
    };
    if !bgr {
        return Ok(img);
    }
    let mut rgb = img.to_rgb8();
    for px in rgb.pixels_mut() {
        px.0.swap(0, 2);
    }
    Ok(DynamicImage::ImageRgb8(rgb))
}

/// Merged-image accumulator: planar rows in, interleaved 8-bit pixels out.
struct Composite<'a> {
    mode: Mode,
    width: u32,
    height: u32,
    depth: u16,
    palette: Option<&'a [u8]>,
    out: Vec<u8>,
    samples: Vec<u8>,
}

impl<'a> Composite<'a> {
    /// Pixel buffers are allocated by [`Self::alloc`] only after the image data has been
    /// shown to be present, so a tiny file cannot force a huge allocation.
    fn new(mode: Mode, width: u32, height: u32, depth: u16, palette: Option<&'a [u8]>) -> Self {
        Self {
            mode,
            width,
            height,
            depth,
            palette,
            out: Vec::new(),
            samples: Vec::new(),
        }
    }

    fn alloc(&mut self) -> Result<()> {
        let out_channels = match self.mode {
            Mode::Bitmap | Mode::Gray => 1,
            Mode::Indexed | Mode::Rgb | Mode::Cmyk => 3,
        };
        let px = u64::from(self.width) * u64::from(self.height);
        check_alloc(px * out_channels)?;
        self.out = vec![0u8; px as usize * out_channels as usize];
        self.samples = vec![0u8; self.width as usize];
        Ok(())
    }

    fn row_bytes(&self) -> usize {
        let w = self.width as usize;
        match self.depth {
            1 => w.div_ceil(8),
            16 => w * 2,
            _ => w,
        }
    }

    fn read(
        &mut self,
        r: &mut Reader<'_>,
        large: bool,
        channels: usize,
        planes: usize,
    ) -> Result<()> {
        let compression = r.u16()?;
        let h = self.height as usize;
        let row_bytes = self.row_bytes();
        match compression {
            0 => {
                let needed = (planes * h) as u64 * row_bytes as u64;
                let body = r.take(needed)?;
                self.alloc()?;
                for (i, row) in body.chunks_exact(row_bytes).enumerate() {
                    self.put_row(i / h, i % h, row);
                }
                Ok(())
            }
            1 => {
                let entry = if large { 4 } else { 2 };
                let table = r.take((channels * h * entry) as u64)?;
                let count = |i: usize| -> usize {
                    let b = &table[i * entry..(i + 1) * entry];
                    if large {
                        u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize
                    } else {
                        usize::from(u16::from_be_bytes([b[0], b[1]]))
                    }
                };
                // Every row needs at least one header byte per 128 output bytes; checking
                // this up front bounds the expansion of tiny hostile files.
                let min_row = 2 * row_bytes.div_ceil(128);
                let mut total = 0u64;
                for i in 0..planes * h {
                    let c = count(i);
                    if c < min_row {
                        return Err(DecodeError::malformed("PSD RLE row too short"));
                    }
                    total += c as u64;
                }
                let body = r.take(total)?;
                let mut row = vec![0u8; row_bytes];
                self.alloc()?;
                let mut at = 0usize;
                for i in 0..planes * h {
                    let c = count(i);
                    unpack_bits(&body[at..at + c], &mut row)?;
                    at += c;
                    self.put_row(i / h, i % h, &row);
                }
                Ok(())
            }
            2 | 3 => Err(DecodeError::malformed(
                "ZIP-compressed PSD composite is not supported",
            )),
            other => Err(DecodeError::malformed(format!(
                "unknown PSD compression {other}"
            ))),
        }
    }

    fn put_row(&mut self, plane: usize, y: usize, row: &[u8]) {
        let w = self.width as usize;
        match self.depth {
            1 => {
                for (x, s) in self.samples.iter_mut().enumerate() {
                    // Bitmap mode: a set bit is black.
                    *s = if row[x / 8] & (0x80 >> (x % 8)) != 0 {
                        0
                    } else {
                        255
                    };
                }
            }
            16 => {
                for (s, pair) in self.samples.iter_mut().zip(row.as_chunks::<2>().0) {
                    *s = pair[0];
                }
            }
            _ => self.samples.copy_from_slice(&row[..w]),
        }
        let samples = &self.samples;
        match self.mode {
            Mode::Bitmap | Mode::Gray => self.out[y * w..(y + 1) * w].copy_from_slice(samples),
            Mode::Indexed => {
                let pal = self.palette.unwrap_or(&[0u8; 768]);
                let line = &mut self.out[y * w * 3..(y + 1) * w * 3];
                for (px, &i) in line.as_chunks_mut::<3>().0.iter_mut().zip(samples) {
                    let i = usize::from(i);
                    *px = [pal[i], pal[256 + i], pal[512 + i]];
                }
            }
            Mode::Rgb | Mode::Cmyk => {
                let line = &mut self.out[y * w * 3..(y + 1) * w * 3];
                if plane < 3 {
                    for (px, &s) in line.as_chunks_mut::<3>().0.iter_mut().zip(samples) {
                        px[plane] = s;
                    }
                } else {
                    // CMYK is stored inverted (255 = no ink): RGB = CMY * K.
                    for (px, &k) in line.as_chunks_mut::<3>().0.iter_mut().zip(samples) {
                        for v in px.iter_mut() {
                            *v = ((u16::from(*v) * u16::from(k) + 127) / 255) as u8;
                        }
                    }
                }
            }
        }
    }

    fn finish(self) -> Result<DynamicImage> {
        let err = || DecodeError::malformed("PSD buffer size mismatch");
        let (w, h) = (self.width, self.height);
        Ok(match self.mode {
            Mode::Bitmap | Mode::Gray => {
                DynamicImage::ImageLuma8(GrayImage::from_raw(w, h, self.out).ok_or_else(err)?)
            }
            _ => DynamicImage::ImageRgb8(RgbImage::from_raw(w, h, self.out).ok_or_else(err)?),
        })
    }
}

/// PackBits: `n` in 0..=127 copies `n + 1` literal bytes, -127..=-1 repeats the next
/// byte `1 - n` times, -128 is a no-op. The row must be filled exactly.
fn unpack_bits(src: &[u8], row: &mut [u8]) -> Result<()> {
    let bad = || DecodeError::malformed("PSD PackBits row does not match the width");
    let (mut s, mut d) = (0usize, 0usize);
    while d < row.len() {
        let n = *src.get(s).ok_or_else(bad)? as i8;
        s += 1;
        if n >= 0 {
            let len = n as usize + 1;
            let lit = src.get(s..s + len).ok_or_else(bad)?;
            row.get_mut(d..d + len)
                .ok_or_else(bad)?
                .copy_from_slice(lit);
            s += len;
            d += len;
        } else if n != -128 {
            let len = (1 - i16::from(n)) as usize;
            let v = *src.get(s).ok_or_else(bad)?;
            row.get_mut(d..d + len).ok_or_else(bad)?.fill(v);
            s += 1;
            d += len;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgb};
    use std::io::Cursor;

    fn header(version: u16, channels: u16, w: u32, h: u32, depth: u16, mode: u16) -> Vec<u8> {
        let mut f = SIGNATURE.to_vec();
        f.extend_from_slice(&version.to_be_bytes());
        f.extend_from_slice(&[0; 6]);
        f.extend_from_slice(&channels.to_be_bytes());
        f.extend_from_slice(&h.to_be_bytes());
        f.extend_from_slice(&w.to_be_bytes());
        f.extend_from_slice(&depth.to_be_bytes());
        f.extend_from_slice(&mode.to_be_bytes());
        f
    }

    fn resource(id: u16, payload: &[u8]) -> Vec<u8> {
        let mut r = b"8BIM".to_vec();
        r.extend_from_slice(&id.to_be_bytes());
        r.extend_from_slice(&[0, 0]); // empty Pascal name, padded
        r.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        r.extend_from_slice(payload);
        if payload.len() % 2 == 1 {
            r.push(0);
        }
        r
    }

    fn jpeg_thumb(color: [u8; 3], w: u32, h: u32) -> Vec<u8> {
        let mut jpg = Cursor::new(Vec::new());
        RgbImage::from_pixel(w, h, Rgb(color))
            .write_to(&mut jpg, ImageFormat::Jpeg)
            .unwrap();
        let jpg = jpg.into_inner();
        let mut p = Vec::new();
        for v in [THUMB_FORMAT_JPEG, w, h, w * 3, w * h * 3, jpg.len() as u32] {
            p.extend_from_slice(&v.to_be_bytes());
        }
        p.extend_from_slice(&24u16.to_be_bytes());
        p.extend_from_slice(&1u16.to_be_bytes());
        p.extend_from_slice(&jpg);
        p
    }

    /// Document with the given resources and image data, empty colour/layer sections.
    fn document(head: Vec<u8>, resources: &[u8], large: bool, image_data: &[u8]) -> Vec<u8> {
        let mut f = head;
        f.extend_from_slice(&0u32.to_be_bytes());
        f.extend_from_slice(&(resources.len() as u32).to_be_bytes());
        f.extend_from_slice(resources);
        if large {
            f.extend_from_slice(&0u64.to_be_bytes());
        } else {
            f.extend_from_slice(&0u32.to_be_bytes());
        }
        f.extend_from_slice(image_data);
        f
    }

    fn close(a: [u8; 3], b: [u8; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 6)
    }

    #[test]
    fn thumbnail_resource_is_preferred() {
        let res = [
            resource(1000, b"x"),
            resource(RES_THUMBNAIL, &jpeg_thumb([200, 30, 30], 16, 8)),
        ]
        .concat();
        // Composite data is garbage on purpose: the thumbnail must win.
        let f = document(header(1, 3, 1000, 500, 8, 3), &res, false, &[0xFF; 4]);
        let img = extract(&f).unwrap();
        assert_eq!((img.width(), img.height()), (16, 8));
        assert!(close(img.to_rgb8().get_pixel(8, 4).0, [200, 30, 30]));
    }

    #[test]
    fn legacy_thumbnail_is_bgr() {
        let res = resource(RES_THUMBNAIL_LEGACY, &jpeg_thumb([20, 40, 220], 8, 8));
        let f = document(header(1, 3, 8, 8, 8, 3), &res, false, &[]);
        let img = extract(&f).unwrap().to_rgb8();
        assert!(close(img.get_pixel(4, 4).0, [220, 40, 20]));
    }

    #[test]
    fn raw_rgb_composite() {
        // 2x1 RGB raw planar: R plane, G plane, B plane.
        let mut data = 0u16.to_be_bytes().to_vec();
        data.extend_from_slice(&[255, 0, 0, 255, 0, 0]);
        let f = document(header(1, 3, 2, 1, 8, 3), &[], false, &data);
        let img = extract(&f).unwrap().to_rgb8();
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 255, 0]);
    }

    #[test]
    fn psb_rle_cmyk_composite() {
        // 3x1 CMYK (inverted storage), 4-byte RLE row counts; an extra alpha channel.
        let rows: [&[u8]; 5] = [
            &[0xFE, 0xFF],             // C: 255 x3 (no cyan)
            &[0x02, 0x00, 0xFF, 0xFF], // M: literal 0, 255, 255
            &[0xFE, 0xFF],             // Y
            &[0x02, 0xFF, 0xFF, 0x00], // K: literal 255, 255, 0 (full black last)
            &[0xFE, 0xFF],             // alpha (ignored)
        ];
        let mut data = 1u16.to_be_bytes().to_vec();
        for r in rows {
            data.extend_from_slice(&(r.len() as u32).to_be_bytes());
        }
        for r in rows {
            data.extend_from_slice(r);
        }
        let f = document(header(2, 5, 3, 1, 8, 4), &[], true, &data);
        let img = extract(&f).unwrap().to_rgb8();
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [255, 255, 255]);
        assert_eq!(img.get_pixel(2, 0).0, [0, 0, 0]);
    }

    #[test]
    fn rle_composite_over_byte_budget_is_too_large() {
        // 16384 x 16384 RGB passes the pixel and PackBits-expansion checks with a 13 MB
        // body, but the 768 MiB output exceeds the decode byte budget.
        let (side, planes) = (16384usize, 3usize);
        let min_row = 2 * side.div_ceil(128);
        let mut data = 1u16.to_be_bytes().to_vec();
        for _ in 0..planes * side {
            data.extend_from_slice(&(min_row as u16).to_be_bytes());
        }
        data.resize(data.len() + planes * side * min_row, 0);
        let f = document(header(1, 3, 16384, 16384, 8, 3), &[], false, &data);
        assert_eq!(extract(&f).err(), Some(DecodeError::TooLarge));
    }

    #[test]
    fn sixteen_bit_gray_and_bitmap() {
        let mut data = 0u16.to_be_bytes().to_vec();
        data.extend_from_slice(&[0x12, 0x34, 0xAB, 0xCD]);
        let f = document(header(1, 1, 2, 1, 16, 1), &[], false, &data);
        assert_eq!(extract(&f).unwrap().to_luma8().as_raw(), &vec![0x12, 0xAB]);

        let mut data = 0u16.to_be_bytes().to_vec();
        data.push(0b0100_0000);
        let f = document(header(1, 1, 3, 1, 1, 0), &[], false, &data);
        assert_eq!(extract(&f).unwrap().to_luma8().as_raw(), &vec![255, 0, 255]);
    }

    #[test]
    fn hostile_inputs_are_rejected() {
        // Section length pointing far beyond the file.
        let mut f = header(1, 3, 10, 10, 8, 3);
        f.extend_from_slice(&0xFFFF_FFF0u32.to_be_bytes());
        assert!(matches!(extract(&f), Err(DecodeError::Malformed(_))));
        // Raw composite shorter than declared.
        let mut data = 0u16.to_be_bytes().to_vec();
        data.extend_from_slice(&[1, 2, 3]);
        let f = document(header(1, 3, 100, 100, 8, 3), &[], false, &data);
        assert!(matches!(extract(&f), Err(DecodeError::Malformed(_))));
        // Huge PSB declared with a tiny RLE table: refused before the pixel buffer.
        let f = document(
            header(2, 3, 300_000, 1700, 8, 3),
            &[],
            true,
            &1u16.to_be_bytes(),
        );
        assert!(matches!(extract(&f), Err(DecodeError::Malformed(_))));
        // RLE row that overruns its width.
        let mut data = 1u16.to_be_bytes().to_vec();
        data.extend_from_slice(&2u16.to_be_bytes());
        data.extend_from_slice(&[0x80 | 0x7F, 0xAA]);
        let f = document(header(1, 1, 4, 1, 8, 1), &[], false, &data);
        assert!(matches!(extract(&f), Err(DecodeError::Malformed(_))));
        // Over the pixel budget.
        let f = document(header(2, 3, 300_000, 300_000, 8, 3), &[], true, &[]);
        assert_eq!(extract(&f), Err(DecodeError::TooLarge));
        // Damaged thumbnail and no composite: still an error, not a panic.
        let mut bad = jpeg_thumb([1, 2, 3], 4, 4);
        bad.truncate(40);
        let f = document(
            header(1, 3, 4, 4, 8, 3),
            &resource(RES_THUMBNAIL, &bad),
            false,
            &[],
        );
        assert!(matches!(extract(&f), Err(DecodeError::Malformed(_))));
        assert_eq!(extract(b"GIF89a"), Err(DecodeError::Unsupported));
    }
}
