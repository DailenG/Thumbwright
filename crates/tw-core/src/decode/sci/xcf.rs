//! GIMP XCF (`.xcf`, versions 0-11 and later 32/64-bit-offset files): the visible layers
//! are composited bottom to top with the normal blend mode, layer opacity, offsets and
//! layer masks onto a transparent canvas. 8-bit RGB, gray and indexed layers with or
//! without alpha; tile data stored raw, RLE or zlib compressed. Layer modes other than
//! normal are drawn as normal, and group layers contribute only through their children.

use image::DynamicImage;
use miniz_oxide::inflate::decompress_to_vec_zlib_with_limit;

use super::{DecodeError, Result, check_alloc, check_dimensions, pixel_buffer, rgba_image};

const TILE: u64 = 64;
const MAX_LAYERS: usize = 1024;
const MAX_PROPS: usize = 4096;
/// Largest number of tile pixels decoded for one file; bounds the work a hostile file can ask.
const MAX_TILE_PIXELS: u64 = 400_000_000;

const PROP_END: u32 = 0;
const PROP_COLORMAP: u32 = 1;
const PROP_OPACITY: u32 = 6;
const PROP_VISIBLE: u32 = 8;
const PROP_APPLY_MASK: u32 = 11;
const PROP_OFFSETS: u32 = 15;
const PROP_COMPRESSION: u32 = 17;
const PROP_GROUP_ITEM: u32 = 29;
const PROP_FLOAT_OPACITY: u32 = 33;

fn truncated() -> DecodeError {
    DecodeError::malformed("XCF data truncated")
}

struct Reader<'a> {
    d: &'a [u8],
    ptr64: bool,
}

impl Reader<'_> {
    fn u32(&self, at: usize) -> Result<u32> {
        let b: [u8; 4] = at
            .checked_add(4)
            .and_then(|e| self.d.get(at..e))
            .and_then(|b| b.try_into().ok())
            .ok_or_else(truncated)?;
        Ok(u32::from_be_bytes(b))
    }

    /// A file offset: 32 bits before version 11, 64 bits from then on.
    fn ptr(&self, at: usize) -> Result<usize> {
        if self.ptr64 {
            let b: [u8; 8] = at
                .checked_add(8)
                .and_then(|e| self.d.get(at..e))
                .and_then(|b| b.try_into().ok())
                .ok_or_else(truncated)?;
            usize::try_from(u64::from_be_bytes(b)).map_err(|_| truncated())
        } else {
            Ok(self.u32(at)? as usize)
        }
    }

    fn ptr_size(&self) -> usize {
        if self.ptr64 { 8 } else { 4 }
    }
}

struct Props {
    compression: u8,
    colormap: Vec<[u8; 3]>,
    opacity: f32,
    visible: bool,
    offset: (i64, i64),
    apply_mask: bool,
    group: bool,
}

/// Reads a property list at `pos`; returns the properties and the offset after `PROP_END`.
fn read_props(r: &Reader<'_>, mut pos: usize) -> Result<(Props, usize)> {
    let mut p = Props {
        compression: 0,
        colormap: Vec::new(),
        opacity: 1.0,
        visible: true,
        offset: (0, 0),
        apply_mask: true,
        group: false,
    };
    for _ in 0..MAX_PROPS {
        let id = r.u32(pos)?;
        let size = r.u32(pos + 4)? as usize;
        let body = pos + 8;
        if id == PROP_END {
            return Ok((p, body));
        }
        let payload = body
            .checked_add(size)
            .and_then(|e| r.d.get(body..e))
            .ok_or_else(truncated)?;
        match id {
            PROP_COLORMAP => {
                let n = r.u32(body)? as usize;
                let colors = payload.get(4..).unwrap_or(&[]);
                p.colormap = colors
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .take(n.min(256))
                    .copied()
                    .collect();
            }
            PROP_COMPRESSION => p.compression = payload.first().copied().unwrap_or(0),
            PROP_OPACITY if size >= 4 => p.opacity = r.u32(body)?.min(255) as f32 / 255.0,
            PROP_FLOAT_OPACITY if size >= 4 => {
                p.opacity = f32::from_bits(r.u32(body)?).clamp(0.0, 1.0);
            }
            PROP_VISIBLE if size >= 4 => p.visible = r.u32(body)? != 0,
            PROP_APPLY_MASK if size >= 4 => p.apply_mask = r.u32(body)? != 0,
            PROP_OFFSETS if size >= 8 => {
                p.offset = (
                    i64::from(r.u32(body)? as i32),
                    i64::from(r.u32(body + 4)? as i32),
                );
            }
            PROP_GROUP_ITEM => p.group = true,
            _ => {}
        }
        pos = body + size;
    }
    Err(DecodeError::malformed("XCF property list does not end"))
}

/// Tile table of one hierarchy (only its full-resolution level).
struct Hierarchy {
    width: u32,
    height: u32,
    bpp: usize,
    tiles: Vec<usize>,
}

fn read_hierarchy(r: &Reader<'_>, at: usize) -> Result<Hierarchy> {
    let (width, height, bpp) = (r.u32(at)?, r.u32(at + 4)?, r.u32(at + 8)? as usize);
    let level = r.ptr(at + 12)?;
    if level == 0 || !(1..=4).contains(&bpp) {
        return Err(DecodeError::Unsupported);
    }
    let tiles_x = u64::from(width).div_ceil(TILE);
    let tiles_y = u64::from(height).div_ceil(TILE);
    let count = tiles_x.checked_mul(tiles_y).ok_or_else(truncated)?;
    // Every tile needs a pointer in the file, so the count is bounded by its size.
    if count
        .checked_mul(r.ptr_size() as u64)
        .is_none_or(|bytes| bytes > r.d.len() as u64)
    {
        return Err(truncated());
    }
    let mut tiles = Vec::with_capacity(count as usize);
    let first = level + 8;
    for i in 0..count as usize {
        let offset = r.ptr(first + i * r.ptr_size())?;
        if offset == 0 {
            return Err(truncated());
        }
        tiles.push(offset);
    }
    Ok(Hierarchy {
        width,
        height,
        bpp,
        tiles,
    })
}

/// Decodes one run-length-encoded channel of `count` samples into `out[i * stride + ch]`.
/// Returns the input position after the channel.
fn rle_channel(
    src: &[u8],
    mut i: usize,
    count: usize,
    out: &mut [u8],
    stride: usize,
    ch: usize,
) -> Result<usize> {
    let mut o = 0usize;
    let byte = |i: usize| src.get(i).copied().ok_or_else(truncated);
    let short =
        |i: usize| -> Result<usize> { Ok(usize::from(byte(i)?) << 8 | usize::from(byte(i + 1)?)) };
    while o < count {
        let op = byte(i)?;
        i += 1;
        if op >= 128 {
            let n = if op == 128 {
                let n = short(i)?;
                i += 2;
                n
            } else {
                256 - usize::from(op)
            };
            if n == 0 || o + n > count {
                return Err(DecodeError::malformed("XCF RLE literal overruns the tile"));
            }
            let lit = src.get(i..i + n).ok_or_else(truncated)?;
            for (k, v) in lit.iter().enumerate() {
                out[(o + k) * stride + ch] = *v;
            }
            i += n;
            o += n;
        } else {
            let n = if op == 127 {
                let n = short(i)?;
                i += 2;
                n
            } else {
                usize::from(op) + 1
            };
            if n == 0 || o + n > count {
                return Err(DecodeError::malformed("XCF RLE run overruns the tile"));
            }
            let v = byte(i)?;
            i += 1;
            for k in 0..n {
                out[(o + k) * stride + ch] = v;
            }
            o += n;
        }
    }
    Ok(i)
}

/// Fills `out` with the `tw * th * bpp` interleaved bytes of tile `index`.
fn read_tile(
    r: &Reader<'_>,
    h: &Hierarchy,
    index: usize,
    (tw, th): (usize, usize),
    compression: u8,
    out: &mut Vec<u8>,
) -> Result<()> {
    let bytes = tw * th * h.bpp;
    out.clear();
    out.resize(bytes, 0);
    let start = h.tiles[index];
    let src = r.d.get(start..).ok_or_else(truncated)?;
    match compression {
        0 => out.copy_from_slice(src.get(..bytes).ok_or_else(truncated)?),
        1 => {
            let mut i = 0;
            for ch in 0..h.bpp {
                i = rle_channel(src, i, tw * th, out, h.bpp, ch)?;
            }
        }
        2 => {
            let inflated = decompress_to_vec_zlib_with_limit(src, bytes)
                .map_err(|_| DecodeError::malformed("XCF zlib tile invalid or too large"))?;
            if inflated.len() != bytes {
                return Err(DecodeError::malformed("XCF zlib tile has the wrong size"));
            }
            out.copy_from_slice(&inflated);
        }
        _ => return Err(DecodeError::Unsupported),
    }
    Ok(())
}

/// Normal "over" blend of a non-premultiplied source pixel onto the canvas pixel.
fn blend(dst: &mut [u8], src: [u8; 3], alpha: u32) {
    if alpha == 0 {
        return;
    }
    let da = u32::from(dst[3]);
    if da == 0 {
        dst.copy_from_slice(&[src[0], src[1], src[2], alpha as u8]);
        return;
    }
    let out_a = alpha + da * (255 - alpha) / 255;
    for c in 0..3 {
        let num = u32::from(src[c]) * alpha * 255 + u32::from(dst[c]) * da * (255 - alpha);
        dst[c] = (num / (out_a * 255)).min(255) as u8;
    }
    dst[3] = out_a.min(255) as u8;
}

struct Canvas {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

#[allow(clippy::too_many_lines)]
fn composite_layer(
    r: &Reader<'_>,
    at: usize,
    image: &Props,
    canvas: &mut Canvas,
    budget: &mut u64,
) -> Result<()> {
    let (lw, lh, ltype) = (r.u32(at)?, r.u32(at + 4)?, r.u32(at + 8)?);
    // Name: a length-prefixed string.
    let name_len = r.u32(at + 12)? as usize;
    let (props, after) = read_props(r, at + 16 + name_len)?;
    let hierarchy = r.ptr(after)?;
    let mask = r.ptr(after + r.ptr_size())?;
    if !props.visible || props.group || lw == 0 || lh == 0 {
        return Ok(());
    }
    let (bpp, indexed) = match ltype {
        0 => (3, false),
        1 => (4, false),
        2 => (1, false),
        3 => (2, false),
        4 => (1, true),
        5 => (2, true),
        _ => return Err(DecodeError::Unsupported),
    };
    let has_alpha = matches!(ltype, 1 | 3 | 5);
    let gray = matches!(ltype, 2 | 3);
    if hierarchy == 0 {
        return Ok(());
    }
    let hier = read_hierarchy(r, hierarchy)?;
    if hier.bpp != bpp || hier.width != lw || hier.height != lh {
        return Err(DecodeError::Unsupported);
    }
    let mask_hier = if mask != 0 && props.apply_mask {
        let name_len = r.u32(mask + 8)? as usize;
        let (_, after) = read_props(r, mask + 12 + name_len)?;
        let h = read_hierarchy(r, r.ptr(after)?)?;
        if h.bpp != 1 || h.width != lw || h.height != lh {
            return Err(DecodeError::Unsupported);
        }
        Some(h)
    } else {
        None
    };
    if indexed && image.colormap.is_empty() {
        return Err(DecodeError::malformed("XCF indexed image has no colormap"));
    }

    let tiles_x = u64::from(lw).div_ceil(TILE) as usize;
    let tiles_y = u64::from(lh).div_ceil(TILE) as usize;
    let (cw, ch) = (i64::from(canvas.width), i64::from(canvas.height));
    let (mut tile, mut mask_tile) = (Vec::new(), Vec::new());
    for ty in 0..tiles_y {
        for tx in 0..tiles_x {
            let (x0, y0) = (tx as u64 * TILE, ty as u64 * TILE);
            let tw = (u64::from(lw) - x0).min(TILE) as usize;
            let th = (u64::from(lh) - y0).min(TILE) as usize;
            let (gx, gy) = (props.offset.0 + x0 as i64, props.offset.1 + y0 as i64);
            // Skip tiles entirely outside the canvas without decoding them.
            if gx >= cw || gy >= ch || gx + tw as i64 <= 0 || gy + th as i64 <= 0 {
                continue;
            }
            *budget = budget
                .checked_sub((tw * th) as u64)
                .ok_or(DecodeError::TooLarge)?;
            let index = ty * tiles_x + tx;
            read_tile(r, &hier, index, (tw, th), image.compression, &mut tile)?;
            if let Some(m) = &mask_hier {
                read_tile(r, m, index, (tw, th), image.compression, &mut mask_tile)?;
            }
            for py in 0..th {
                let y = gy + py as i64;
                if y < 0 || y >= ch {
                    continue;
                }
                for px in 0..tw {
                    let x = gx + px as i64;
                    if x < 0 || x >= cw {
                        continue;
                    }
                    let s = &tile[(py * tw + px) * bpp..(py * tw + px + 1) * bpp];
                    let (rgb, a) = if indexed {
                        let c = image
                            .colormap
                            .get(usize::from(s[0]))
                            .copied()
                            .unwrap_or([0; 3]);
                        (c, if has_alpha { s[1] } else { 255 })
                    } else if gray {
                        ([s[0]; 3], if has_alpha { s[1] } else { 255 })
                    } else {
                        ([s[0], s[1], s[2]], if has_alpha { s[3] } else { 255 })
                    };
                    let mut alpha = (f32::from(a) * props.opacity + 0.5) as u32;
                    if !mask_tile.is_empty() && mask_hier.is_some() {
                        alpha = (alpha * u32::from(mask_tile[py * tw + px]) + 127) / 255;
                    }
                    let at = (y as usize * canvas.width as usize + x as usize) * 4;
                    blend(&mut canvas.pixels[at..at + 4], rgb, alpha);
                }
            }
        }
    }
    Ok(())
}

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if !data.starts_with(b"gimp xcf ") {
        return Err(DecodeError::Unsupported);
    }
    if data.len() < 26 {
        return Err(DecodeError::malformed("XCF header truncated"));
    }
    let version = match &data[9..13] {
        b"file" => 0u32,
        v if v[0] == b'v' && v[1..].iter().all(u8::is_ascii_digit) => std::str::from_utf8(&v[1..])
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        _ => return Err(DecodeError::Unsupported),
    };
    if data[13] != 0 {
        return Err(DecodeError::Unsupported);
    }
    let r = Reader {
        d: data,
        ptr64: version >= 11,
    };
    let (width, height, base_type) = (r.u32(14)?, r.u32(18)?, r.u32(22)?);
    check_dimensions(width, height)?;
    check_alloc(u64::from(width) * u64::from(height) * 4)?;
    if base_type > 2 {
        return Err(DecodeError::Unsupported);
    }
    let mut pos = 26;
    if version >= 4 {
        // Image precision: only 8-bit samples are decoded.
        if !matches!(r.u32(pos)?, 0 | 100 | 150 | 175) {
            return Err(DecodeError::Unsupported);
        }
        pos += 4;
    }
    let (props, mut pos) = read_props(&r, pos)?;

    let mut layers = Vec::new();
    for _ in 0..MAX_LAYERS + 1 {
        let ptr = r.ptr(pos)?;
        pos += r.ptr_size();
        if ptr == 0 {
            break;
        }
        if layers.len() == MAX_LAYERS {
            return Err(DecodeError::TooLarge);
        }
        layers.push(ptr);
    }

    let mut canvas = Canvas {
        width,
        height,
        pixels: pixel_buffer(width, height, 4)?,
    };
    let mut budget = MAX_TILE_PIXELS;
    // The first layer in the file is the topmost.
    for &ptr in layers.iter().rev() {
        composite_layer(&r, ptr, &props, &mut canvas, &mut budget)?;
    }
    rgba_image(width, height, canvas.pixels)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Builder {
        buf: Vec<u8>,
        ptr64: bool,
    }

    impl Builder {
        fn u32(&mut self, v: u32) {
            self.buf.extend(v.to_be_bytes());
        }

        fn slot(&mut self) -> usize {
            let at = self.buf.len();
            self.buf.extend(vec![0u8; if self.ptr64 { 8 } else { 4 }]);
            at
        }

        fn patch(&mut self, slot: usize, value: usize) {
            if self.ptr64 {
                self.buf[slot..slot + 8].copy_from_slice(&(value as u64).to_be_bytes());
            } else {
                self.buf[slot..slot + 4].copy_from_slice(&(value as u32).to_be_bytes());
            }
        }

        fn prop(&mut self, id: u32, payload: &[u8]) {
            self.u32(id);
            self.u32(payload.len() as u32);
            self.buf.extend(payload);
        }

        fn end_props(&mut self) {
            self.u32(PROP_END);
            self.u32(0);
        }
    }

    struct LayerSpec {
        ltype: u32,
        w: u32,
        h: u32,
        offset: (i32, i32),
        opacity: u32,
        visible: bool,
        /// Interleaved pixels, row-major.
        pixels: Vec<u8>,
        mask: Option<Vec<u8>>,
    }

    fn rle_encode_channel(values: &[u8]) -> Vec<u8> {
        // Alternate run and literal encodings to exercise both.
        let mut out = Vec::new();
        let mut i = 0;
        while i < values.len() {
            let n = (values.len() - i).min(100);
            if values[i..i + n].iter().all(|v| *v == values[i]) {
                out.push((n - 1) as u8);
                out.push(values[i]);
            } else {
                out.push((256 - n) as u8);
                out.extend(&values[i..i + n]);
            }
            i += n;
        }
        out
    }

    fn write_hierarchy(
        b: &mut Builder,
        w: u32,
        h: u32,
        bpp: usize,
        pixels: &[u8],
        compression: u8,
    ) -> usize {
        let start = b.buf.len();
        b.u32(w);
        b.u32(h);
        b.u32(bpp as u32);
        let level_slot = b.slot();
        let level = b.buf.len();
        b.patch(level_slot, level);
        b.u32(w);
        b.u32(h);
        let (tx, ty) = (
            u64::from(w).div_ceil(TILE) as usize,
            u64::from(h).div_ceil(TILE) as usize,
        );
        let slots: Vec<usize> = (0..tx * ty).map(|_| b.slot()).collect();
        b.patch_zero_terminator();
        for (i, slot) in slots.into_iter().enumerate() {
            let (cx, cy) = (i % tx, i / tx);
            let (x0, y0) = (cx * 64, cy * 64);
            let (tw, th) = ((w as usize - x0).min(64), (h as usize - y0).min(64));
            let mut tile = Vec::new();
            for py in 0..th {
                let row = ((y0 + py) * w as usize + x0) * bpp;
                tile.extend(&pixels[row..row + tw * bpp]);
            }
            let at = b.buf.len();
            b.patch(slot, at);
            match compression {
                0 => b.buf.extend(&tile),
                1 => {
                    for ch in 0..bpp {
                        let plane: Vec<u8> = tile.chunks(bpp).map(|p| p[ch]).collect();
                        b.buf.extend(rle_encode_channel(&plane));
                    }
                }
                _ => b
                    .buf
                    .extend(miniz_oxide::deflate::compress_to_vec_zlib(&tile, 6)),
            }
        }
        start
    }

    impl Builder {
        fn patch_zero_terminator(&mut self) {
            let n = if self.ptr64 { 8 } else { 4 };
            self.buf.extend(vec![0u8; n]);
        }
    }

    fn build(version: &str, w: u32, h: u32, compression: u8, layers: &[LayerSpec]) -> Vec<u8> {
        let ptr64 = version == "v011";
        let mut b = Builder {
            buf: Vec::new(),
            ptr64,
        };
        b.buf.extend(b"gimp xcf ");
        b.buf.extend(version.as_bytes());
        b.buf.push(0);
        b.u32(w);
        b.u32(h);
        b.u32(0); // RGB
        if version >= "v004" {
            b.u32(150);
        }
        b.prop(PROP_COMPRESSION, &[compression]);
        b.end_props();
        let slots: Vec<usize> = layers.iter().map(|_| b.slot()).collect();
        b.patch_zero_terminator();
        b.patch_zero_terminator(); // no channels
        for (slot, l) in slots.into_iter().zip(layers) {
            let at = b.buf.len();
            b.patch(slot, at);
            b.u32(l.w);
            b.u32(l.h);
            b.u32(l.ltype);
            b.u32(2);
            b.buf.extend(b"l\0");
            b.prop(PROP_OPACITY, &l.opacity.to_be_bytes());
            b.prop(PROP_VISIBLE, &u32::from(l.visible).to_be_bytes());
            let mut off = Vec::new();
            off.extend(l.offset.0.to_be_bytes());
            off.extend(l.offset.1.to_be_bytes());
            b.prop(PROP_OFFSETS, &off);
            b.end_props();
            let hslot = b.slot();
            let mslot = b.slot();
            let bpp = match l.ltype {
                0 => 3,
                1 => 4,
                2 => 1,
                _ => 2,
            };
            let hier = write_hierarchy(&mut b, l.w, l.h, bpp, &l.pixels, compression);
            b.patch(hslot, hier);
            if let Some(mask) = &l.mask {
                let at = b.buf.len();
                b.patch(mslot, at);
                b.u32(l.w);
                b.u32(l.h);
                b.u32(2);
                b.buf.extend(b"m\0");
                b.end_props();
                let hslot = b.slot();
                let hier = write_hierarchy(&mut b, l.w, l.h, 1, mask, compression);
                b.patch(hslot, hier);
            }
        }
        b.buf
    }

    fn solid(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
        (0..w * h).flat_map(|_| rgba).collect()
    }

    fn two_layers() -> Vec<LayerSpec> {
        // File order: topmost first. Top: 2x2 green RGBA at (1,1) half-opaque layer.
        // Hidden: blue full canvas. Bottom: red opaque canvas.
        vec![
            LayerSpec {
                ltype: 1,
                w: 2,
                h: 2,
                offset: (1, 1),
                opacity: 255,
                visible: true,
                pixels: solid(2, 2, [0, 255, 0, 255]),
                mask: None,
            },
            LayerSpec {
                ltype: 0,
                w: 4,
                h: 4,
                offset: (0, 0),
                opacity: 255,
                visible: false,
                pixels: (0..16).flat_map(|_| [0u8, 0, 255]).collect(),
                mask: None,
            },
            LayerSpec {
                ltype: 0,
                w: 4,
                h: 4,
                offset: (0, 0),
                opacity: 255,
                visible: true,
                pixels: (0..16).flat_map(|_| [255u8, 0, 0]).collect(),
                mask: None,
            },
        ]
    }

    #[test]
    fn composites_visible_layers_for_every_compression_and_pointer_width() {
        for version in ["v001", "v003", "v011"] {
            for compression in [0u8, 1, 2] {
                let f = build(version, 4, 4, compression, &two_layers());
                let img = decode(&f).unwrap().to_rgba8();
                assert_eq!(img.dimensions(), (4, 4), "{version} {compression}");
                assert_eq!(
                    img.get_pixel(0, 0).0,
                    [255, 0, 0, 255],
                    "{version} {compression}"
                );
                assert_eq!(
                    img.get_pixel(1, 1).0,
                    [0, 255, 0, 255],
                    "{version} {compression}"
                );
                assert_eq!(
                    img.get_pixel(2, 2).0,
                    [0, 255, 0, 255],
                    "{version} {compression}"
                );
                assert_eq!(
                    img.get_pixel(3, 3).0,
                    [255, 0, 0, 255],
                    "{version} {compression}"
                );
            }
        }
    }

    #[test]
    fn opacity_mask_and_multi_tile_layer() {
        // A 70x3 white gray-alpha layer (two tiles wide) at 50% over a transparent canvas,
        // masked out on the left half.
        let (w, h) = (70u32, 3u32);
        let pixels: Vec<u8> = (0..w * h).flat_map(|_| [255u8, 255]).collect();
        let mask: Vec<u8> = (0..w * h)
            .map(|i| if i % w < 35 { 0 } else { 255 })
            .collect();
        let layers = vec![LayerSpec {
            ltype: 3,
            w,
            h,
            offset: (0, 0),
            opacity: 128,
            visible: true,
            pixels,
            mask: Some(mask),
        }];
        let f = build("v003", w, h, 1, &layers);
        let img = decode(&f).unwrap().to_rgba8();
        assert_eq!(img.get_pixel(10, 1).0[3], 0);
        let p = img.get_pixel(60, 1).0;
        assert!(p[0] == 255 && (127..=129).contains(&p[3]), "{p:?}");
    }

    #[test]
    fn hostile_inputs_are_rejected() {
        assert_eq!(decode(b"GIF89a"), Err(DecodeError::Unsupported));
        assert!(matches!(
            decode(b"gimp xcf v003\0"),
            Err(DecodeError::Malformed(_))
        ));
        // 65535 x 65535 canvas.
        let f = build("v003", 65535, 65535, 0, &[]);
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
        // 20000 x 20000 canvas would need 1.6 GB of RGBA.
        let f = build("v003", 20000, 20000, 0, &[]);
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
        // A layer (and its hierarchy) declaring u32::MAX x u32::MAX pixels: the tile table
        // it implies cannot fit in the file.
        let mut layers = two_layers();
        layers.truncate(1);
        let mut f = build("v003", 4, 4, 0, &layers);
        let u32_at =
            |f: &[u8], at: usize| u32::from_be_bytes(f[at..at + 4].try_into().unwrap()) as usize;
        // Header 26 bytes + compression property 9 + PROP_END 8, then the layer pointer.
        let layer_at = u32_at(&f, 26 + 9 + 8);
        // Layer header 12, name 6, three properties (12 + 12 + 16), PROP_END 8, then pointers.
        let hier_at = u32_at(&f, layer_at + 12 + 6 + 12 + 12 + 16 + 8);
        for at in [layer_at, layer_at + 4, hier_at, hier_at + 4] {
            f[at..at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        }
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        // Layer count beyond the cap is refused.
        let mut f = build("v003", 4, 4, 0, &[]);
        let end = f.len();
        f.truncate(end - 8);
        for _ in 0..=MAX_LAYERS {
            f.extend(100u32.to_be_bytes());
        }
        f.extend(0u32.to_be_bytes());
        assert_eq!(decode(&f), Err(DecodeError::TooLarge));
        // Pointer to a layer outside the file.
        let mut f = build("v003", 4, 4, 0, &[]);
        let end = f.len();
        f.truncate(end - 8);
        f.extend(0xFFFF_FFF0u32.to_be_bytes());
        f.extend(0u32.to_be_bytes());
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
    }
}
