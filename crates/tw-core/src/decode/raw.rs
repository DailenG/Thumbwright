//! Camera RAW: the largest embedded JPEG preview that decodes.
//!
//! Containers understood:
//! - TIFF-structured raws (standard `II*\0`/`MM\0*` plus the Olympus `IIRO`/`IIRS`/`MMOR`
//!   and Panasonic `IIU\0` magics): IFD chain, SubIFDs, the EXIF IFD and common maker
//!   notes (Nikon embedded TIFF, Olympus, Pentax `AOC\0`, plain-IFD maker notes).
//!   Candidates are JPEGInterchangeFormat/Length pairs, single-run JPEG strips and the
//!   Panasonic JpgFromRaw blob (tag 0x002E).
//! - Fujifilm RAF (`FUJIFILMCCD-RAW`): JPEG offset/length in the fixed header.
//! - Canon CR3 (ISO BMFF, brand `crx `): `PRVW`/`THMB` boxes in the metadata boxes ahead
//!   of `mdat`; orientation from the `CMT1` TIFF.
//! - Canon CRW (CIFF heaps): JpgFromRaw record 0x2007.
//! - Minolta MRW: the TIFF inside the `TTW` block.
//! - Sigma X3F: JPEG payloads of the image sections listed in the `SECd` directory.
//!
//! The raw sensor data itself is never decoded.

use std::collections::HashSet;
use std::ops::Range;

use image::metadata::Orientation;
use image::{DynamicImage, ImageFormat};

use super::{DecodeError, Result, decode_limited};

pub const EXTENSIONS: &[&str] = &[
    "3fr", "arw", "cr2", "cr3", "crw", "dcr", "dcs", "dng", "drf", "erf", "fff", "iiq", "k25",
    "kdc", "mef", "mos", "mrw", "nef", "nrw", "orf", "ori", "pef", "ptx", "raf", "rw2", "rwl",
    "sr2", "srf", "srw", "x3f",
];

/// IFDs visited per file (loops and fan-out guard).
const MAX_IFDS: usize = 64;
/// Entries read per IFD.
const MAX_IFD_ENTRIES: usize = 4096;
/// Values read from an offset array (SubIFDs, strips).
const MAX_ARRAY: usize = 1024;
/// Candidates kept per file.
const MAX_CANDIDATES: usize = 64;
/// Container boxes / heaps / sections walked.
const MAX_BOXES: usize = 4096;
/// How far into a preview box the JPEG start marker is searched.
const JPEG_SEARCH_WINDOW: usize = 64;

/// TIFF tags.
const TAG_RW2_JPG_FROM_RAW: u16 = 0x002E;
const TAG_COMPRESSION: u16 = 0x0103;
const TAG_STRIP_OFFSETS: u16 = 0x0111;
const TAG_ORIENTATION: u16 = 0x0112;
const TAG_STRIP_BYTE_COUNTS: u16 = 0x0117;
const TAG_SUB_IFDS: u16 = 0x014A;
const TAG_JPEG_OFFSET: u16 = 0x0201;
const TAG_JPEG_LENGTH: u16 = 0x0202;
const TAG_EXIF_IFD: u16 = 0x8769;
const TAG_MAKER_NOTE: u16 = 0x927C;

/// Maker-note (start, length) tag pairs: Minolta, Olympus camera settings, Pentax.
const MAKERNOTE_PAIRS: &[(u16, u16)] = &[
    (TAG_JPEG_OFFSET, TAG_JPEG_LENGTH),
    (0x0088, 0x0089),
    (0x0101, 0x0102),
    (0x0004, 0x0003),
];
/// Maker-note tags holding a JPEG blob inline (Minolta/old Olympus thumbnail).
const MAKERNOTE_BLOBS: &[u16] = &[0x0081];
/// Maker-note tags pointing at a nested IFD (Nikon PreviewIFD, Olympus CameraSettings).
const MAKERNOTE_SUBIFDS: &[u16] = &[0x0011, 0x2020];

pub fn extract(data: &[u8]) -> Result<DynamicImage> {
    let mut found = Found::default();
    if data.starts_with(b"FUJIFILMCCD-RAW") {
        raf(data, &mut found);
    } else if data.get(4..8) == Some(b"ftyp") && data.get(8..12) == Some(b"crx ") {
        cr3(data, &mut found);
    } else if data.starts_with(b"\0MRM") {
        mrw(data, &mut found);
    } else if data.get(6..14) == Some(b"HEAPCCDR") {
        crw(data, &mut found);
    } else if data.starts_with(b"FOVb") {
        x3f(data, &mut found);
    } else if let Some((tiff, first)) = Tiff::open(data, 0) {
        let mut walker = Walker::new(data, &mut found);
        walker.walk(tiff, first, false);
    } else {
        return Err(DecodeError::Unsupported);
    }
    found.best(data)
}

#[derive(Default)]
struct Found {
    candidates: Vec<Range<usize>>,
    orientation: Option<u16>,
}

impl Found {
    /// Records `len` bytes at absolute `start`, clamped to the file; out-of-file starts
    /// are dropped.
    fn add(&mut self, data: &[u8], start: usize, len: usize) {
        if len == 0 || start >= data.len() || self.candidates.len() >= MAX_CANDIDATES {
            return;
        }
        let end = start.saturating_add(len).min(data.len());
        if data[start..end].starts_with(&[0xFF, 0xD8]) {
            self.candidates.push(start..end);
        }
    }

    /// Records a JPEG found within the first bytes of `range` (a preview box payload).
    fn add_searched(&mut self, data: &[u8], range: Range<usize>) {
        let Some(body) = data.get(range.clone()) else {
            return;
        };
        let window = &body[..body.len().min(JPEG_SEARCH_WINDOW)];
        if let Some(i) = window.windows(3).position(|w| w == [0xFF, 0xD8, 0xFF]) {
            self.add(data, range.start + i, body.len() - i);
        }
    }

    /// Largest declared preview first; the first that decodes wins.
    fn best(mut self, data: &[u8]) -> Result<DynamicImage> {
        self.candidates.sort_by_key(|r| r.start);
        self.candidates.dedup_by_key(|r| r.start);
        let mut sized: Vec<(u64, Range<usize>)> = self
            .candidates
            .into_iter()
            .filter_map(|r| {
                let (w, h) = jpeg_dimensions(&data[r.clone()])?;
                Some((u64::from(w) * u64::from(h), r))
            })
            .collect();
        sized.sort_by_key(|s| std::cmp::Reverse(s.0));
        let mut last = DecodeError::Unsupported;
        for (_, r) in sized {
            match decode_jpeg(&data[r], self.orientation) {
                Ok(img) => return Ok(img),
                Err(e) if e.falls_through() => last = e,
                Err(e) => return Err(e),
            }
        }
        Err(last)
    }
}

/// Decodes a preview, applying its own EXIF orientation or, when it has none, the
/// orientation recorded by the container.
fn decode_jpeg(bytes: &[u8], fallback: Option<u16>) -> Result<DynamicImage> {
    let (mut img, own) = decode_limited(bytes, ImageFormat::Jpeg)?;
    let orientation = match own {
        Some(o) if o != Orientation::NoTransforms => Some(o),
        _ => fallback
            .and_then(|v| u8::try_from(v).ok())
            .and_then(Orientation::from_exif),
    };
    if let Some(o) = orientation {
        img.apply_orientation(o);
    }
    Ok(img)
}

/// Frame size from the first SOF marker; `None` for lossless (raw sensor) JPEGs, which
/// the `image` decoder cannot handle.
fn jpeg_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    if !b.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut i = 2usize;
    for _ in 0..1024 {
        if *b.get(i)? != 0xFF {
            return None;
        }
        let mut m = *b.get(i + 1)?;
        while m == 0xFF {
            i += 1;
            m = *b.get(i + 1)?;
        }
        match m {
            0x01 | 0xD0..=0xD8 => {
                i += 2;
                continue;
            }
            0xD9 | 0xDA => return None,
            0xC3 | 0xC7 | 0xCB | 0xCF => return None,
            0xC0..=0xCF if !matches!(m, 0xC4 | 0xC8 | 0xCC) => {
                let h = u16::from_be_bytes([*b.get(i + 5)?, *b.get(i + 6)?]);
                let w = u16::from_be_bytes([*b.get(i + 7)?, *b.get(i + 8)?]);
                return (w > 0 && h > 0).then_some((u32::from(w), u32::from(h)));
            }
            _ => {}
        }
        let len = usize::from(u16::from_be_bytes([*b.get(i + 2)?, *b.get(i + 3)?]));
        i = i.checked_add(2 + len)?;
    }
    None
}

fn be32(data: &[u8], at: usize) -> Option<u32> {
    let b = data.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn le32(data: &[u8], at: usize) -> Option<u32> {
    let b = data.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

// ---------------------------------------------------------------------------------------
// TIFF

/// A TIFF structure whose offsets are relative to `origin` (absolute file position).
#[derive(Clone, Copy)]
struct Tiff {
    origin: usize,
    le: bool,
}

struct Entry {
    tag: u16,
    typ: u16,
    count: u32,
    /// Relative offset of the value bytes (inline field or pointed-to data).
    value_at: usize,
    value_len: usize,
}

impl Tiff {
    /// Recognises a TIFF header at absolute `origin`; returns the first IFD offset.
    fn open(data: &[u8], origin: usize) -> Option<(Self, u32)> {
        let le = match data.get(origin..origin.checked_add(2)?)? {
            b"II" => true,
            b"MM" => false,
            _ => return None,
        };
        let t = Self { origin, le };
        // 42: TIFF; 0x4F52 / 0x5352: Olympus ORF; 0x55: Panasonic RW2.
        if !matches!(t.u16(data, 2)?, 42 | 0x4F52 | 0x5352 | 0x55) {
            return None;
        }
        Some((t, t.u32(data, 4)?))
    }

    fn u16(&self, data: &[u8], rel: usize) -> Option<u16> {
        let at = self.origin.checked_add(rel)?;
        let b = data.get(at..at.checked_add(2)?)?;
        Some(if self.le {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        })
    }

    fn u32(&self, data: &[u8], rel: usize) -> Option<u32> {
        let at = self.origin.checked_add(rel)?;
        let b = data.get(at..at.checked_add(4)?)?;
        Some(if self.le {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        })
    }

    fn entry(&self, data: &[u8], rel: usize) -> Option<Entry> {
        let tag = self.u16(data, rel)?;
        let typ = self.u16(data, rel + 2)?;
        let count = self.u32(data, rel + 4)?;
        let unit: usize = match typ {
            1 | 2 | 6 | 7 => 1,
            3 | 8 => 2,
            4 | 9 | 11 | 13 => 4,
            5 | 10 | 12 => 8,
            _ => return None,
        };
        let value_len = unit.checked_mul(count as usize)?;
        let value_at = if value_len <= 4 {
            rel + 8
        } else {
            self.u32(data, rel + 8)? as usize
        };
        Some(Entry {
            tag,
            typ,
            count,
            value_at,
            value_len,
        })
    }

    /// Integer values of a SHORT/LONG/IFD entry (capped).
    fn values(&self, data: &[u8], e: &Entry) -> Vec<u32> {
        let n = (e.count as usize).min(MAX_ARRAY);
        (0..n)
            .map_while(|i| match e.typ {
                3 => self.u16(data, e.value_at + i * 2).map(u32::from),
                4 | 13 => self.u32(data, e.value_at + i * 4),
                _ => None,
            })
            .collect()
    }

    fn first(&self, data: &[u8], e: &Entry) -> Option<u32> {
        self.values(data, e).first().copied()
    }

    /// Absolute range of an entry's value bytes.
    fn blob(&self, e: &Entry) -> (usize, usize) {
        (self.origin.saturating_add(e.value_at), e.value_len)
    }
}

struct Walker<'a, 'f> {
    data: &'a [u8],
    found: &'f mut Found,
    visited: HashSet<usize>,
    makernotes: usize,
}

impl<'a, 'f> Walker<'a, 'f> {
    fn new(data: &'a [u8], found: &'f mut Found) -> Self {
        Self {
            data,
            found,
            visited: HashSet::new(),
            makernotes: 0,
        }
    }

    /// Walks the IFD tree rooted at relative offset `first`.
    fn walk(&mut self, t: Tiff, first: u32, makernote: bool) {
        let data = self.data;
        let mut queue = vec![first];
        let mut is_ifd0 = !makernote;
        let mut pending_makernotes = Vec::new();
        while let Some(off) = queue.pop() {
            let off = off as usize;
            let Some(abs) = t.origin.checked_add(off) else {
                continue;
            };
            if off == 0 || abs >= data.len() || self.visited.len() >= MAX_IFDS {
                continue;
            }
            if !self.visited.insert(abs) {
                continue; // IFD loop
            }
            let Some(count) = t.u16(data, off) else {
                continue;
            };
            let count = usize::from(count).min(MAX_IFD_ENTRIES);

            let mut compression = None;
            let mut strips: Vec<u32> = Vec::new();
            let mut strip_counts: Vec<u32> = Vec::new();
            let mut jpeg_pairs: Vec<(u16, u32)> = Vec::new();
            for i in 0..count {
                let Some(e) = t.entry(data, off + 2 + i * 12) else {
                    continue;
                };
                match e.tag {
                    TAG_COMPRESSION => compression = t.first(data, &e),
                    TAG_STRIP_OFFSETS => strips = t.values(data, &e),
                    TAG_STRIP_BYTE_COUNTS => strip_counts = t.values(data, &e),
                    TAG_ORIENTATION if is_ifd0 && self.found.orientation.is_none() => {
                        self.found.orientation =
                            t.first(data, &e).and_then(|v| u16::try_from(v).ok());
                    }
                    TAG_SUB_IFDS | TAG_EXIF_IFD if !makernote => {
                        queue.extend(t.values(data, &e));
                    }
                    TAG_RW2_JPG_FROM_RAW if !makernote => {
                        let (start, len) = t.blob(&e);
                        self.found.add(data, start, len);
                    }
                    TAG_MAKER_NOTE if !makernote => pending_makernotes.push(t.blob(&e)),
                    tag if makernote && MAKERNOTE_BLOBS.contains(&tag) => {
                        let (start, len) = t.blob(&e);
                        self.found.add(data, start, len);
                    }
                    tag if makernote && MAKERNOTE_SUBIFDS.contains(&tag) => {
                        // Either an IFD pointer or an UNDEFINED blob holding the IFD.
                        let target = if e.typ == 7 {
                            Some(e.value_at as u32)
                        } else {
                            t.first(data, &e)
                        };
                        queue.extend(target);
                    }
                    tag => {
                        let in_pairs =
                            |pairs: &[(u16, u16)]| pairs.iter().any(|&(a, b)| a == tag || b == tag);
                        let relevant = if makernote {
                            in_pairs(MAKERNOTE_PAIRS)
                        } else {
                            tag == TAG_JPEG_OFFSET || tag == TAG_JPEG_LENGTH
                        };
                        if relevant && let Some(v) = t.first(data, &e) {
                            jpeg_pairs.push((tag, v));
                        }
                    }
                }
            }
            let pairs: &[(u16, u16)] = if makernote {
                MAKERNOTE_PAIRS
            } else {
                &[(TAG_JPEG_OFFSET, TAG_JPEG_LENGTH)]
            };
            for &(start_tag, len_tag) in pairs {
                let get = |tag| jpeg_pairs.iter().find(|p| p.0 == tag).map(|p| p.1);
                if let (Some(start), Some(len)) = (get(start_tag), get(len_tag)) {
                    self.found
                        .add(data, t.origin.saturating_add(start as usize), len as usize);
                }
            }
            // JPEG-compressed strips stored as one contiguous run.
            if matches!(compression, Some(6 | 7))
                && !strips.is_empty()
                && strips.len() == strip_counts.len()
            {
                let contiguous = strips
                    .windows(2)
                    .zip(&strip_counts)
                    .all(|(w, &c)| u64::from(w[0]) + u64::from(c) == u64::from(w[1]));
                if contiguous {
                    let total: u64 = strip_counts.iter().map(|&c| u64::from(c)).sum();
                    let start = t.origin.saturating_add(strips[0] as usize);
                    self.found
                        .add(data, start, usize::try_from(total).unwrap_or(usize::MAX));
                }
            }
            if let Some(next) = t.u32(data, off + 2 + count * 12) {
                queue.push(next);
            }
            is_ifd0 = false;
        }
        for (start, len) in pending_makernotes {
            self.makernote(t, start, len);
        }
    }

    fn makernote(&mut self, t: Tiff, start: usize, len: usize) {
        self.makernotes += 1;
        if self.makernotes > 4 || len < 8 {
            return;
        }
        let data = self.data;
        let Some(note) = data.get(start..start.saturating_add(len).min(data.len())) else {
            return;
        };
        if note.starts_with(b"Nikon\0") {
            // Nikon type 2/3: a complete TIFF header follows the 10-byte prefix.
            if let Some((inner, first)) = Tiff::open(data, start + 10) {
                self.walk(inner, first, true);
            }
        } else if note.starts_with(b"OLYMPUS\0") || note.starts_with(b"OM SYSTEM") {
            // New Olympus: own byte order, offsets relative to the maker note.
            let prefix = if note.starts_with(b"OLYMPUS\0") {
                8
            } else {
                12
            };
            let le = match note.get(prefix..prefix + 2) {
                Some(b"II") => true,
                Some(b"MM") => false,
                _ => return,
            };
            let first = (prefix + 4) as u32;
            self.walk(Tiff { origin: start, le }, first, true);
        } else if note.starts_with(b"OLYMP\0") {
            self.walk(t, (start - t.origin + 8) as u32, true);
        } else if note.starts_with(b"AOC\0") {
            let le = match &note[4..6] {
                b"II" => true,
                b"MM" => false,
                _ => t.le,
            };
            self.walk(
                Tiff {
                    origin: t.origin,
                    le,
                },
                (start - t.origin + 6) as u32,
                true,
            );
        } else {
            // Plain IFD maker note (Minolta and others) using the parent's offsets.
            self.walk(t, (start - t.origin) as u32, true);
        }
    }
}

// ---------------------------------------------------------------------------------------
// Other containers

fn raf(data: &[u8], found: &mut Found) {
    if let (Some(off), Some(len)) = (be32(data, 84), be32(data, 88)) {
        found.add(data, off as usize, len as usize);
    }
}

/// ISO BMFF box header at `pos` within `end`: (type, body range, next box position).
fn bmff_box(data: &[u8], pos: usize, end: usize) -> Option<([u8; 4], Range<usize>, usize)> {
    let size = be32(data, pos)? as u64;
    let mut typ = [0u8; 4];
    typ.copy_from_slice(data.get(pos + 4..pos + 8)?);
    let (header, size) = match size {
        0 => (8u64, (end - pos) as u64),
        1 => {
            let hi = u64::from(be32(data, pos + 8)?);
            let lo = u64::from(be32(data, pos + 12)?);
            (16, (hi << 32) | lo)
        }
        s => (8, s),
    };
    if size < header {
        return None;
    }
    let box_end = (pos as u64).checked_add(size)?.min(end as u64) as usize;
    let body = pos + header as usize..box_end;
    (body.start <= body.end).then_some((typ, body, box_end))
}

fn cr3(data: &[u8], found: &mut Found) {
    let mut pos = 0usize;
    for _ in 0..MAX_BOXES {
        let Some((typ, body, next)) = bmff_box(data, pos, data.len()) else {
            break;
        };
        if &typ == b"mdat" {
            // Previews and metadata precede the media data in CR3.
            break;
        }
        scan_cr3_metadata(data, body, found);
        if next <= pos {
            break;
        }
        pos = next;
    }
}

/// Finds `PRVW`/`THMB` preview boxes and the `CMT1` TIFF anywhere inside a top-level box.
fn scan_cr3_metadata(data: &[u8], range: Range<usize>, found: &mut Found) {
    let Some(region) = data.get(range.clone()) else {
        return;
    };
    let mut hits = 0usize;
    for i in 4..region.len().saturating_sub(4) {
        let tag = &region[i..i + 4];
        if tag != b"PRVW" && tag != b"THMB" && tag != b"CMT1" {
            continue;
        }
        hits += 1;
        if hits > MAX_CANDIDATES {
            break;
        }
        let abs = range.start + i - 4;
        let Some((_, body, _)) = bmff_box(data, abs, range.end) else {
            continue;
        };
        if tag == b"CMT1" {
            if found.orientation.is_none()
                && let Some((tiff, first)) = Tiff::open(data, body.start)
                && let Some(n) = tiff.u16(data, first as usize)
            {
                for k in 0..usize::from(n).min(MAX_IFD_ENTRIES) {
                    let Some(e) = tiff.entry(data, first as usize + 2 + k * 12) else {
                        break;
                    };
                    if e.tag == TAG_ORIENTATION {
                        found.orientation =
                            tiff.first(data, &e).and_then(|v| u16::try_from(v).ok());
                    }
                }
            }
        } else {
            found.add_searched(data, body);
        }
    }
}

fn mrw(data: &[u8], found: &mut Found) {
    let Some(len) = be32(data, 4) else { return };
    let end = (8u64 + u64::from(len)).min(data.len() as u64) as usize;
    let mut pos = 8usize;
    for _ in 0..MAX_BOXES {
        let (Some(name), Some(size)) = (data.get(pos..pos + 4), be32(data, pos + 4)) else {
            break;
        };
        if pos + 8 > end {
            break;
        }
        if name == b"\0TTW" {
            if let Some((tiff, first)) = Tiff::open(data, pos + 8) {
                Walker::new(data, found).walk(tiff, first, false);
            }
            return;
        }
        pos = match pos.checked_add(8 + size as usize) {
            Some(p) if p < end => p,
            _ => break,
        };
    }
}

fn crw(data: &[u8], found: &mut Found) {
    let le = match data.get(..2) {
        Some(b"II") => true,
        Some(b"MM") => false,
        _ => return,
    };
    let u32_at = |at: usize| if le { le32(data, at) } else { be32(data, at) };
    let Some(header_len) = u32_at(2) else { return };
    let mut heaps = 0usize;
    crw_heap(
        data,
        le,
        header_len as usize,
        data.len(),
        0,
        &mut heaps,
        found,
    );
}

const CIFF_JPG_FROM_RAW: u16 = 0x2007;

fn crw_heap(
    data: &[u8],
    le: bool,
    start: usize,
    end: usize,
    depth: usize,
    heaps: &mut usize,
    found: &mut Found,
) {
    *heaps += 1;
    if depth > 8 || *heaps > MAX_BOXES || end <= start || end < 4 || end > data.len() {
        return;
    }
    let u16_at = |at: usize| {
        let b = data.get(at..at + 2)?;
        Some(if le {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        })
    };
    let u32_at = |at: usize| if le { le32(data, at) } else { be32(data, at) };
    let Some(table) = u32_at(end - 4).and_then(|o| start.checked_add(o as usize)) else {
        return;
    };
    let Some(count) = u16_at(table) else { return };
    for i in 0..usize::from(count).min(MAX_IFD_ENTRIES) {
        let at = table + 2 + i * 10;
        let (Some(tag), Some(size), Some(off)) = (u16_at(at), u32_at(at + 2), u32_at(at + 6))
        else {
            break;
        };
        if tag >> 14 != 0 {
            continue; // value stored inside the record itself
        }
        let Some(rec_start) = start.checked_add(off as usize) else {
            continue;
        };
        let rec_end = rec_start.saturating_add(size as usize).min(end);
        match tag & 0x3800 {
            0x2800 | 0x3000 => {
                crw_heap(data, le, rec_start, rec_end, depth + 1, heaps, found);
            }
            _ if tag & 0x3FFF == CIFF_JPG_FROM_RAW => {
                found.add(data, rec_start, rec_end.saturating_sub(rec_start));
            }
            _ => {}
        }
    }
}

fn x3f(data: &[u8], found: &mut Found) {
    let Some(dir) = data.len().checked_sub(4).and_then(|at| le32(data, at)) else {
        return;
    };
    let dir = dir as usize;
    if data.get(dir..dir.saturating_add(4)) != Some(b"SECd") {
        return;
    }
    let Some(count) = le32(data, dir + 8) else {
        return;
    };
    for i in 0..(count as usize).min(MAX_BOXES) {
        let at = dir + 12 + i * 12;
        let (Some(off), Some(len), Some(kind)) = (
            le32(data, at),
            le32(data, at + 4),
            data.get(at + 8..at + 12),
        ) else {
            break;
        };
        if kind == b"IMA2" || kind == b"IMAG" {
            let start = off as usize;
            let end = start.saturating_add(len as usize).min(data.len());
            if start < end {
                found.add_searched(data, start..end);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};
    use std::io::Cursor;

    fn jpeg(w: u32, h: u32, color: [u8; 3]) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        RgbImage::from_pixel(w, h, Rgb(color))
            .write_to(&mut out, ImageFormat::Jpeg)
            .unwrap();
        out.into_inner()
    }

    fn close(a: [u8; 3], b: [u8; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 8)
    }

    /// Little-endian TIFF builder with IFDs placed at caller-chosen offsets.
    struct Lt(Vec<u8>);

    impl Lt {
        fn new(magic: &[u8; 4], len: usize) -> Self {
            let mut v = vec![0u8; len];
            v[..4].copy_from_slice(magic);
            v[4..8].copy_from_slice(&8u32.to_le_bytes());
            Self(v)
        }

        /// Entries are (tag, type, count, value-or-offset).
        fn ifd(&mut self, at: usize, entries: &[(u16, u16, u32, u32)], next: u32) {
            let v = &mut self.0;
            v[at..at + 2].copy_from_slice(&(entries.len() as u16).to_le_bytes());
            for (i, &(tag, typ, count, val)) in entries.iter().enumerate() {
                let e = at + 2 + i * 12;
                v[e..e + 2].copy_from_slice(&tag.to_le_bytes());
                v[e + 2..e + 4].copy_from_slice(&typ.to_le_bytes());
                v[e + 4..e + 8].copy_from_slice(&count.to_le_bytes());
                v[e + 8..e + 12].copy_from_slice(&val.to_le_bytes());
            }
            let n = at + 2 + entries.len() * 12;
            v[n..n + 4].copy_from_slice(&next.to_le_bytes());
        }

        fn blob(&mut self, at: usize, bytes: &[u8]) {
            if self.0.len() < at + bytes.len() {
                self.0.resize(at + bytes.len(), 0);
            }
            self.0[at..at + bytes.len()].copy_from_slice(bytes);
        }
    }

    #[test]
    fn largest_preview_wins_and_ifd0_orientation_applies() {
        let small = jpeg(8, 8, [0, 0, 255]);
        let large = jpeg(32, 16, [255, 0, 0]);
        let mut t = Lt::new(b"II*\0", 600);
        // IFD0: orientation 6 (rotate 90 cw), SubIFD at 200, next IFD1 at 100.
        t.ifd(
            8,
            &[(TAG_ORIENTATION, 3, 1, 6), (TAG_SUB_IFDS, 4, 1, 200)],
            100,
        );
        // IFD1: thumbnail via JPEGInterchangeFormat.
        t.ifd(
            100,
            &[
                (TAG_JPEG_OFFSET, 4, 1, 600),
                (TAG_JPEG_LENGTH, 4, 1, small.len() as u32),
            ],
            0,
        );
        let large_at = 600 + small.len() as u32;
        // SubIFD: JPEG-compressed single strip.
        t.ifd(
            200,
            &[
                (TAG_COMPRESSION, 3, 1, 6),
                (TAG_STRIP_OFFSETS, 4, 1, large_at),
                (TAG_STRIP_BYTE_COUNTS, 4, 1, large.len() as u32),
            ],
            0,
        );
        t.blob(600, &small);
        t.blob(large_at as usize, &large);
        let img = extract(&t.0).unwrap();
        assert_eq!((img.width(), img.height()), (16, 32));
        assert!(close(img.to_rgb8().get_pixel(8, 16).0, [255, 0, 0]));
    }

    #[test]
    fn ifd_loops_terminate() {
        let j = jpeg(4, 4, [0, 255, 0]);
        let mut t = Lt::new(b"II*\0", 300);
        // IFD0 at 8 lists itself as next IFD, SubIFD and EXIF IFD.
        t.ifd(
            8,
            &[
                (TAG_SUB_IFDS, 4, 1, 8),
                (TAG_JPEG_OFFSET, 4, 1, 300),
                (TAG_JPEG_LENGTH, 4, 1, j.len() as u32),
                (TAG_EXIF_IFD, 4, 1, 8),
            ],
            8,
        );
        t.blob(300, &j);
        let img = extract(&t.0).unwrap();
        assert_eq!((img.width(), img.height()), (4, 4));
    }

    #[test]
    fn embedded_jpeg_with_huge_sof_is_too_large() {
        // 20000 x 20000 passes the pixel-area check; its RGB buffer (1.2 GB) does not
        // fit the decode byte budget and must be refused before allocation.
        let mut j = jpeg(8, 8, [0, 0, 255]);
        let sof = j.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
        j[sof + 5..sof + 7].copy_from_slice(&20000u16.to_be_bytes());
        j[sof + 7..sof + 9].copy_from_slice(&20000u16.to_be_bytes());
        let mut t = Lt::new(b"II*\0", 100);
        t.ifd(
            8,
            &[
                (TAG_JPEG_OFFSET, 4, 1, 100),
                (TAG_JPEG_LENGTH, 4, 1, j.len() as u32),
            ],
            0,
        );
        t.blob(100, &j);
        assert_eq!(extract(&t.0).err(), Some(DecodeError::TooLarge));
    }

    #[test]
    fn rw2_jpg_from_raw_and_lying_length() {
        let j = jpeg(24, 12, [10, 200, 10]);
        let mut t = Lt::new(b"IIU\0", 100);
        // JpgFromRaw blob, plus a JPEGInterchangeFormat pointing past the end of file and
        // one with a length far beyond EOF (clamped).
        t.ifd(
            8,
            &[
                (TAG_RW2_JPG_FROM_RAW, 7, j.len() as u32, 100),
                (TAG_JPEG_OFFSET, 4, 1, 0x7FFF_FFF0),
                (TAG_JPEG_LENGTH, 4, 1, 0xFFFF_FFFF),
            ],
            0,
        );
        t.blob(100, &j);
        let img = extract(&t.0).unwrap();
        assert_eq!((img.width(), img.height()), (24, 12));

        let mut t = Lt::new(b"II*\0", 100);
        t.ifd(
            8,
            &[
                (TAG_JPEG_OFFSET, 4, 1, 100),
                (TAG_JPEG_LENGTH, 4, 1, 0xFFFF_FFFF),
            ],
            0,
        );
        t.blob(100, &j);
        assert_eq!(extract(&t.0).unwrap().width(), 24);
    }

    #[test]
    fn olympus_makernote_preview() {
        let j = jpeg(20, 10, [0, 0, 200]);
        let mut t = Lt::new(b"IIRO", 400);
        t.ifd(8, &[(TAG_EXIF_IFD, 4, 1, 40)], 0);
        t.ifd(40, &[(TAG_MAKER_NOTE, 7, 200, 100)], 0);
        // Maker note at 100: "OLYMPUS\0II\x03\0" + IFD (offsets relative to 100).
        t.blob(100, b"OLYMPUS\0II\x03\0");
        let mut inner = Lt(vec![0u8; 300]);
        inner.ifd(112, &[(0x2020, 13, 1, 40)], 0);
        inner.ifd(
            140,
            &[(0x0101, 4, 1, 300), (0x0102, 4, 1, j.len() as u32)],
            0,
        );
        t.blob(112, &inner.0[112..140]);
        t.blob(140, &inner.0[140..170]);
        t.blob(400, &j);
        let img = extract(&t.0).unwrap();
        assert_eq!((img.width(), img.height()), (20, 10));
    }

    #[test]
    fn raf_header_offsets() {
        let j = jpeg(10, 6, [200, 200, 0]);
        let mut f = b"FUJIFILMCCD-RAW 0201FF383501".to_vec();
        f.resize(160, 0);
        f[84..88].copy_from_slice(&160u32.to_be_bytes());
        f[88..92].copy_from_slice(&(j.len() as u32).to_be_bytes());
        f.extend_from_slice(&j);
        let img = extract(&f).unwrap();
        assert_eq!((img.width(), img.height()), (10, 6));
    }

    fn bmff(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(typ);
        b.extend_from_slice(body);
        b
    }

    #[test]
    fn cr3_preview_box_and_cmt1_orientation() {
        let thumb = jpeg(16, 12, [0, 0, 0]);
        let prvw_jpeg = jpeg(40, 20, [255, 255, 255]);
        let mut prvw = vec![0u8; 16]; // preview header fields
        prvw.extend_from_slice(&prvw_jpeg);
        let mut thmb = vec![0u8; 16];
        thmb.extend_from_slice(&thumb);

        let mut cmt1 = Lt::new(b"II*\0", 40);
        cmt1.ifd(8, &[(TAG_ORIENTATION, 3, 1, 8)], 0);

        let canon_uuid = [bmff(b"CMT1", &cmt1.0), bmff(b"THMB", &thmb)].concat();
        let moov = bmff(
            b"moov",
            &bmff(b"uuid", &[vec![0u8; 16], canon_uuid].concat()),
        );
        let preview_uuid = bmff(b"uuid", &[vec![0u8; 24], bmff(b"PRVW", &prvw)].concat());
        let mut f = bmff(b"ftyp", b"crx \0\0\0\x01crx isom");
        f.extend_from_slice(&moov);
        f.extend_from_slice(&preview_uuid);
        // A JPEG inside mdat must not be considered.
        f.extend_from_slice(&bmff(b"mdat", &bmff(b"PRVW", &jpeg(64, 64, [1, 1, 1]))));
        let img = extract(&f).unwrap();
        // 40x20 preview rotated by orientation 8 (90 ccw).
        assert_eq!((img.width(), img.height()), (20, 40));
    }

    #[test]
    fn mrw_ttw_block() {
        let j = jpeg(12, 8, [50, 60, 70]);
        let mut tiff = Lt::new(b"MM\0*", 64);
        // Big-endian TIFF: rewrite header and IFD by hand.
        tiff.0[..8].copy_from_slice(b"MM\0*\0\0\0\x08");
        let ifd: Vec<u8> = [
            &1u16.to_be_bytes()[..],
            &TAG_EXIF_IFD.to_be_bytes(),
            &4u16.to_be_bytes(),
            &1u32.to_be_bytes(),
            &26u32.to_be_bytes(),
            &0u32.to_be_bytes(),
            // EXIF IFD at 26: maker note (plain IFD) at 44, 30 bytes.
            &1u16.to_be_bytes(),
            &TAG_MAKER_NOTE.to_be_bytes(),
            &7u16.to_be_bytes(),
            &30u32.to_be_bytes(),
            &44u32.to_be_bytes(),
            &0u32.to_be_bytes(),
            // Minolta maker note IFD at 44: 0x0088 start / 0x0089 length.
            &2u16.to_be_bytes(),
            &0x0088u16.to_be_bytes(),
            &4u16.to_be_bytes(),
            &1u32.to_be_bytes(),
            &80u32.to_be_bytes(),
            &0x0089u16.to_be_bytes(),
            &4u16.to_be_bytes(),
            &1u32.to_be_bytes(),
            &(j.len() as u32).to_be_bytes(),
            &0u32.to_be_bytes(),
        ]
        .concat();
        tiff.0.truncate(8);
        tiff.0.extend_from_slice(&ifd);
        tiff.0.resize(80, 0);
        tiff.0.extend_from_slice(&j);

        let mut ttw = b"\0TTW".to_vec();
        ttw.extend_from_slice(&(tiff.0.len() as u32).to_be_bytes());
        ttw.extend_from_slice(&tiff.0);
        let mut prd = b"\0PRD".to_vec();
        prd.extend_from_slice(&4u32.to_be_bytes());
        prd.extend_from_slice(&[0; 4]);
        let blocks = [prd, ttw].concat();
        let mut f = b"\0MRM".to_vec();
        f.extend_from_slice(&(blocks.len() as u32).to_be_bytes());
        f.extend_from_slice(&blocks);
        let img = extract(&f).unwrap();
        assert_eq!((img.width(), img.height()), (12, 8));
    }

    #[test]
    fn crw_ciff_heap() {
        let j = jpeg(14, 7, [90, 0, 90]);
        // Root heap at 26: JPEG record, then the record table, then the table offset.
        let mut f = b"II".to_vec();
        f.extend_from_slice(&26u32.to_le_bytes());
        f.extend_from_slice(b"HEAPCCDR");
        f.resize(26, 0);
        f.extend_from_slice(&j);
        let table_rel = j.len() as u32;
        f.extend_from_slice(&1u16.to_le_bytes());
        f.extend_from_slice(&CIFF_JPG_FROM_RAW.to_le_bytes());
        f.extend_from_slice(&(j.len() as u32).to_le_bytes());
        f.extend_from_slice(&0u32.to_le_bytes());
        f.extend_from_slice(&table_rel.to_le_bytes());
        let img = extract(&f).unwrap();
        assert_eq!((img.width(), img.height()), (14, 7));
    }

    #[test]
    fn x3f_image_section() {
        let j = jpeg(18, 9, [0, 128, 255]);
        let mut f = b"FOVb".to_vec();
        f.resize(64, 0);
        let sec_at = f.len() as u32;
        f.extend_from_slice(b"SECi");
        f.extend_from_slice(&[0u8; 24]);
        f.extend_from_slice(&j);
        let sec_len = f.len() as u32 - sec_at;
        let dir_at = f.len() as u32;
        f.extend_from_slice(b"SECd");
        f.extend_from_slice(&0x0002_0000u32.to_le_bytes());
        f.extend_from_slice(&1u32.to_le_bytes());
        f.extend_from_slice(&sec_at.to_le_bytes());
        f.extend_from_slice(&sec_len.to_le_bytes());
        f.extend_from_slice(b"IMA2");
        f.extend_from_slice(&dir_at.to_le_bytes());
        let img = extract(&f).unwrap();
        assert_eq!((img.width(), img.height()), (18, 9));
    }

    #[test]
    fn hostile_and_foreign_inputs() {
        assert_eq!(extract(b"not a raw file"), Err(DecodeError::Unsupported));
        // Bare TIFF header, IFD offset beyond EOF.
        assert_eq!(
            extract(b"II*\0\xFF\xFF\x00\x00"),
            Err(DecodeError::Unsupported)
        );
        // Truncated preview: candidate found but undecodable.
        let j = jpeg(32, 32, [1, 2, 3]);
        let mut t = Lt::new(b"II*\0", 100);
        t.ifd(
            8,
            &[
                (TAG_JPEG_OFFSET, 4, 1, 100),
                (TAG_JPEG_LENGTH, 4, 1, j.len() as u32),
            ],
            0,
        );
        // Keep the headers (SOF present) but cut before the scan data.
        let sos = j.windows(2).position(|w| w == [0xFF, 0xDA]).unwrap();
        t.blob(100, &j[..sos]);
        assert!(matches!(extract(&t.0), Err(DecodeError::Malformed(_))));
        // IFD declaring 65535 entries in a tiny file.
        let mut f = b"II*\0\x08\0\0\0".to_vec();
        f.extend_from_slice(&0xFFFFu16.to_le_bytes());
        assert_eq!(extract(&f), Err(DecodeError::Unsupported));
        // RAF with offsets beyond the end.
        let mut f = b"FUJIFILMCCD-RAW ".to_vec();
        f.resize(100, 0);
        f[84..88].copy_from_slice(&0xFFFF_FF00u32.to_be_bytes());
        f[88..92].copy_from_slice(&0xFFu32.to_be_bytes());
        assert_eq!(extract(&f), Err(DecodeError::Unsupported));
    }
}
