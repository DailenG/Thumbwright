//! Aseprite (`.ase`, `.aseprite`): first frame composited onto a transparent canvas.
//!
//! Layer chunks give visibility (including parent groups) and opacity; cel chunks supply
//! the pixels (raw, linked is meaningless in frame 0, zlib-compressed image; tilemaps are
//! skipped); palette chunks (new and old) back indexed sprites. Blend modes other than
//! normal are treated as normal.

use std::io::Read;

use flate2::read::ZlibDecoder;
use image::{DynamicImage, RgbaImage};

use crate::decode::{DecodeError, Result, check_alloc, check_dimensions};
use crate::limits;

const FILE_MAGIC: u16 = 0xA5E0;
const FRAME_MAGIC: u16 = 0xF1FA;
const HEADER_LEN: usize = 128;
/// Chunks read from the first frame.
const MAX_CHUNKS: usize = 65_536;

struct Reader<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.d.len());
        let end = end.ok_or_else(|| DecodeError::malformed("aseprite: truncated"))?;
        let s = &self.d[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(
            self.bytes(2)?.try_into().unwrap_or_default(),
        ))
    }
    fn i16(&mut self) -> Result<i16> {
        Ok(self.u16()? as i16)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(
            self.bytes(4)?.try_into().unwrap_or_default(),
        ))
    }
    fn skip(&mut self, n: usize) -> Result<()> {
        self.bytes(n).map(|_| ())
    }
    fn rest(&self) -> &'a [u8] {
        &self.d[self.pos.min(self.d.len())..]
    }
}

struct Layer {
    visible: bool,
    level: u16,
    opacity: u8,
    is_image: bool,
    background: bool,
}

/// A raw or zlib cel chunk awaiting palette resolution (palettes may follow layers/cels).
struct PendingCel<'a> {
    layer: usize,
    x: i32,
    y: i32,
    opacity: u8,
    z: i16,
    cel_type: u16,
    body: &'a [u8],
}

/// A decoded cel: geometry, and the raw samples it is drawn from (not expanded to RGBA).
struct Cel<'a> {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    raw: &'a [u8],
    depth: Depth,
    palette: &'a [[u8; 4]],
    /// Palette index that is transparent (`None` on a background layer).
    transparent: Option<u8>,
}

impl Cel<'_> {
    /// Sample `i` (row-major) as non-premultiplied RGBA.
    fn pixel(&self, i: usize) -> Option<[u8; 4]> {
        Some(match self.depth {
            Depth::Rgba => {
                let p = self.raw.get(i * 4..i * 4 + 4)?;
                [p[0], p[1], p[2], p[3]]
            }
            Depth::Gray => {
                let p = self.raw.get(i * 2..i * 2 + 2)?;
                [p[0], p[0], p[0], p[1]]
            }
            Depth::Indexed => {
                let index = *self.raw.get(i)?;
                if Some(index) == self.transparent {
                    [0, 0, 0, 0]
                } else {
                    self.palette
                        .get(usize::from(index))
                        .copied()
                        .unwrap_or([0, 0, 0, 255])
                }
            }
        })
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Depth {
    Rgba,
    Gray,
    Indexed,
}

impl Depth {
    fn bytes(self) -> u64 {
        match self {
            Depth::Rgba => 4,
            Depth::Gray => 2,
            Depth::Indexed => 1,
        }
    }
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    decode_within(data, limits::ARCHIVE_TOTAL_MAX_BYTES)
}

/// [`decode`] with `cel_budget` bytes of cel pixel data (summed over every cel decoded) for
/// the whole file. Cel size is independent of the canvas size, and a file may hold tens of
/// thousands of cels, each a zlib stream that inflates about a thousandfold.
fn decode_within(data: &[u8], cel_budget: u64) -> Result<DynamicImage> {
    let mut r = Reader { d: data, pos: 0 };
    if data.len() < HEADER_LEN || data.get(4..6) != Some(&FILE_MAGIC.to_le_bytes()) {
        return Err(DecodeError::Unsupported);
    }
    r.skip(4)?; // file size
    r.skip(2)?; // magic
    let frames = r.u16()?;
    let (w, h) = (u32::from(r.u16()?), u32::from(r.u16()?));
    let depth = match r.u16()? {
        32 => Depth::Rgba,
        16 => Depth::Gray,
        8 => Depth::Indexed,
        _ => return Err(DecodeError::malformed("aseprite: colour depth")),
    };
    let flags = r.u32()?;
    r.skip(2 + 4 + 4)?; // speed, reserved
    let transparent_index = r.u8()?;
    if frames == 0 {
        return Err(DecodeError::malformed("aseprite: no frames"));
    }
    check_dimensions(w, h)?;
    check_alloc(u64::from(w) * u64::from(h) * 4)?;

    // Frame 0.
    r.pos = HEADER_LEN;
    let frame_len = r.u32()? as usize;
    if r.u16()? != FRAME_MAGIC {
        return Err(DecodeError::malformed("aseprite: bad frame magic"));
    }
    let old_chunks = r.u16()?;
    r.skip(2 + 2)?; // duration, reserved
    let new_chunks = r.u32()?;
    let chunk_count = if new_chunks != 0 {
        new_chunks
    } else {
        u32::from(old_chunks)
    };
    let frame_end = HEADER_LEN
        .checked_add(frame_len)
        .filter(|&e| e >= HEADER_LEN + 16 && e <= data.len())
        .ok_or_else(|| DecodeError::malformed("aseprite: frame length"))?;

    let mut palette: Vec<[u8; 4]> = Vec::new();
    let mut layers: Vec<Layer> = Vec::new();
    let mut pending: Vec<PendingCel> = Vec::new();
    for _ in 0..(chunk_count as usize).min(MAX_CHUNKS) {
        if r.pos + 6 > frame_end {
            break;
        }
        let start = r.pos;
        let size = r.u32()? as usize;
        let ty = r.u16()?;
        let end = start
            .checked_add(size)
            .filter(|&e| size >= 6 && e <= frame_end)
            .ok_or_else(|| DecodeError::malformed("aseprite: chunk length"))?;
        let mut c = Reader {
            d: &data[..end],
            pos: r.pos,
        };
        r.pos = end;
        match ty {
            0x2004 => layers.push(parse_layer(&mut c, flags)?),
            0x2005 => {
                let layer = usize::from(c.u16()?);
                let (x, y) = (i32::from(c.i16()?), i32::from(c.i16()?));
                let opacity = c.u8()?;
                let cel_type = c.u16()?;
                let z = c.i16()?;
                c.skip(5)?;
                if cel_type == 0 || cel_type == 2 {
                    pending.push(PendingCel {
                        layer,
                        x,
                        y,
                        opacity,
                        z,
                        cel_type,
                        body: c.rest(),
                    });
                }
            }
            0x2019 => parse_palette(&mut c, &mut palette)?,
            0x0004 if palette.is_empty() => parse_old_palette(&mut c, &mut palette)?,
            _ => {}
        }
    }

    // Draw order: layer index plus z-index, then z-index (higher draws later); stable, so
    // equal keys keep file order.
    pending.sort_by_key(|p| (p.layer as i64 + i64::from(p.z), p.z));

    // Cels are decoded and drawn one at a time: what a file can make us hold is the canvas
    // and the largest single cel, not every cel at once.
    let canvas_bytes = u64::from(w) * u64::from(h) * 4;
    let mut canvas = vec![0u8; canvas_bytes as usize];
    let mut cel_budget = cel_budget;
    let mut seen = 0usize;
    for PendingCel {
        layer,
        x,
        y,
        opacity,
        cel_type,
        body,
        ..
    } in pending
    {
        let Some(l) = layers.get(layer) else { continue };
        if !l.is_image || !layer_visible(&layers, layer) {
            continue;
        }
        let background = l.background;
        let mut c = Reader { d: body, pos: 0 };
        let (cw, ch) = (u32::from(c.u16()?), u32::from(c.u16()?));
        if cw == 0 || ch == 0 {
            continue;
        }
        check_dimensions(cw, ch)?;
        let expected = u64::from(cw) * u64::from(ch) * depth.bytes();
        // The canvas stays live while the cel is read, so both count against one buffer budget.
        check_alloc(expected)?;
        check_alloc(expected + canvas_bytes)?;
        seen += 1;
        // A cel wholly outside the canvas draws nothing: do not decode it.
        if i64::from(x) >= i64::from(w)
            || i64::from(y) >= i64::from(h)
            || i64::from(x) + i64::from(cw) <= 0
            || i64::from(y) + i64::from(ch) <= 0
        {
            continue;
        }
        cel_budget = cel_budget
            .checked_sub(expected)
            .ok_or(DecodeError::TooLarge)?;
        let inflated;
        let raw: &[u8] = if cel_type == 0 {
            c.bytes(expected as usize)?
        } else {
            let mut out = Vec::new();
            ZlibDecoder::new(c.rest())
                .take(expected)
                .read_to_end(&mut out)
                .map_err(|e| DecodeError::malformed(format!("aseprite: zlib: {e}")))?;
            if out.len() as u64 != expected {
                return Err(DecodeError::malformed("aseprite: short cel data"));
            }
            inflated = out;
            &inflated
        };
        let cel = Cel {
            x,
            y,
            w: cw,
            h: ch,
            raw,
            depth,
            palette: &palette,
            transparent: if background {
                None
            } else {
                Some(transparent_index)
            },
        };
        let layer_opacity = layers.get(layer).map_or(255, |l| l.opacity);
        let opacity = u32::from(opacity) * u32::from(layer_opacity) / 255;
        blit(&mut canvas, w, h, &cel, opacity);
    }
    if seen == 0 {
        return Err(DecodeError::malformed(
            "aseprite: first frame has no visible cels",
        ));
    }
    RgbaImage::from_raw(w, h, canvas)
        .map(DynamicImage::ImageRgba8)
        .ok_or_else(|| DecodeError::malformed("aseprite: pixel buffer"))
}

fn parse_layer(c: &mut Reader, header_flags: u32) -> Result<Layer> {
    let flags = c.u16()?;
    let ty = c.u16()?;
    let level = c.u16()?;
    c.skip(4)?; // default size
    c.skip(2)?; // blend mode: every mode composites as normal
    let opacity = c.u8()?;
    Ok(Layer {
        visible: flags & 1 != 0,
        level,
        // Layer opacity is only meaningful when the header says so.
        opacity: if header_flags & 1 != 0 { opacity } else { 255 },
        is_image: ty == 0,
        background: flags & 8 != 0,
    })
}

/// Visible, and so is every enclosing group (parents have a lower child level and
/// precede their children).
fn layer_visible(layers: &[Layer], index: usize) -> bool {
    let mut level = layers[index].level;
    if !layers[index].visible {
        return false;
    }
    for l in layers[..index].iter().rev() {
        if level == 0 {
            break;
        }
        if l.level < level {
            if !l.visible {
                return false;
            }
            level = l.level;
        }
    }
    true
}

fn parse_palette(c: &mut Reader, palette: &mut Vec<[u8; 4]>) -> Result<()> {
    let size = c.u32()? as usize;
    let (first, last) = (c.u32()? as usize, c.u32()? as usize);
    c.skip(8)?;
    if last < first || last >= 65_536 || size > 65_536 {
        return Err(DecodeError::malformed("aseprite: palette range"));
    }
    palette.clear();
    palette.resize(size.max(last + 1), [0, 0, 0, 255]);
    let entries = palette
        .get_mut(first..=last)
        .ok_or_else(|| DecodeError::malformed("aseprite: palette range"))?;
    for entry in entries {
        let flags = c.u16()?;
        let rgba = c.bytes(4)?;
        *entry = [rgba[0], rgba[1], rgba[2], rgba[3]];
        if flags & 1 != 0 {
            let n = usize::from(c.u16()?);
            c.skip(n)?;
        }
    }
    Ok(())
}

fn parse_old_palette(c: &mut Reader, palette: &mut Vec<[u8; 4]>) -> Result<()> {
    let packets = c.u16()?;
    let mut index = 0usize;
    for _ in 0..packets {
        index += usize::from(c.u8()?);
        let count = match c.u8()? {
            0 => 256,
            n => usize::from(n),
        };
        for _ in 0..count {
            let rgb = c.bytes(3)?;
            if index >= 256 {
                return Err(DecodeError::malformed("aseprite: palette index"));
            }
            if palette.len() <= index {
                palette.resize(index + 1, [0, 0, 0, 255]);
            }
            palette[index] = [rgb[0], rgb[1], rgb[2], 255];
            index += 1;
        }
    }
    Ok(())
}

/// Source-over of a non-premultiplied cel (scaled by `opacity`) onto the canvas, clipped.
fn blit(canvas: &mut [u8], cw: u32, ch: u32, cel: &Cel, opacity: u32) {
    // Only the part of the cel that lies on the canvas is visited (a cel can be far larger).
    let (cx, cy) = (i64::from(cel.x), i64::from(cel.y));
    let sx_range = (-cx).max(0)..(i64::from(cw) - cx).min(i64::from(cel.w));
    let sy_range = (-cy).max(0)..(i64::from(ch) - cy).min(i64::from(cel.h));
    for sy in sy_range {
        let dy = cy + sy;
        for sx in sx_range.clone() {
            let dx = cx + sx;
            let s = sy as usize * cel.w as usize + sx as usize;
            let d = (dy as usize * cw as usize + dx as usize) * 4;
            let (Some(src), Some(dst)) = (cel.pixel(s), canvas.get_mut(d..d + 4)) else {
                continue;
            };
            let sa = u32::from(src[3]) * opacity / 255;
            if sa == 0 {
                continue;
            }
            let da = u32::from(dst[3]);
            let out_a = sa + da * (255 - sa) / 255;
            if out_a == 0 {
                continue;
            }
            for k in 0..3 {
                let sc = u32::from(src[k]) * sa;
                let dc = u32::from(dst[k]) * da * (255 - sa) / 255;
                dst[k] = ((sc + dc) / out_a).min(255) as u8;
            }
            dst[3] = out_a.min(255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn chunk(ty: u16, body: &[u8]) -> Vec<u8> {
        let mut c = (body.len() as u32 + 6).to_le_bytes().to_vec();
        c.extend(ty.to_le_bytes());
        c.extend(body);
        c
    }

    fn layer(flags: u16, level: u16, opacity: u8, ty: u16) -> Vec<u8> {
        let mut b = flags.to_le_bytes().to_vec();
        b.extend(ty.to_le_bytes());
        b.extend(level.to_le_bytes());
        b.extend([0u8; 4]);
        b.extend(0u16.to_le_bytes()); // blend
        b.push(opacity);
        b.extend([0u8; 3]);
        b.extend(1u16.to_le_bytes());
        b.push(b'L');
        chunk(0x2004, &b)
    }

    #[allow(clippy::too_many_arguments)]
    fn cel(
        layer: u16,
        x: i16,
        y: i16,
        opacity: u8,
        kind: u16,
        w: u16,
        h: u16,
        data: &[u8],
    ) -> Vec<u8> {
        let mut b = layer.to_le_bytes().to_vec();
        b.extend(x.to_le_bytes());
        b.extend(y.to_le_bytes());
        b.push(opacity);
        b.extend(kind.to_le_bytes());
        b.extend(0i16.to_le_bytes());
        b.extend([0u8; 5]);
        b.extend(w.to_le_bytes());
        b.extend(h.to_le_bytes());
        b.extend(data);
        chunk(0x2005, &b)
    }

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        z.write_all(data).unwrap();
        z.finish().unwrap()
    }

    fn file(w: u16, h: u16, depth: u16, header_flags: u32, chunks: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = chunks.concat();
        let frame_len = 16 + body.len();
        let mut f = ((HEADER_LEN + frame_len) as u32).to_le_bytes().to_vec();
        f.extend(FILE_MAGIC.to_le_bytes());
        f.extend(1u16.to_le_bytes());
        f.extend(w.to_le_bytes());
        f.extend(h.to_le_bytes());
        f.extend(depth.to_le_bytes());
        f.extend(header_flags.to_le_bytes());
        f.extend(100u16.to_le_bytes());
        f.extend([0u8; 8]);
        f.push(0); // transparent index
        f.resize(HEADER_LEN, 0);
        f.extend((frame_len as u32).to_le_bytes());
        f.extend(FRAME_MAGIC.to_le_bytes());
        f.extend((chunks.len() as u16).to_le_bytes());
        f.extend(100u16.to_le_bytes());
        f.extend([0u8; 2]);
        f.extend((chunks.len() as u32).to_le_bytes());
        f.extend(body);
        f
    }

    #[test]
    fn rgba_layers_composite_with_visibility_and_offsets() {
        let red = [255, 0, 0, 255].repeat(4 * 4);
        let blue_half = [0, 0, 255, 255].repeat(2 * 2);
        let hidden = [0, 255, 0, 255].repeat(4 * 4);
        let f = file(
            4,
            4,
            32,
            1,
            &[
                layer(1, 0, 255, 0),
                layer(1, 0, 255, 0),
                layer(0, 0, 255, 0), // hidden
                cel(0, 0, 0, 255, 2, 4, 4, &zlib(&red)),
                cel(1, 1, 1, 128, 0, 2, 2, &blue_half),
                cel(2, 0, 0, 255, 0, 4, 4, &hidden),
            ],
        );
        let img = decode(&f).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (4, 4));
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
        let p = img.get_pixel(1, 1).0;
        assert!(
            p[0] > 120 && p[0] < 135 && p[2] > 120 && p[2] < 135 && p[3] == 255,
            "{p:?}"
        );
        assert_eq!(img.get_pixel(3, 3).0, [255, 0, 0, 255]);
    }

    #[test]
    fn indexed_with_palette_and_transparent_index() {
        let mut pal = 2u32.to_le_bytes().to_vec();
        pal.extend(0u32.to_le_bytes());
        pal.extend(1u32.to_le_bytes());
        pal.extend([0u8; 8]);
        for rgb in [[1u8, 2, 3], [10, 20, 30]] {
            pal.extend(0u16.to_le_bytes());
            pal.extend([rgb[0], rgb[1], rgb[2], 255]);
        }
        let f = file(
            2,
            1,
            8,
            0,
            &[
                chunk(0x2019, &pal),
                layer(1, 0, 255, 0),
                cel(0, 0, 0, 255, 0, 2, 1, &[0, 1]),
            ],
        );
        let img = decode(&f).unwrap().to_rgba8();
        assert_eq!(img.get_pixel(0, 0).0[3], 0); // index 0 is transparent on non-background layers
        assert_eq!(img.get_pixel(1, 0).0, [10, 20, 30, 255]);
    }

    #[test]
    fn grayscale_depth_and_old_palette() {
        let f = file(
            1,
            1,
            16,
            0,
            &[layer(1, 0, 255, 0), cel(0, 0, 0, 255, 0, 1, 1, &[77, 200])],
        );
        assert_eq!(
            decode(&f).unwrap().to_rgba8().get_pixel(0, 0).0,
            [77, 77, 77, 200]
        );

        let mut old = 1u16.to_le_bytes().to_vec();
        old.extend([1, 2, 9, 8, 7, 6, 5, 4]); // skip 1, 2 colours
        let f = file(
            1,
            1,
            8,
            0,
            &[
                chunk(0x0004, &old),
                layer(9, 0, 255, 0),
                cel(0, 0, 0, 255, 0, 1, 1, &[2]),
            ],
        );
        // Index 1 = (9,8,7), index 2 = (6,5,4); layer is background so index 0 style
        // transparency does not apply.
        assert_eq!(
            decode(&f).unwrap().to_rgba8().get_pixel(0, 0).0,
            [6, 5, 4, 255]
        );
    }

    #[test]
    fn hostile_inputs() {
        let good = file(
            2,
            2,
            32,
            0,
            &[layer(1, 0, 255, 0), cel(0, 0, 0, 255, 0, 2, 2, &[0u8; 16])],
        );
        assert!(decode(&good).is_ok());
        for cut in [0, 10, 127, 130, 150, good.len() - 5] {
            assert!(decode(&good[..cut]).is_err(), "cut {cut}");
        }
        // Canvas 65535 x 65535.
        let mut big = good.clone();
        big[8..12].copy_from_slice(&[0xFF; 4]);
        assert_eq!(decode(&big), Err(DecodeError::TooLarge));
        // Cel declaring 65535 x 65535 pixels over a tiny body.
        let bomb = file(
            2,
            2,
            32,
            0,
            &[
                layer(1, 0, 255, 0),
                cel(0, 0, 0, 255, 0, 65535, 65535, &[0u8; 4]),
            ],
        );
        assert_eq!(decode(&bomb), Err(DecodeError::TooLarge));
        // Raw cel shorter than declared.
        let short = file(
            2,
            2,
            32,
            0,
            &[
                layer(1, 0, 255, 0),
                cel(0, 0, 0, 255, 0, 100, 100, &[0u8; 4]),
            ],
        );
        assert!(matches!(decode(&short), Err(DecodeError::Malformed(_))));
        // Compressed cel inflating past its declared size is cut off and rejected as short/garbled.
        let z = zlib(&vec![0u8; 1 << 20]);
        let bomb = file(
            2,
            2,
            32,
            0,
            &[layer(1, 0, 255, 0), cel(0, 0, 0, 255, 2, 100, 100, &z)],
        );
        assert!(
            decode(&bomb).is_ok(),
            "extra inflated data beyond w*h*bpp is ignored"
        );
        // Chunk length lying past the frame.
        let mut lie = good.clone();
        let at = HEADER_LEN + 16;
        lie[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(decode(&lie), Err(DecodeError::Malformed(_))));
        assert_eq!(decode(&[0u8; 200]), Err(DecodeError::Unsupported));
    }

    /// An indexed file with `count` visible layers and one `side` x `side` cel (all zero
    /// samples, zlib) per layer, all at the origin of a 16 x 16 canvas.
    fn many_cels(count: u16, side: u16) -> Vec<u8> {
        let samples = zlib(&vec![0u8; usize::from(side) * usize::from(side)]);
        let mut chunks = Vec::new();
        for _ in 0..count {
            chunks.push(layer(1, 0, 255, 0));
        }
        for i in 0..count {
            chunks.push(cel(i, 0, 0, 255, 2, side, side, &samples));
        }
        file(16, 16, 8, 0, &chunks)
    }

    /// DB-01: every cel was expanded to RGBA (4x for indexed) and kept until the end, so
    /// peak memory grew with the number of cels. Cels are now drawn one at a time.
    #[test]
    fn cel_memory_does_not_accumulate() {
        // 12 cels x 4 MiB of samples: 192 MiB of retained RGBA before the fix.
        let f = many_cels(12, 2048);
        let (result, usage) = crate::testalloc::measure(|| decode(&f));
        assert!(result.is_ok());
        assert!(usage.peak < 16 << 20, "peak {} bytes", usage.peak);
        // No single request near the 16 MiB RGBA expansion of one cel.
        assert!(usage.largest <= 8 << 20, "largest {} bytes", usage.largest);
    }

    /// A cel outside the canvas draws nothing and is not decoded (its data may be garbage).
    #[test]
    fn off_canvas_cels_are_not_decoded() {
        let f = file(
            4,
            4,
            32,
            0,
            &[
                layer(1, 0, 255, 0),
                layer(1, 0, 255, 0),
                cel(0, 0, 0, 255, 0, 4, 4, &[255u8; 64]),
                cel(1, 100, 0, 255, 2, 10, 10, b"not zlib"),
            ],
        );
        let img = decode(&f).unwrap().to_rgba8();
        assert_eq!(img.get_pixel(3, 3).0, [255; 4]);
    }

    /// The summed cel bytes of one file are budgeted, not just each cel.
    #[test]
    fn cumulative_cel_bytes_are_budgeted() {
        let f = many_cels(3, 512); // 3 x 256 KiB
        assert!(decode_within(&f, 3 * 512 * 512).is_ok());
        assert_eq!(
            decode_within(&f, 3 * 512 * 512 - 1),
            Err(DecodeError::TooLarge)
        );
    }
}
