//! Utah Raster Toolkit RLE (`.rle`, magic `52 CC`): per-channel run-length opcodes over a
//! bottom-up raster, with optional background colour and colour map. 8-bit data; one
//! channel (optionally through a colour map), or RGB(A).

use image::DynamicImage;

use super::{
    DecodeError, Result, check_alloc, check_dimensions, gray_image, le16, pixel_buffer, rgb_image,
    rgba_image, slice,
};

const MAGIC: [u8; 2] = [0x52, 0xCC];
const HEADER_LEN: usize = 15;

const FLAG_CLEAR_FIRST: u8 = 0x01;
const FLAG_NO_BACKGROUND: u8 = 0x02;
const FLAG_ALPHA: u8 = 0x04;
const FLAG_COMMENT: u8 = 0x08;

const OP_SKIP_LINES: u8 = 1;
const OP_SET_COLOR: u8 = 2;
const OP_SKIP_PIXELS: u8 = 3;
const OP_BYTE_DATA: u8 = 5;
const OP_RUN_DATA: u8 = 6;
const OP_EOF: u8 = 7;
const LONG: u8 = 0x40;

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if !data.starts_with(&MAGIC) {
        return Err(DecodeError::Unsupported);
    }
    if data.len() < HEADER_LEN {
        return Err(DecodeError::malformed("RLE header truncated"));
    }
    let width = u32::from(le16(data, 6).unwrap_or(0));
    let height = u32::from(le16(data, 8).unwrap_or(0));
    let flags = data[10];
    let ncolors = usize::from(data[11]);
    let pixelbits = data[12];
    let ncmap = usize::from(data[13]);
    let cmaplen = u32::from(data[14]);
    let has_alpha = flags & FLAG_ALPHA != 0;
    if pixelbits != 8 || !matches!(ncolors, 1 | 3) || cmaplen > 8 || ncmap > 3 {
        return Err(DecodeError::Unsupported);
    }
    check_dimensions(width, height)?;
    let channels = ncolors + usize::from(has_alpha);
    check_alloc(u64::from(width) * u64::from(height) * channels as u64)?;

    let mut pos = HEADER_LEN;
    let mut background = vec![0u8; channels];
    if flags & FLAG_NO_BACKGROUND == 0 {
        let bg = slice(data, pos, ncolors)
            .ok_or_else(|| DecodeError::malformed("RLE background truncated"))?;
        background[..ncolors].copy_from_slice(bg);
        pos += ncolors;
        if ncolors % 2 == 0 {
            pos += 1;
        }
    } else {
        pos += 1;
    }
    // Colour map: `ncmap` channels of 2^cmaplen little-endian 16-bit entries.
    let entries = 1usize << cmaplen;
    let map_bytes = ncmap * entries * 2;
    let map = slice(data, pos, map_bytes)
        .ok_or_else(|| DecodeError::malformed("RLE colour map truncated"))?;
    pos += map_bytes;
    let map_value = |channel: usize, index: u8| -> u8 {
        let at = (channel * entries + usize::from(index)) * 2;
        map.get(at + 1).copied().unwrap_or(0)
    };
    if flags & FLAG_COMMENT != 0 {
        let len = usize::from(
            le16(data, pos).ok_or_else(|| DecodeError::malformed("RLE comment truncated"))?,
        );
        pos += 2 + len + (len & 1);
    }

    let (w, h) = (width as usize, height as usize);
    let mut raster = vec![0u8; w * h * channels];
    if flags & FLAG_CLEAR_FIRST != 0 && flags & FLAG_NO_BACKGROUND == 0 {
        for px in raster.chunks_exact_mut(channels) {
            px.copy_from_slice(&background);
        }
    }

    // Opcode interpreter. Each instruction consumes at least two input bytes; the number of
    // pixel writes is capped so that run opcodes cannot multiply a small file.
    let mut budget = w * h * (channels + 1) * 2 + 65_536;
    let (mut x, mut y, mut channel) = (0usize, 0usize, 0usize);
    loop {
        let op = *data
            .get(pos)
            .ok_or_else(|| DecodeError::malformed("RLE data ended without an EOF opcode"))?;
        let (operand, step) = if op & LONG != 0 {
            let v = le16(data, pos + 2)
                .ok_or_else(|| DecodeError::malformed("RLE long operand truncated"))?;
            (usize::from(v), 4)
        } else {
            let v = *data
                .get(pos + 1)
                .ok_or_else(|| DecodeError::malformed("RLE operand truncated"))?;
            (usize::from(v), 2)
        };
        pos += step;
        match op & 0x3F {
            OP_SKIP_LINES => {
                y = y.saturating_add(operand);
                x = 0;
            }
            OP_SET_COLOR => {
                // Operand 255 selects the alpha channel.
                channel = if operand == 255 { ncolors } else { operand };
                x = 0;
            }
            OP_SKIP_PIXELS => x = x.saturating_add(operand),
            OP_BYTE_DATA | OP_RUN_DATA => {
                let count = operand + 1;
                let values: &[u8];
                let run_value;
                if op & 0x3F == OP_BYTE_DATA {
                    values = slice(data, pos, count)
                        .ok_or_else(|| DecodeError::malformed("RLE byte data truncated"))?;
                    pos += count + (count & 1);
                } else {
                    run_value = [*data
                        .get(pos)
                        .ok_or_else(|| DecodeError::malformed("RLE run truncated"))?];
                    values = &run_value;
                    pos += 2;
                }
                budget = budget
                    .checked_sub(count)
                    .ok_or_else(|| DecodeError::malformed("RLE expands beyond the image budget"))?;
                if channel < channels && y < h {
                    for i in 0..count {
                        let px = x + i;
                        if px >= w {
                            break;
                        }
                        let v = if values.len() == 1 {
                            values[0]
                        } else {
                            values[i]
                        };
                        raster[(y * w + px) * channels + channel] = v;
                    }
                }
                x = x.saturating_add(count);
            }
            OP_EOF => break,
            _ => return Err(DecodeError::malformed("unknown RLE opcode")),
        }
    }

    // Convert to the output layout, applying the colour map and flipping to top-down.
    let color_channels = if ncolors == 1 && ncmap >= 3 {
        3
    } else {
        ncolors
    };
    let out_channels = color_channels + usize::from(has_alpha);
    let mut out = pixel_buffer(width, height, out_channels as u64)?;
    for (src_row, row) in raster.chunks_exact(w * channels).enumerate() {
        let y = h - 1 - src_row;
        for (x, px) in row.chunks_exact(channels).enumerate() {
            let dst = &mut out[(y * w + x) * out_channels..(y * w + x + 1) * out_channels];
            if ncolors == 1 && ncmap >= 3 {
                for (c, d) in dst.iter_mut().take(3).enumerate() {
                    *d = map_value(c, px[0]);
                }
            } else {
                for (c, d) in dst.iter_mut().take(ncolors).enumerate() {
                    *d = if ncmap >= ncolors && ncmap > 0 {
                        map_value(c, px[c])
                    } else if ncolors == 1 && ncmap == 1 {
                        map_value(0, px[0])
                    } else {
                        px[c]
                    };
                }
            }
            if has_alpha {
                dst[color_channels] = px[ncolors];
            }
        }
    }
    match (color_channels, has_alpha) {
        (1, false) => gray_image(width, height, out),
        (3, false) => rgb_image(width, height, out),
        (3, true) => rgba_image(width, height, out),
        _ => {
            let rgba = out
                .as_chunks::<2>()
                .0
                .iter()
                .flat_map(|p| [p[0], p[0], p[0], p[1]])
                .collect();
            rgba_image(width, height, rgba)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(w: u16, h: u16, flags: u8, ncolors: u8, ncmap: u8, cmaplen: u8) -> Vec<u8> {
        let mut f = vec![0x52, 0xCC, 0, 0, 0, 0];
        f.extend(w.to_le_bytes());
        f.extend(h.to_le_bytes());
        f.extend([flags, ncolors, 8, ncmap, cmaplen]);
        f
    }

    fn op(code: u8, operand: u8) -> Vec<u8> {
        vec![code, operand]
    }

    fn byte_data(values: &[u8]) -> Vec<u8> {
        let mut v = op(OP_BYTE_DATA, (values.len() - 1) as u8);
        v.extend(values);
        if !values.len().is_multiple_of(2) {
            v.push(0);
        }
        v
    }

    #[test]
    fn rgb_with_runs_skips_and_bottom_up_rows() {
        // No background (pad byte follows). 3x2; bottom line: red run; top line: skip 1 then
        // green via byte data, blue via run using a long skip-pixels instruction.
        let mut f = header(3, 2, FLAG_NO_BACKGROUND, 3, 0, 0);
        f.push(0);
        f.extend(op(OP_SET_COLOR, 0));
        f.extend(op(OP_RUN_DATA, 2));
        f.extend([255, 0]);
        f.extend(op(OP_SKIP_LINES, 1));
        f.extend(op(OP_SET_COLOR, 1));
        // Long skip pixels by 1 (opcode | LONG, pad, 16-bit operand).
        f.extend([OP_SKIP_PIXELS | LONG, 0, 1, 0]);
        f.extend(byte_data(&[200, 100]));
        f.extend(op(OP_SET_COLOR, 2));
        f.extend(op(OP_RUN_DATA, 0));
        f.extend([77, 0]);
        f.extend(op(OP_EOF, 0));
        let img = decode(&f).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (3, 2));
        // Bottom file line is the bottom image row.
        assert_eq!(img.get_pixel(0, 1).0, [255, 0, 0]);
        assert_eq!(img.get_pixel(2, 1).0, [255, 0, 0]);
        assert_eq!(img.get_pixel(0, 0).0, [0, 0, 77]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 200, 0]);
        assert_eq!(img.get_pixel(2, 0).0, [0, 100, 0]);
    }

    #[test]
    fn color_mapped_single_channel_with_background() {
        // Background index 1, colour map of 2 entries (2^1) for 3 channels.
        let mut f = header(2, 1, FLAG_CLEAR_FIRST, 1, 3, 1);
        f.push(1); // background index (one colour: no pad byte)
        for channel in 0..3u16 {
            for entry in 0..2u16 {
                let v = if channel == 0 { entry * 0xFF00 } else { 0 };
                f.extend(v.to_le_bytes());
            }
        }
        f.extend(op(OP_SET_COLOR, 0));
        f.extend(byte_data(&[0]));
        f.extend(op(OP_EOF, 0));
        let img = decode(&f).unwrap().to_rgb8();
        assert_eq!(img.get_pixel(0, 0).0, [0, 0, 0]);
        // Second pixel keeps the background index 1 -> red entry.
        assert_eq!(img.get_pixel(1, 0).0, [255, 0, 0]);
    }

    #[test]
    fn hostile_inputs_are_rejected() {
        assert_eq!(decode(b"BM"), Err(DecodeError::Unsupported));
        assert!(matches!(
            decode(&[0x52, 0xCC, 0]),
            Err(DecodeError::Malformed(_))
        ));
        assert_eq!(
            decode(&header(65535, 65535, 2, 3, 0, 0)),
            Err(DecodeError::TooLarge)
        );
        // Data ends without EOF.
        let mut f = header(2, 1, FLAG_NO_BACKGROUND, 1, 0, 0);
        f.push(0);
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
        // Run opcodes may not multiply a tiny file into a huge number of writes.
        let mut f = header(60000, 1000, FLAG_NO_BACKGROUND, 1, 0, 0);
        f.push(0);
        for _ in 0..5000 {
            f.extend([OP_RUN_DATA | LONG, 0, 0xFF, 0xFF, 1, 0]);
        }
        f.extend(op(OP_EOF, 0));
        assert!(matches!(decode(&f), Err(DecodeError::Malformed(_))));
    }
}
