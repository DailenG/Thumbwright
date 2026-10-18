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

struct Cel {
    layer: usize,
    x: i32,
    y: i32,
    opacity: u8,
    z: i16,
    w: u32,
    h: u32,
    /// RGBA8 (already palette-resolved).
    rgba: Vec<u8>,
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
    let mut cels: Vec<Cel> = Vec::new();
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

    for PendingCel {
        layer,
        x,
        y,
        opacity,
        z,
        cel_type,
        body,
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
        check_alloc(expected)?;
        let raw = if cel_type == 0 {
            c.bytes(expected as usize)?.to_vec()
        } else {
            let mut out = Vec::new();
            ZlibDecoder::new(c.rest())
                .take(expected)
                .read_to_end(&mut out)
                .map_err(|e| DecodeError::malformed(format!("aseprite: zlib: {e}")))?;
            if out.len() as u64 != expected {
                return Err(DecodeError::malformed("aseprite: short cel data"));
            }
            out
        };
        let transparent = if background {
            None
        } else {
            Some(transparent_index)
        };
        cels.push(Cel {
            layer,
            x,
            y,
            opacity,
            z,
            w: cw,
            h: ch,
            rgba: to_rgba(&raw, depth, &palette, transparent),
        });
    }
    if cels.is_empty() {
        return Err(DecodeError::malformed(
            "aseprite: first frame has no visible cels",
        ));
    }
    // Layer order, then z-index (higher draws later).
    cels.sort_by_key(|c| (c.layer as i64 + i64::from(c.z), c.z));

    let mut canvas = vec![0u8; w as usize * h as usize * 4];
    for cel in &cels {
        let layer_opacity = layers.get(cel.layer).map_or(255, |l| l.opacity);
        let opacity = u32::from(cel.opacity) * u32::from(layer_opacity) / 255;
        blit(&mut canvas, w, h, cel, opacity);
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

fn to_rgba(raw: &[u8], depth: Depth, palette: &[[u8; 4]], transparent: Option<u8>) -> Vec<u8> {
    match depth {
        Depth::Rgba => raw.to_vec(),
        Depth::Gray => raw
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        Depth::Indexed => raw
            .iter()
            .flat_map(|&i| {
                if Some(i) == transparent {
                    [0, 0, 0, 0]
                } else {
                    palette
                        .get(usize::from(i))
                        .copied()
                        .unwrap_or([0, 0, 0, 255])
                }
            })
            .collect(),
    }
}

/// Source-over of a non-premultiplied cel (scaled by `opacity`) onto the canvas, clipped.
fn blit(canvas: &mut [u8], cw: u32, ch: u32, cel: &Cel, opacity: u32) {
    for sy in 0..cel.h {
        let dy = i64::from(cel.y) + i64::from(sy);
        if dy < 0 || dy >= i64::from(ch) {
            continue;
        }
        for sx in 0..cel.w {
            let dx = i64::from(cel.x) + i64::from(sx);
            if dx < 0 || dx >= i64::from(cw) {
                continue;
            }
            let s = (sy as usize * cel.w as usize + sx as usize) * 4;
            let d = (dy as usize * cw as usize + dx as usize) * 4;
            let (Some(src), Some(dst)) = (cel.rgba.get(s..s + 4), canvas.get_mut(d..d + 4)) else {
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
}
