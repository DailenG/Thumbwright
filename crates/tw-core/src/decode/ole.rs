//! Legacy Office and other OLE compound-file documents: the thumbnail stored in the
//! `\u{5}SummaryInformation` property set (MS-OLEPS, property id 17 `PIDSI_THUMBNAIL`,
//! type `VT_CF`). The container is read by a small bounded Compound File Binary reader
//! (MS-CFB): every sector id, chain length, stream size, entry count and property-set
//! offset is checked against the bytes actually present before anything is allocated,
//! chains are cycle-checked, and the directory tree is walked iteratively.
//!
//! Office only writes the thumbnail when "Save thumbnail" is enabled, so many real files
//! carry none (`Unsupported`). A `CF_DIB`, PNG or JPEG payload is decoded in-process
//! ([`extract`]); metafile payloads (`CF_METAFILEPICT`, `CF_ENHMETAFILE`) are handed out
//! by [`extract_metafile`] for GDI playback in the worker (`Decoder::OleMetafile`).

use image::{DynamicImage, ImageFormat};

use super::{DecodeError, Result, check_alloc, check_dimensions, decode_bytes, decode_limited};
use crate::limits;

pub const EXTENSIONS: &[&str] = &[
    "doc", "dot", "xls", "xlt", "ppt", "pps", "pot", "vsd", "pub",
];

const SIGNATURE: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
const END_OF_CHAIN: u32 = 0xFFFF_FFFE;
const NO_STREAM: u32 = 0xFFFF_FFFF;
const HEADER_SIZE: usize = 512;
const ENTRY_SIZE: usize = 128;
const MAX_DIR_ENTRIES: usize = 1 << 16;
const MINI_SECTOR_SIZE: usize = 64;

const SUMMARY_INFORMATION: &str = "\u{5}SummaryInformation";
const PIDSI_THUMBNAIL: u32 = 17;
const VT_CF: u16 = 0x47;
const CF_METAFILEPICT: u32 = 3;
const CF_DIB: u32 = 8;
const CF_ENHMETAFILE: u32 = 14;

fn bad(msg: &'static str) -> DecodeError {
    DecodeError::malformed(msg)
}

fn le_u16(d: &[u8], off: usize) -> Option<u16> {
    let b = d.get(off..off.checked_add(2)?)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

fn le_u32(d: &[u8], off: usize) -> Option<u32> {
    let b = d.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn le_u64(d: &[u8], off: usize) -> Option<u64> {
    let b = d.get(off..off.checked_add(8)?)?;
    let mut a = [0u8; 8];
    a.copy_from_slice(b);
    Some(u64::from_le_bytes(a))
}

/// Visited-set over `n` ids, used to detect chain cycles.
struct Seen(Vec<u64>);

impl Seen {
    fn new(n: usize) -> Self {
        Self(vec![0; n.div_ceil(64)])
    }

    /// Marks `id`; returns false when it was already marked (or out of range).
    fn mark(&mut self, id: usize) -> bool {
        match self.0.get_mut(id / 64) {
            Some(w) => {
                let bit = 1u64 << (id % 64);
                let fresh = *w & bit == 0;
                *w |= bit;
                fresh
            }
            None => false,
        }
    }
}

struct Entry {
    name: String,
    kind: u8,
    left: u32,
    right: u32,
    child: u32,
    start: u32,
    size: u64,
}

struct Cfb<'a> {
    data: &'a [u8],
    sector_shift: u32,
    sector_count: u32,
    fat_sectors: Vec<u32>,
    first_dir: u32,
    first_minifat: u32,
    minifat_count: u32,
    mini_cutoff: u64,
    v3: bool,
}

impl<'a> Cfb<'a> {
    fn open(data: &'a [u8]) -> Result<Self> {
        if data.len() < SIGNATURE.len() || data[..SIGNATURE.len()] != SIGNATURE {
            return Err(DecodeError::Unsupported);
        }
        if data.len() < HEADER_SIZE {
            return Err(bad("truncated compound file header"));
        }
        let major = le_u16(data, 0x1A).ok_or_else(|| bad("header"))?;
        let sector_shift = u32::from(le_u16(data, 0x1E).ok_or_else(|| bad("header"))?);
        let mini_shift = le_u16(data, 0x20).ok_or_else(|| bad("header"))?;
        let v3 = match (major, sector_shift) {
            (3, 9) => true,
            (4, 12) => false,
            _ => return Err(bad("unsupported compound file version or sector size")),
        };
        if le_u16(data, 0x1C) != Some(0xFFFE) || mini_shift != 6 {
            return Err(bad(
                "unsupported compound file byte order or mini sector size",
            ));
        }
        let sector_size = 1usize << sector_shift;
        if data.len() < sector_size {
            return Err(bad("truncated compound file header"));
        }
        let sector_count = u32::try_from((data.len() - sector_size) / sector_size)
            .map_err(|_| DecodeError::TooLarge)?;

        let nfat = le_u32(data, 0x2C).ok_or_else(|| bad("header"))?;
        if nfat == 0 || nfat > sector_count {
            return Err(bad("FAT sector count exceeds the file"));
        }
        let mut fat_sectors = Vec::with_capacity(nfat as usize);
        for i in 0..109usize {
            if fat_sectors.len() == nfat as usize {
                break;
            }
            fat_sectors.push(le_u32(data, 0x4C + 4 * i).ok_or_else(|| bad("header"))?);
        }
        if fat_sectors.len() < nfat as usize {
            let ndifat = le_u32(data, 0x48).ok_or_else(|| bad("header"))?;
            let mut next = le_u32(data, 0x44).ok_or_else(|| bad("header"))?;
            let mut hops = 0u32;
            let mut seen = Seen::new(sector_count as usize);
            let per_sector = sector_size / 4 - 1;
            while fat_sectors.len() < nfat as usize {
                hops += 1;
                if hops > ndifat || next >= sector_count || !seen.mark(next as usize) {
                    return Err(bad("DIFAT chain is cyclic, short or out of range"));
                }
                let off = (next as usize + 1) << sector_shift;
                for i in 0..per_sector {
                    if fat_sectors.len() == nfat as usize {
                        break;
                    }
                    fat_sectors.push(le_u32(data, off + 4 * i).ok_or_else(|| bad("DIFAT"))?);
                }
                next = le_u32(data, off + sector_size - 4).ok_or_else(|| bad("DIFAT"))?;
            }
        }
        if fat_sectors.iter().any(|&s| s >= sector_count) {
            return Err(bad("FAT sector id out of range"));
        }
        Ok(Self {
            data,
            sector_shift,
            sector_count,
            fat_sectors,
            first_dir: le_u32(data, 0x30).ok_or_else(|| bad("header"))?,
            first_minifat: le_u32(data, 0x3C).ok_or_else(|| bad("header"))?,
            minifat_count: le_u32(data, 0x40).ok_or_else(|| bad("header"))?,
            mini_cutoff: u64::from(le_u32(data, 0x38).ok_or_else(|| bad("header"))?),
            v3,
        })
    }

    fn sector_size(&self) -> usize {
        1usize << self.sector_shift
    }

    fn sector(&self, id: u32) -> Result<&'a [u8]> {
        if id >= self.sector_count {
            return Err(bad("sector id out of range"));
        }
        let off = (id as usize + 1) << self.sector_shift;
        off.checked_add(self.sector_size())
            .and_then(|end| self.data.get(off..end))
            .ok_or_else(|| bad("sector beyond end of file"))
    }

    fn fat_next(&self, id: u32) -> Result<u32> {
        let per = (self.sector_size() / 4) as u32;
        let fat_sector = self
            .fat_sectors
            .get((id / per) as usize)
            .ok_or_else(|| bad("sector id beyond the FAT"))?;
        let sector = self.sector(*fat_sector)?;
        le_u32(sector, (id % per) as usize * 4).ok_or_else(|| bad("FAT"))
    }

    /// Follows a FAT chain. `size` is the declared stream size (the chain must hold it);
    /// `None` reads to the end of the chain. `max_bytes` caps the result.
    fn read_chain(&self, start: u32, size: Option<u64>, max_bytes: u64) -> Result<Vec<u8>> {
        let ss = self.sector_size() as u64;
        let max_sectors = match size {
            Some(s) => {
                if s > max_bytes {
                    return Err(DecodeError::TooLarge);
                }
                let n = s.div_ceil(ss);
                if n > u64::from(self.sector_count) {
                    return Err(bad("stream larger than the file"));
                }
                n
            }
            None => (max_bytes / ss).min(u64::from(self.sector_count)),
        };
        let mut out = Vec::new();
        if size.is_some() {
            out.reserve((max_sectors * ss) as usize);
        }
        let mut seen = Seen::new(self.sector_count as usize);
        let mut id = start;
        let mut n = 0u64;
        while n < max_sectors && id != END_OF_CHAIN {
            if !seen.mark(id as usize) {
                return Err(bad("cyclic or out-of-range sector chain"));
            }
            out.extend_from_slice(self.sector(id)?);
            n += 1;
            id = self.fat_next(id)?;
        }
        match size {
            Some(s) => {
                if n < max_sectors {
                    return Err(bad("stream chain shorter than its declared size"));
                }
                out.truncate(s as usize);
            }
            None if id != END_OF_CHAIN => return Err(bad("sector chain too long")),
            None => {}
        }
        Ok(out)
    }

    fn directory(&self) -> Result<Vec<Entry>> {
        let raw = self.read_chain(self.first_dir, None, (MAX_DIR_ENTRIES * ENTRY_SIZE) as u64)?;
        let mut entries = Vec::with_capacity(raw.len() / ENTRY_SIZE);
        for rec in raw.as_chunks::<ENTRY_SIZE>().0 {
            let rec: &[u8] = rec;
            let name_len = usize::from(le_u16(rec, 64).ok_or_else(|| bad("entry"))?).min(64);
            let units: Vec<u16> = rec[..name_len.saturating_sub(2)]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect();
            let size = le_u64(rec, 120).ok_or_else(|| bad("entry"))?;
            entries.push(Entry {
                name: String::from_utf16_lossy(&units),
                kind: rec[66],
                left: le_u32(rec, 68).ok_or_else(|| bad("entry"))?,
                right: le_u32(rec, 72).ok_or_else(|| bad("entry"))?,
                child: le_u32(rec, 76).ok_or_else(|| bad("entry"))?,
                start: le_u32(rec, 116).ok_or_else(|| bad("entry"))?,
                // Version 3 files only define the low 32 bits.
                size: if self.v3 { size & 0xFFFF_FFFF } else { size },
            });
        }
        if entries.first().map(|e| e.kind) != Some(5) {
            return Err(bad("missing root directory entry"));
        }
        Ok(entries)
    }

    /// A stream directly inside the root storage.
    fn root_stream<'e>(entries: &'e [Entry], name: &str) -> Result<Option<&'e Entry>> {
        let mut seen = Seen::new(entries.len());
        let mut stack = vec![entries[0].child];
        while let Some(id) = stack.pop() {
            if id == NO_STREAM {
                continue;
            }
            let entry = entries
                .get(id as usize)
                .ok_or_else(|| bad("directory entry id out of range"))?;
            if !seen.mark(id as usize) {
                return Err(bad("cyclic directory tree"));
            }
            if entry.kind == 2 && entry.name == name {
                return Ok(Some(entry));
            }
            stack.push(entry.left);
            stack.push(entry.right);
        }
        Ok(None)
    }

    fn read_stream(&self, entries: &[Entry], entry: &Entry) -> Result<Vec<u8>> {
        let cap = limits::ARCHIVE_ENTRY_MAX_BYTES;
        if entry.size >= self.mini_cutoff {
            return self.read_chain(entry.start, Some(entry.size), cap);
        }
        let ministream = self.read_chain(entries[0].start, Some(entries[0].size), cap)?;
        let minifat_bytes = u64::from(self.minifat_count)
            .checked_mul(self.sector_size() as u64)
            .ok_or(DecodeError::TooLarge)?;
        let minifat = self.read_chain(self.first_minifat, Some(minifat_bytes), cap)?;
        let mini_count = ministream.len() / MINI_SECTOR_SIZE;
        let wanted = usize::try_from(entry.size.div_ceil(MINI_SECTOR_SIZE as u64))
            .map_err(|_| DecodeError::TooLarge)?;
        if wanted > mini_count {
            return Err(bad("mini stream larger than the mini container"));
        }
        let mut out = Vec::with_capacity(wanted * MINI_SECTOR_SIZE);
        let mut seen = Seen::new(mini_count);
        let mut id = entry.start;
        for _ in 0..wanted {
            if id == END_OF_CHAIN {
                return Err(bad("mini stream chain shorter than its declared size"));
            }
            let idx = id as usize;
            if idx >= mini_count || !seen.mark(idx) {
                return Err(bad("cyclic or out-of-range mini sector chain"));
            }
            out.extend_from_slice(
                &ministream[idx * MINI_SECTOR_SIZE..(idx + 1) * MINI_SECTOR_SIZE],
            );
            id = le_u32(&minifat, idx * 4).ok_or_else(|| bad("mini FAT too short"))?;
        }
        out.truncate(entry.size as usize);
        Ok(out)
    }
}

/// The payload of the `PIDSI_THUMBNAIL` property: a `CLIPDATA` structure.
/// Returns `(windows clipboard format if any, data)`.
fn thumbnail_clipdata(props: &[u8]) -> Result<Option<(Option<u32>, &[u8])>> {
    // Property set stream header: byte order, version, OS, CLSID, set count, then
    // (FMTID, offset) per set; the first set is SummaryInformation.
    if le_u16(props, 0) != Some(0xFFFE) {
        return Err(bad("property set byte order"));
    }
    let nsets = le_u32(props, 24).ok_or_else(|| bad("property set header"))?;
    if nsets == 0 || nsets > 2 {
        return Err(bad("property set count"));
    }
    let set = le_u32(props, 44).ok_or_else(|| bad("property set header"))? as usize;
    let set_data = props.get(set..).ok_or_else(|| bad("property set offset"))?;
    let count = le_u32(set_data, 4).ok_or_else(|| bad("property set"))? as usize;
    // The (id, offset) table must fit in the set.
    if count > set_data.len().saturating_sub(8) / 8 {
        return Err(bad("property count exceeds the property set"));
    }
    for i in 0..count {
        let at = 8 + 8 * i;
        if le_u32(set_data, at) != Some(PIDSI_THUMBNAIL) {
            continue;
        }
        let off = le_u32(set_data, at + 4).ok_or_else(|| bad("property table"))? as usize;
        let value = set_data.get(off..).ok_or_else(|| bad("property offset"))?;
        if le_u16(value, 0) != Some(VT_CF) {
            return Ok(None);
        }
        // CLIPDATA: size (covers format + data), format tag, data.
        let size = le_u32(value, 4).ok_or_else(|| bad("thumbnail"))? as usize;
        let body = value
            .get(8..)
            .and_then(|v| v.get(..size))
            .ok_or_else(|| bad("thumbnail size exceeds the property"))?;
        let tag = le_u32(body, 0).ok_or_else(|| bad("thumbnail format"))?;
        let rest = &body[4..];
        return Ok(match tag {
            // Built-in Windows format: its value precedes the data.
            0xFFFF_FFFF => {
                let cf = le_u32(rest, 0).ok_or_else(|| bad("thumbnail clipboard format"))?;
                Some((Some(cf), &rest[4..]))
            }
            // Macintosh format value, or an FMTID: not a Windows image.
            0xFFFF_FFFE => Some((None, &rest[rest.len().min(4)..])),
            0xFFFF_FFFD => Some((None, &rest[rest.len().min(16)..])),
            0 => None,
            // Named format: `tag` bytes of name precede the data.
            n => Some((None, rest.get(n as usize..).unwrap_or(&[]))),
        });
    }
    Ok(None)
}

/// Wraps a packed DIB (`BITMAPINFOHEADER` + palette + bits) in a BMP file header after
/// validating its declared geometry against the bytes present.
fn dib_to_bmp(dib: &[u8]) -> Result<Vec<u8>> {
    let hs = le_u32(dib, 0).ok_or_else(|| bad("DIB header"))? as usize;
    let (w, h, bpp, compression, colors_used, palette_entry) = if hs == 12 {
        (
            i32::from(le_u16(dib, 4).ok_or_else(|| bad("DIB header"))?),
            i32::from(le_u16(dib, 6).ok_or_else(|| bad("DIB header"))?),
            u32::from(le_u16(dib, 10).ok_or_else(|| bad("DIB header"))?),
            0u32,
            0u32,
            3usize,
        )
    } else if hs >= 40 {
        (
            le_u32(dib, 4).ok_or_else(|| bad("DIB header"))? as i32,
            le_u32(dib, 8).ok_or_else(|| bad("DIB header"))? as i32,
            u32::from(le_u16(dib, 14).ok_or_else(|| bad("DIB header"))?),
            le_u32(dib, 16).ok_or_else(|| bad("DIB header"))?,
            le_u32(dib, 32).ok_or_else(|| bad("DIB header"))?,
            4usize,
        )
    } else {
        return Err(bad("unsupported DIB header size"));
    };
    let width = u32::try_from(w).map_err(|_| bad("negative DIB width"))?;
    let height = h.unsigned_abs();
    check_dimensions(width, height)?;
    check_alloc(u64::from(width) * u64::from(height) * 4)?;
    if !matches!(bpp, 1 | 4 | 8 | 16 | 24 | 32) {
        return Err(bad("unsupported DIB bit depth"));
    }
    let palette = if bpp <= 8 {
        let n = if colors_used == 0 {
            1u32 << bpp
        } else {
            colors_used
        };
        if n > 256 {
            return Err(bad("DIB palette too large"));
        }
        n as usize * palette_entry
    } else {
        0
    };
    let masks = match (hs, compression) {
        (40, 3) => 12,
        (40, 6) => 16,
        _ => 0,
    };
    let bits_offset = hs
        .checked_add(masks)
        .and_then(|v| v.checked_add(palette))
        .ok_or_else(|| bad("DIB offsets"))?;
    // Uncompressed pixels must all be present.
    if matches!(compression, 0 | 3 | 6) {
        let stride = (u64::from(width) * u64::from(bpp)).div_ceil(32) * 4;
        let need = stride
            .checked_mul(u64::from(height))
            .and_then(|v| v.checked_add(bits_offset as u64))
            .ok_or_else(|| bad("DIB size"))?;
        if need > dib.len() as u64 {
            return Err(bad("DIB pixel data truncated"));
        }
    }
    if bits_offset > dib.len() {
        return Err(bad("DIB header beyond the data"));
    }
    let file_size = u32::try_from(14 + dib.len()).map_err(|_| DecodeError::TooLarge)?;
    let mut bmp = Vec::with_capacity(14 + dib.len());
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&file_size.to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&(14 + bits_offset as u32).to_le_bytes());
    bmp.extend_from_slice(dib);
    Ok(bmp)
}

/// Offset of a WMF stream (placeable key or `METAHEADER`) inside a `CF_METAFILEPICT`
/// payload, which carries a few bytes of `METAFILEPICT` before the bits.
fn wmf_offset(data: &[u8]) -> Option<usize> {
    (0..=32).step_by(2).find(|&at| {
        le_u32(data, at) == Some(0x9AC6_CDD7)
            || (matches!(le_u16(data, at), Some(1 | 2))
                && le_u16(data, at + 2) == Some(9)
                && matches!(le_u16(data, at + 4), Some(0x0100 | 0x0300)))
    })
}

fn decode_thumbnail(format: Option<u32>, data: &[u8]) -> Result<DynamicImage> {
    match format {
        Some(CF_DIB) => decode_limited(&dib_to_bmp(data)?, ImageFormat::Bmp).map(|(img, _)| img),
        // Rendering metafiles needs GDI, which only a worker may use: see `extract_metafile`.
        Some(CF_METAFILEPICT | CF_ENHMETAFILE) => Err(DecodeError::Unsupported),
        // Other or unnamed formats: only an embedded PNG or JPEG is accepted.
        _ => match image::guess_format(data) {
            Ok(ImageFormat::Png | ImageFormat::Jpeg) => decode_bytes(data),
            _ => Err(DecodeError::Unsupported),
        },
    }
}

/// The `PIDSI_THUMBNAIL` clipboard format and payload (copied out of the property stream).
fn thumbnail_property(data: &[u8]) -> Result<(Option<u32>, Vec<u8>)> {
    let cfb = Cfb::open(data)?;
    let entries = cfb.directory()?;
    let Some(summary) = Cfb::root_stream(&entries, SUMMARY_INFORMATION)? else {
        return Err(DecodeError::Unsupported);
    };
    let props = cfb.read_stream(&entries, summary)?;
    let Some((format, payload)) = thumbnail_clipdata(&props)? else {
        return Err(DecodeError::Unsupported);
    };
    Ok((format, payload.to_vec()))
}

/// The bitmap thumbnail (`CF_DIB`, PNG or JPEG). A metafile thumbnail is `Unsupported`
/// here: see [`extract_metafile`].
pub fn extract(data: &[u8]) -> Result<DynamicImage> {
    let (format, payload) = thumbnail_property(data)?;
    decode_thumbnail(format, &payload)
}

/// The EMF/WMF stream of a metafile thumbnail (`CF_ENHMETAFILE`, `CF_METAFILEPICT`), for
/// GDI playback in the worker. `Unsupported` when the thumbnail is absent or a bitmap.
pub fn extract_metafile(data: &[u8]) -> Result<Vec<u8>> {
    let (format, mut payload) = thumbnail_property(data)?;
    match format {
        Some(CF_ENHMETAFILE) => Ok(payload),
        Some(CF_METAFILEPICT) => {
            let at = wmf_offset(&payload).ok_or(DecodeError::Unsupported)?;
            payload.drain(..at);
            Ok(payload)
        }
        _ => Err(DecodeError::Unsupported),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};
    use std::io::Cursor;

    const SECTOR: usize = 512;

    fn utf16_name(name: &str) -> ([u8; 64], u16) {
        let mut out = [0u8; 64];
        let mut n = 0;
        for u in name.encode_utf16() {
            out[n..n + 2].copy_from_slice(&u.to_le_bytes());
            n += 2;
        }
        (out, (n + 2) as u16)
    }

    fn dir_entry(name: &str, kind: u8, right: u32, child: u32, start: u32, size: u64) -> Vec<u8> {
        let mut e = vec![0u8; ENTRY_SIZE];
        let (nm, len) = utf16_name(name);
        e[..64].copy_from_slice(&nm);
        e[64..66].copy_from_slice(&len.to_le_bytes());
        e[66] = kind;
        e[67] = 1;
        e[68..72].copy_from_slice(&NO_STREAM.to_le_bytes());
        e[72..76].copy_from_slice(&right.to_le_bytes());
        e[76..80].copy_from_slice(&child.to_le_bytes());
        e[116..120].copy_from_slice(&start.to_le_bytes());
        e[120..128].copy_from_slice(&size.to_le_bytes());
        e
    }

    /// Builds a version 3 compound file (one FAT sector, so at most 128 sectors) holding
    /// `streams` directly under the root. Streams under 4096 bytes go in the mini stream.
    fn build_cfb(streams: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let nstreams = streams.len();
        let dir_sectors = (1 + nstreams).div_ceil(4);
        // Mini stream layout.
        let mut mini = Vec::new();
        let mut mini_fat: Vec<u32> = Vec::new();
        let mut mini_start = vec![0u32; nstreams];
        for (i, (_, d)) in streams.iter().enumerate() {
            if d.len() < 4096 {
                mini_start[i] = mini_fat.len() as u32;
                let n = d.len().div_ceil(64).max(1);
                for k in 0..n {
                    mini_fat.push(if k + 1 == n {
                        END_OF_CHAIN
                    } else {
                        mini_fat.len() as u32 + 1
                    });
                }
                mini.extend_from_slice(d);
                mini.resize(mini_fat.len() * 64, 0);
            }
        }
        let minifat_sectors = (mini_fat.len() * 4).div_ceil(SECTOR);
        let ministream_sectors = mini.len().div_ceil(SECTOR);
        // Sector plan: 0 FAT, dir, minifat, ministream, large streams.
        let mut fat: Vec<u32> = vec![0xFFFF_FFFD];
        let chain = |fat: &mut Vec<u32>, n: usize| -> u32 {
            let start = fat.len() as u32;
            for k in 0..n {
                fat.push(if k + 1 == n {
                    END_OF_CHAIN
                } else {
                    fat.len() as u32 + 1
                });
            }
            if n == 0 { END_OF_CHAIN } else { start }
        };
        let dir_start = chain(&mut fat, dir_sectors);
        let minifat_start = chain(&mut fat, minifat_sectors);
        let ministream_start = chain(&mut fat, ministream_sectors);
        let mut starts = vec![0u32; nstreams];
        for (i, (_, d)) in streams.iter().enumerate() {
            starts[i] = if d.len() < 4096 {
                mini_start[i]
            } else {
                chain(&mut fat, d.len().div_ceil(SECTOR))
            };
        }
        assert!(fat.len() <= 128, "test builder supports one FAT sector");

        let mut file = vec![0u8; SECTOR];
        file[..8].copy_from_slice(&SIGNATURE);
        file[0x18..0x1A].copy_from_slice(&0x3Eu16.to_le_bytes());
        file[0x1A..0x1C].copy_from_slice(&3u16.to_le_bytes());
        file[0x1C..0x1E].copy_from_slice(&0xFFFEu16.to_le_bytes());
        file[0x1E..0x20].copy_from_slice(&9u16.to_le_bytes());
        file[0x20..0x22].copy_from_slice(&6u16.to_le_bytes());
        file[0x2C..0x30].copy_from_slice(&1u32.to_le_bytes());
        file[0x30..0x34].copy_from_slice(&dir_start.to_le_bytes());
        file[0x38..0x3C].copy_from_slice(&4096u32.to_le_bytes());
        file[0x3C..0x40].copy_from_slice(&minifat_start.to_le_bytes());
        file[0x40..0x44].copy_from_slice(&(minifat_sectors as u32).to_le_bytes());
        file[0x44..0x48].copy_from_slice(&END_OF_CHAIN.to_le_bytes());
        for i in 0..109 {
            let v = if i == 0 { 0 } else { NO_STREAM };
            file[0x4C + 4 * i..0x50 + 4 * i].copy_from_slice(&v.to_le_bytes());
        }
        // FAT sector.
        let mut fat_bytes: Vec<u8> = fat.iter().flat_map(|v| v.to_le_bytes()).collect();
        fat_bytes.resize(SECTOR, 0xFF);
        file.extend_from_slice(&fat_bytes);
        // Directory: root, then streams chained through right siblings.
        let mut dir = dir_entry(
            "Root Entry",
            5,
            NO_STREAM,
            if nstreams > 0 { 1 } else { NO_STREAM },
            if ministream_sectors > 0 {
                ministream_start
            } else {
                END_OF_CHAIN
            },
            mini.len() as u64,
        );
        for (i, (name, d)) in streams.iter().enumerate() {
            let right = if i + 1 < nstreams {
                i as u32 + 2
            } else {
                NO_STREAM
            };
            dir.extend(dir_entry(
                name,
                2,
                right,
                NO_STREAM,
                starts[i],
                d.len() as u64,
            ));
        }
        dir.resize(dir_sectors * SECTOR, 0);
        for e in dir
            .as_chunks_mut::<ENTRY_SIZE>()
            .0
            .iter_mut()
            .skip(1 + nstreams)
        {
            e[68..80].fill(0xFF);
        }
        file.extend_from_slice(&dir);
        let mut mf: Vec<u8> = mini_fat.iter().flat_map(|v| v.to_le_bytes()).collect();
        mf.resize(minifat_sectors * SECTOR, 0xFF);
        file.extend_from_slice(&mf);
        mini.resize(ministream_sectors * SECTOR, 0);
        file.extend_from_slice(&mini);
        for (_, d) in streams.iter().filter(|(_, d)| d.len() >= 4096) {
            let mut padded = d.clone();
            padded.resize(d.len().div_ceil(SECTOR) * SECTOR, 0);
            file.extend_from_slice(&padded);
        }
        file
    }

    /// SummaryInformation stream with an unrelated property and the thumbnail property.
    fn summary(clipdata: &[u8]) -> Vec<u8> {
        let mut s = Vec::new();
        s.extend_from_slice(&0xFFFEu16.to_le_bytes());
        s.extend_from_slice(&0u16.to_le_bytes());
        s.extend_from_slice(&0u32.to_le_bytes());
        s.extend_from_slice(&[0u8; 16]);
        s.extend_from_slice(&1u32.to_le_bytes());
        s.extend_from_slice(&[0xE0, 0x85, 0x9F, 0xF2, 0xF9, 0x4F, 0x68, 0x10]);
        s.extend_from_slice(&[0xAB, 0x91, 0x08, 0x00, 0x2B, 0x27, 0xB3, 0xD9]);
        s.extend_from_slice(&48u32.to_le_bytes());
        // Property set at 48: size, count, table, values.
        let mut values = Vec::new();
        let table_len = 8 + 2 * 8;
        let int_off = table_len;
        values.extend_from_slice(&0x03u32.to_le_bytes()); // VT_I4
        values.extend_from_slice(&1252u32.to_le_bytes());
        let cf_off = table_len + values.len();
        values.extend_from_slice(&u32::from(VT_CF).to_le_bytes());
        values.extend_from_slice(clipdata);
        while values.len() % 4 != 0 {
            values.push(0);
        }
        let mut set = Vec::new();
        set.extend_from_slice(&((table_len + values.len()) as u32).to_le_bytes());
        set.extend_from_slice(&2u32.to_le_bytes());
        set.extend_from_slice(&1u32.to_le_bytes());
        set.extend_from_slice(&(int_off as u32).to_le_bytes());
        set.extend_from_slice(&PIDSI_THUMBNAIL.to_le_bytes());
        set.extend_from_slice(&(cf_off as u32).to_le_bytes());
        set.extend_from_slice(&values);
        s.extend_from_slice(&set);
        s
    }

    /// CLIPDATA with a built-in Windows clipboard format.
    fn clip(cf: u32, data: &[u8]) -> Vec<u8> {
        let mut c = Vec::new();
        c.extend_from_slice(&((8 + data.len()) as u32).to_le_bytes());
        c.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        c.extend_from_slice(&cf.to_le_bytes());
        c.extend_from_slice(data);
        c
    }

    /// Bottom-up 24-bit DIB; `px(x, y)` returns RGB with y counted from the top.
    fn dib24(w: u32, h: u32, px: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
        let stride = (w * 3).div_ceil(4) * 4;
        let mut d = Vec::new();
        d.extend_from_slice(&40u32.to_le_bytes());
        d.extend_from_slice(&(w as i32).to_le_bytes());
        d.extend_from_slice(&(h as i32).to_le_bytes());
        d.extend_from_slice(&1u16.to_le_bytes());
        d.extend_from_slice(&24u16.to_le_bytes());
        d.extend_from_slice(&[0u8; 24]);
        for y in (0..h).rev() {
            let mut row = Vec::new();
            for x in 0..w {
                let [r, g, b] = px(x, y);
                row.extend_from_slice(&[b, g, r]);
            }
            row.resize(stride as usize, 0);
            d.extend_from_slice(&row);
        }
        d
    }

    fn png(w: u32, h: u32, c: [u8; 4]) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        RgbaImage::from_pixel(w, h, Rgba(c))
            .write_to(&mut out, ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    fn red_blue_dib(w: u32, h: u32) -> Vec<u8> {
        dib24(
            w,
            h,
            |x, _| if x < w / 2 { [255, 0, 0] } else { [0, 0, 255] },
        )
    }

    #[test]
    fn dib_thumbnail_in_a_mini_stream() {
        let file = build_cfb(&[(
            "\u{5}SummaryInformation",
            summary(&clip(8, &red_blue_dib(4, 2))),
        )]);
        let img = extract(&file).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (4, 2));
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(3, 1).0, [0, 0, 255, 255]);
    }

    #[test]
    fn dib_thumbnail_in_a_regular_stream_among_other_streams() {
        let big = dib24(60, 40, |x, y| [x as u8 * 4, y as u8 * 6, 9]);
        let file = build_cfb(&[
            ("WordDocument", vec![7; 600]),
            ("\u{5}SummaryInformation", summary(&clip(8, &big))),
            ("1Table", vec![1; 5000]),
        ]);
        let img = extract(&file).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (60, 40));
        assert_eq!(img.get_pixel(10, 5).0, [40, 30, 9, 255]);
    }

    #[test]
    fn png_and_jpeg_payloads_are_decoded() {
        let file = build_cfb(&[(
            "\u{5}SummaryInformation",
            summary(&clip(0x1234, &png(5, 3, [10, 200, 30, 255]))),
        )]);
        let img = extract(&file).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (5, 3));
        assert_eq!(img.get_pixel(2, 1).0, [10, 200, 30, 255]);

        let mut jpg = Cursor::new(Vec::new());
        RgbImage::from_pixel(16, 8, Rgb([250, 10, 10]))
            .write_to(&mut jpg, ImageFormat::Jpeg)
            .unwrap();
        let file = build_cfb(&[(
            "\u{5}SummaryInformation",
            summary(&clip(0x1234, &jpg.into_inner())),
        )]);
        let img = extract(&file).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (16, 8));
        assert!(img.get_pixel(8, 4).0[0] > 200);
    }

    #[test]
    fn documents_without_a_usable_thumbnail_are_unsupported() {
        let no_summary = build_cfb(&[("WordDocument", vec![0; 100])]);
        assert_eq!(extract(&no_summary), Err(DecodeError::Unsupported));
        let metafile = build_cfb(&[(
            "\u{5}SummaryInformation",
            summary(&clip(CF_ENHMETAFILE, &[0; 64])),
        )]);
        assert_eq!(extract(&metafile), Err(DecodeError::Unsupported));
        let wmf = build_cfb(&[(
            "\u{5}SummaryInformation",
            summary(&clip(CF_METAFILEPICT, &[0; 64])),
        )]);
        assert_eq!(extract(&wmf), Err(DecodeError::Unsupported));
        assert_eq!(
            extract(b"{\\rtf1 not a compound file}"),
            Err(DecodeError::Unsupported)
        );
        assert_eq!(extract(b""), Err(DecodeError::Unsupported));
    }

    #[test]
    fn metafile_thumbnails_are_handed_out_for_gdi_playback() {
        let emf = [1u8, 0, 0, 0, 0xAA, 0xBB];
        let doc = build_cfb(&[(
            "\u{5}SummaryInformation",
            summary(&clip(CF_ENHMETAFILE, &emf)),
        )]);
        assert_eq!(extract_metafile(&doc).unwrap(), emf);
        // CF_METAFILEPICT: a few METAFILEPICT bytes precede the (placeable) WMF bits.
        let mut pict = vec![8u8, 0, 0, 0, 0, 0, 0, 0];
        pict.extend_from_slice(&0x9AC6_CDD7u32.to_le_bytes());
        pict.extend_from_slice(&[7; 10]);
        let doc = build_cfb(&[(
            "\u{5}SummaryInformation",
            summary(&clip(CF_METAFILEPICT, &pict)),
        )]);
        assert_eq!(extract_metafile(&doc).unwrap(), pict[8..]);
        // No WMF header inside the picture payload, a bitmap thumbnail, or no thumbnail.
        let junk = build_cfb(&[(
            "\u{5}SummaryInformation",
            summary(&clip(CF_METAFILEPICT, &[0; 64])),
        )]);
        assert_eq!(extract_metafile(&junk), Err(DecodeError::Unsupported));
        let dib = build_cfb(&[(
            "\u{5}SummaryInformation",
            summary(&clip(CF_DIB, &red_blue_dib(4, 2))),
        )]);
        assert_eq!(extract_metafile(&dib), Err(DecodeError::Unsupported));
        assert_eq!(extract_metafile(b""), Err(DecodeError::Unsupported));
    }

    #[cfg(windows)]
    #[test]
    fn emf_thumbnail_in_a_compound_file_renders_through_gdi() {
        use super::super::metafile;
        let emf = metafile::tests::emf_with_red_left_half();
        let doc = build_cfb(&[(
            "\u{5}SummaryInformation",
            summary(&clip(CF_ENHMETAFILE, &emf)),
        )]);
        let payload = extract_metafile(&doc).unwrap();
        let img = metafile::render(&payload, 64).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (64, 32));
        assert_eq!(img.get_pixel(2, 16).0[..3], [255, 0, 0]);
    }

    fn assert_rejected(data: &[u8]) {
        match extract(data) {
            Err(DecodeError::Malformed(_) | DecodeError::TooLarge | DecodeError::Unsupported) => {}
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[test]
    fn truncated_files_are_rejected() {
        let file = build_cfb(&[(
            "\u{5}SummaryInformation",
            summary(&clip(8, &red_blue_dib(4, 2))),
        )]);
        assert!(matches!(
            extract(&file[..100]),
            Err(DecodeError::Malformed(_))
        ));
        for cut in [512, 600, 1024, file.len() - 1000] {
            assert_rejected(&file[..cut.min(file.len())]);
        }
    }

    #[test]
    fn fat_cycle_is_malformed() {
        let big = dib24(60, 40, |_, _| [1, 2, 3]);
        let mut file = build_cfb(&[("\u{5}SummaryInformation", summary(&clip(8, &big)))]);
        // Sector 0 is the FAT; sector 1 the directory; the stream starts at sector 2
        // (no mini stream here). Make its second sector point back at its first.
        let fat = SECTOR; // file offset of sector 0
        let first = 2u32;
        file[fat + 4 * (first as usize + 1)..][..4].copy_from_slice(&first.to_le_bytes());
        assert!(matches!(extract(&file), Err(DecodeError::Malformed(_))));
    }

    #[test]
    fn mini_fat_cycle_is_malformed() {
        let mut file = build_cfb(&[(
            "\u{5}SummaryInformation",
            summary(&clip(8, &red_blue_dib(4, 2))),
        )]);
        // Sectors: 0 FAT, 1 directory, 2 mini FAT, 3.. mini stream. Mini sector 0 -> 0.
        let minifat = SECTOR * 3;
        file[minifat..minifat + 4].copy_from_slice(&0u32.to_le_bytes());
        assert!(matches!(extract(&file), Err(DecodeError::Malformed(_))));
    }

    #[test]
    fn lying_header_and_sizes_are_rejected_without_large_allocation() {
        let good = build_cfb(&[(
            "\u{5}SummaryInformation",
            summary(&clip(8, &red_blue_dib(4, 2))),
        )]);

        let mut f = good.clone();
        f[0x2C..0x30].copy_from_slice(&u32::MAX.to_le_bytes()); // FAT sector count
        assert!(matches!(extract(&f), Err(DecodeError::Malformed(_))));

        let mut f = good.clone();
        f[0x1E..0x20].copy_from_slice(&31u16.to_le_bytes()); // sector shift
        assert!(matches!(extract(&f), Err(DecodeError::Malformed(_))));

        let mut f = good.clone();
        f[0x30..0x34].copy_from_slice(&0x00FF_FFF0u32.to_le_bytes()); // directory start
        assert!(matches!(extract(&f), Err(DecodeError::Malformed(_))));

        // Declared stream size of 4 GiB - 1 in the SummaryInformation entry (entry 1).
        let mut f = good.clone();
        let entry1 = SECTOR * 2 + ENTRY_SIZE;
        f[entry1 + 120..entry1 + 128].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(matches!(
            extract(&f),
            Err(DecodeError::Malformed(_) | DecodeError::TooLarge)
        ));

        // Huge DIFAT chain declared with too few FAT ids in the header.
        let mut f = good.clone();
        f[0x2C..0x30].copy_from_slice(&110u32.to_le_bytes());
        f[0x48..0x4C].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_rejected(&f);
    }

    #[test]
    fn lying_property_set_sizes_are_rejected() {
        let stream = |mutate: &dyn Fn(&mut Vec<u8>)| {
            let mut s = summary(&clip(8, &red_blue_dib(4, 2)));
            mutate(&mut s);
            build_cfb(&[("\u{5}SummaryInformation", s)])
        };
        // Property count far beyond the set.
        let f = stream(&|s| s[52..56].copy_from_slice(&u32::MAX.to_le_bytes()));
        assert!(matches!(extract(&f), Err(DecodeError::Malformed(_))));
        // Property offset past the end.
        let f =
            stream(&|s| s[48 + 8 + 8 + 4..48 + 8 + 8 + 8].copy_from_slice(&u32::MAX.to_le_bytes()));
        assert!(matches!(extract(&f), Err(DecodeError::Malformed(_))));
        // CLIPDATA size beyond the stream.
        let f = stream(&|s| {
            let pos = 48 + 8 + 16 + 8 + 4; // value start + type(4) -> size field
            s[pos..pos + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        });
        assert!(matches!(extract(&f), Err(DecodeError::Malformed(_))));
    }

    #[test]
    fn hostile_dib_geometry_is_rejected_before_allocating() {
        let mut huge = red_blue_dib(4, 2);
        huge[4..8].copy_from_slice(&65535i32.to_le_bytes());
        huge[8..12].copy_from_slice(&65535i32.to_le_bytes());
        let f = build_cfb(&[("\u{5}SummaryInformation", summary(&clip(8, &huge)))]);
        assert_eq!(extract(&f), Err(DecodeError::TooLarge));

        let mut max = red_blue_dib(4, 2);
        max[4..8].copy_from_slice(&i32::MAX.to_le_bytes());
        max[8..12].copy_from_slice(&i32::MAX.to_le_bytes());
        let f = build_cfb(&[("\u{5}SummaryInformation", summary(&clip(8, &max)))]);
        assert_eq!(extract(&f), Err(DecodeError::TooLarge));

        // Plausible geometry but pixel rows missing.
        let mut short = red_blue_dib(4, 2);
        short.truncate(short.len() - 8);
        let f = build_cfb(&[("\u{5}SummaryInformation", summary(&clip(8, &short)))]);
        assert!(matches!(extract(&f), Err(DecodeError::Malformed(_))));

        let f = build_cfb(&[("\u{5}SummaryInformation", summary(&clip(8, &[1, 0])))]);
        assert!(matches!(extract(&f), Err(DecodeError::Malformed(_))));
    }
}
