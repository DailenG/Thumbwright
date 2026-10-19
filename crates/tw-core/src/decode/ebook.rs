//! Ebooks and comics that are not ZIP packages: Mobipocket/Kindle (Palm database), FictionBook 2
//! (XML with base64 `<binary>` blobs) and TAR comic books.
//!
//! REQ-SEC-1: every container is walked with checked arithmetic over the in-memory file
//! (no DOM, no extraction to disk); record, entry and header counts are bounded, a payload
//! is rejected before it is copied or decoded when it exceeds
//! [`limits::ARCHIVE_ENTRY_MAX_BYTES`], and the bytes handed to image decoders across a
//! container are capped by [`limits::ARCHIVE_TOTAL_MAX_BYTES`].

use image::DynamicImage;

use super::zip_preview::{MAX_DECODE_ATTEMPTS, MAX_ENTRIES, normalize, rank_images, tags};
use super::{DecodeError, Result, check_alloc, decode_bytes};
use crate::limits;

pub const EXTENSIONS: &[&str] = &["mobi", "azw", "azw3", "prc", "fb2", "cbt"];

pub fn extract(data: &[u8], ext: &str) -> Result<DynamicImage> {
    match ext.to_ascii_lowercase().as_str() {
        "mobi" | "azw" | "azw3" | "prc" => mobi(data),
        "fb2" => fb2(data),
        "cbt" => cbt(data),
        _ => Err(DecodeError::Unsupported),
    }
}

fn be16(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(
        d.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn be32(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        d.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

/// Offset of the first `needle` at or after `from`.
fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    let (&first, rest) = needle.split_first()?;
    let mut i = from;
    while i < hay.len() {
        i += hay.get(i..)?.iter().position(|&b| b == first)?;
        if hay.get(i + 1..)?.starts_with(rest) {
            return Some(i);
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------------------
// Mobipocket / Kindle (Palm database)

const PDB_HEADER_LEN: usize = 78;
const PDB_RECORD_ENTRY_LEN: usize = 8;
/// EXTH records inspected per header.
const MAX_EXTH_RECORDS: u32 = 1024;
/// Records examined by the "first image record" fallback.
const MAX_IMAGE_SCAN: usize = 64;

const EXTH_KF8_BOUNDARY: u32 = 121;
const EXTH_COVER_OFFSET: u32 = 201;
const EXTH_THUMB_OFFSET: u32 = 202;

/// A Palm database: record `i` spans `offset[i]..offset[i + 1]` (the last one runs to the
/// end of the file).
struct Palm<'a> {
    data: &'a [u8],
    count: usize,
}

impl<'a> Palm<'a> {
    fn parse(data: &'a [u8]) -> Result<Self> {
        if data.len() < PDB_HEADER_LEN || data.get(60..68) != Some(b"BOOKMOBI") {
            return Err(DecodeError::Unsupported);
        }
        let count = usize::from(be16(data, 76).unwrap_or(0));
        PDB_RECORD_ENTRY_LEN
            .checked_mul(count)
            .and_then(|n| n.checked_add(PDB_HEADER_LEN))
            .filter(|&end| end <= data.len())
            .ok_or_else(|| DecodeError::malformed("Palm record list exceeds the file"))?;
        if count == 0 {
            return Err(DecodeError::malformed("Palm database without records"));
        }
        Ok(Self { data, count })
    }

    fn offset(&self, i: usize) -> Option<usize> {
        if i >= self.count {
            return None;
        }
        let at = PDB_HEADER_LEN + i * PDB_RECORD_ENTRY_LEN;
        usize::try_from(be32(self.data, at)?).ok()
    }

    fn record(&self, i: usize) -> Option<&'a [u8]> {
        let start = self.offset(i)?;
        let end = if i + 1 < self.count {
            self.offset(i + 1)?
        } else {
            self.data.len()
        };
        self.data.get(start..end)
    }
}

/// The parts of a MOBI/EXTH header that locate images.
#[derive(Default)]
struct MobiHeader {
    first_image: Option<usize>,
    cover_offset: Option<usize>,
    thumb_offset: Option<usize>,
    kf8_header: Option<usize>,
}

fn mobi_header(rec0: &[u8]) -> Option<MobiHeader> {
    // 16-byte PalmDOC header, then "MOBI" and the header length.
    if rec0.get(16..20) != Some(b"MOBI") {
        return None;
    }
    let header_len = usize::try_from(be32(rec0, 20)?).ok()?;
    let mut h = MobiHeader::default();
    if header_len >= 96 {
        h.first_image = be32(rec0, 108)
            .filter(|&v| v != u32::MAX)
            .and_then(|v| usize::try_from(v).ok());
    }
    let has_exth = header_len >= 116 && be32(rec0, 128).is_some_and(|f| f & 0x40 != 0);
    if has_exth {
        let exth = 16usize.checked_add(header_len)?;
        if rec0.get(exth..exth.checked_add(4)?) == Some(b"EXTH") {
            let count = be32(rec0, exth + 8)?.min(MAX_EXTH_RECORDS);
            let mut at = exth.checked_add(12)?;
            for _ in 0..count {
                let (Some(kind), Some(len)) = (be32(rec0, at), be32(rec0, at + 4)) else {
                    break;
                };
                let len = usize::try_from(len).ok().filter(|&l| l >= 8)?;
                let value = rec0
                    .get(at + 8..at.checked_add(len)?)
                    .filter(|v| v.len() == 4)
                    .and_then(|v| be32(v, 0))
                    .and_then(|v| usize::try_from(v).ok());
                match kind {
                    EXTH_COVER_OFFSET => h.cover_offset = value,
                    EXTH_THUMB_OFFSET => h.thumb_offset = value,
                    EXTH_KF8_BOUNDARY => h.kf8_header = value,
                    _ => {}
                }
                at = at.checked_add(len)?;
            }
        }
    }
    Some(h)
}

fn looks_like_image(rec: &[u8]) -> bool {
    image::guess_format(rec.get(..rec.len().min(16)).unwrap_or_default()).is_ok()
}

fn mobi(data: &[u8]) -> Result<DynamicImage> {
    let palm = Palm::parse(data)?;
    let rec0 = palm
        .record(0)
        .ok_or_else(|| DecodeError::malformed("MOBI record 0 out of range"))?;
    if rec0.get(16..20) != Some(b"MOBI") {
        // A plain PalmDOC (or an unknown header): there are no images to show.
        return Err(DecodeError::Unsupported);
    }
    let main = mobi_header(rec0).ok_or_else(|| DecodeError::malformed("MOBI header"))?;

    // Candidate image records, most specific first. In a joint MOBI6 + KF8 file the KF8
    // header (EXTH 121) carries its own image numbering: try it both as absolute record
    // indices and relative to the KF8 part.
    let mut candidates: Vec<usize> = Vec::new();
    let mut scan_from: Vec<usize> = Vec::new();
    let mut add = |h: &MobiHeader, base: usize| {
        let Some(first) = h.first_image.and_then(|f| f.checked_add(base)) else {
            return;
        };
        for off in [h.cover_offset, h.thumb_offset].into_iter().flatten() {
            candidates.extend(first.checked_add(off));
        }
        scan_from.push(first);
    };
    add(&main, 0);
    if let Some(kf8) = main.kf8_header.filter(|&k| k > 0 && k < palm.count)
        && let Some(h) = palm.record(kf8).and_then(mobi_header)
    {
        add(&h, 0);
        add(&h, kf8);
    }
    for first in scan_from {
        candidates.extend(first..first.saturating_add(MAX_IMAGE_SCAN));
    }

    let mut seen: Vec<usize> = Vec::new();
    let mut attempts = 0;
    let mut used = 0u64;
    let mut last = DecodeError::Unsupported;
    for idx in candidates {
        if seen.contains(&idx) {
            continue;
        }
        seen.push(idx);
        let Some(rec) = palm.record(idx).filter(|r| looks_like_image(r)) else {
            continue;
        };
        if attempts == MAX_DECODE_ATTEMPTS {
            break;
        }
        attempts += 1;
        let len = rec.len() as u64;
        used = used.saturating_add(len);
        if len > limits::ARCHIVE_ENTRY_MAX_BYTES || used > limits::ARCHIVE_TOTAL_MAX_BYTES {
            return Err(DecodeError::TooLarge);
        }
        match decode_bytes(rec) {
            Ok(img) => return Ok(img),
            Err(e) if e.falls_through() => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

// ---------------------------------------------------------------------------------------
// FictionBook 2

/// Largest part of the file examined for `<coverpage>` (it sits in `<description>`, ahead
/// of the first `<body>`).
const FB2_DESCRIPTION_MAX: usize = 8 * 1024 * 1024;
/// A `<binary>` start tag is never longer than this.
const FB2_TAG_MAX: usize = 4096;

/// Base64 text length that decodes to at most the per-entry cap (with slack for line
/// breaks and entities).
fn fb2_payload_cap() -> u64 {
    limits::ARCHIVE_ENTRY_MAX_BYTES / 3 * 4 * 9 / 8 + FB2_TAG_MAX as u64
}

fn fb2(data: &[u8]) -> Result<DynamicImage> {
    let head = data.get(..data.len().min(8192)).unwrap_or_default();
    if find(head, b"FictionBook", 0).is_none() {
        return Err(DecodeError::Unsupported);
    }
    let body_start = find(data, b"<body", 0);
    let description_end = body_start.unwrap_or(data.len()).min(FB2_DESCRIPTION_MAX);
    let description = String::from_utf8_lossy(&data[..description_end]);
    let mut cover_id = None;
    let mut in_coverpage = false;
    for t in tags(&description) {
        if t.is("coverpage") {
            in_coverpage = !t.end;
        } else if in_coverpage && t.is("image") && !t.end {
            cover_id = t
                .attr("href")
                .map(|h| h.trim_start_matches('#').to_string());
            break;
        }
    }

    let mut pos = body_start.unwrap_or(0);
    let mut first_image: Option<(usize, usize)> = None;
    let mut cover: Option<(usize, usize)> = None;
    // First `>` at or after the last `<binary` probed. Every probe either reuses it or
    // scans forward to a new one, so the scans never overlap: a run of `<binary ` with no
    // `>` costs one pass over the file, not 4 KiB per occurrence.
    let mut next_gt: Option<usize> = None;
    while let Some(at) = find(data, b"<binary", pos) {
        pos = at + 7;
        let Some(&next) = data.get(pos) else { break };
        if !(next.is_ascii_whitespace() || next == b'>' || next == b'/') {
            continue;
        }
        let gt = match next_gt {
            Some(g) if g >= at => g,
            _ => match memchr::memchr(b'>', &data[at..]) {
                Some(i) => {
                    next_gt = Some(at + i);
                    at + i
                }
                // No `>` anywhere after this: no later start tag can end either.
                None => break,
            },
        };
        if gt - at >= FB2_TAG_MAX {
            continue;
        }
        if data[gt - 1] == b'/' {
            continue;
        }
        let tag_text = String::from_utf8_lossy(&data[at..=gt]);
        let Some(tag) = tags(&tag_text).next() else {
            continue;
        };
        let payload_start = gt + 1;
        let payload_end = find(data, b"<", payload_start).unwrap_or(data.len());
        pos = payload_end;
        let id = tag.attr("id").unwrap_or_default();
        if cover_id.as_deref().is_some_and(|c| c == id) {
            cover = Some((payload_start, payload_end));
            break;
        }
        let is_image = tag
            .attr("content-type")
            .is_some_and(|c| c.to_ascii_lowercase().starts_with("image/"));
        if is_image && first_image.is_none() {
            first_image = Some((payload_start, payload_end));
        }
    }
    let (start, end) = cover.or(first_image).ok_or(DecodeError::Unsupported)?;
    let text = &data[start..end];
    if text.len() as u64 > fb2_payload_cap() {
        return Err(DecodeError::TooLarge);
    }
    decode_bytes(&base64_decode(text, limits::ARCHIVE_ENTRY_MAX_BYTES)?)
}

/// Base64 (standard or URL alphabet) tolerant of whitespace and numeric entities
/// (`&#10;`); stops at the first `=`.
fn base64_decode(text: &[u8], max_out: u64) -> Result<Vec<u8>> {
    let cap = (text.len() / 4 * 3 + 3).min(usize::try_from(max_out).unwrap_or(usize::MAX));
    check_alloc(cap as u64)?;
    let mut out = Vec::with_capacity(cap);
    let (mut acc, mut bits) = (0u32, 0u32);
    let mut i = 0;
    while let Some(&c) = text.get(i) {
        i += 1;
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            b'&' => {
                let window = text.get(i..text.len().min(i + 10)).unwrap_or_default();
                let semi = window
                    .iter()
                    .position(|&b| b == b';')
                    .ok_or_else(|| DecodeError::malformed("bad entity in base64 payload"))?;
                i += semi + 1;
                continue;
            }
            c if c.is_ascii_whitespace() => continue,
            _ => {
                return Err(DecodeError::malformed(
                    "invalid base64 in FictionBook binary",
                ));
            }
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            if out.len() >= cap {
                return Err(DecodeError::TooLarge);
            }
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// TAR comic books

const TAR_BLOCK: usize = 512;
/// Headers visited (regular, long-name and pax records alike).
const TAR_MAX_HEADERS: usize = MAX_ENTRIES * 3;
/// Largest GNU long-name / pax extended header honoured.
const TAR_META_MAX: usize = 64 * 1024;

struct TarEntry {
    name: String,
    start: usize,
    len: usize,
}

/// Octal number field, or GNU base-256 when the high bit of the first byte is set.
fn tar_number(field: &[u8]) -> Option<u64> {
    if field.first()? & 0x80 != 0 {
        let mut v = u64::from(field[0] & 0x7f);
        for &b in &field[1..] {
            v = v.checked_mul(256)?.checked_add(u64::from(b))?;
        }
        return Some(v);
    }
    let mut v = 0u64;
    let mut seen = false;
    for &b in field {
        match b {
            b'0'..=b'7' => {
                v = v.checked_mul(8)?.checked_add(u64::from(b - b'0'))?;
                seen = true;
            }
            b' ' | 0 if !seen => {}
            b' ' | 0 => break,
            _ => return None,
        }
    }
    seen.then_some(v)
}

fn tar_checksum_ok(block: &[u8]) -> bool {
    let Some(stored) = block.get(148..156).and_then(tar_number) else {
        return false;
    };
    let (mut unsigned, mut signed) = (0u64, 0i64);
    for (i, &b) in block.iter().enumerate() {
        let b = if (148..156).contains(&i) { b' ' } else { b };
        unsigned += u64::from(b);
        signed += i64::from(b as i8);
    }
    stored == unsigned || i64::try_from(stored).is_ok_and(|s| s == signed)
}

fn c_string(field: &[u8]) -> &[u8] {
    &field[..field.iter().position(|&b| b == 0).unwrap_or(field.len())]
}

/// `path` / `size` from a pax extended header ("<len> <key>=<value>\n" records).
fn pax_records(data: &[u8]) -> (Option<String>, Option<u64>) {
    let (mut path, mut size) = (None, None);
    let mut at = 0;
    while at < data.len() {
        let rest = &data[at..];
        let Some(sp) = rest.iter().position(|&b| b == b' ') else {
            break;
        };
        let Some(len) = std::str::from_utf8(&rest[..sp])
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&l| l > sp + 1 && l <= rest.len())
        else {
            break;
        };
        let record = &rest[sp + 1..len];
        let record = record.strip_suffix(b"\n").unwrap_or(record);
        if let Some(eq) = record.iter().position(|&b| b == b'=') {
            let value = &record[eq + 1..];
            match &record[..eq] {
                b"path" => path = Some(String::from_utf8_lossy(value).into_owned()),
                b"size" => size = std::str::from_utf8(value).ok().and_then(|s| s.parse().ok()),
                _ => {}
            }
        }
        at += len;
    }
    (path, size)
}

/// Regular files of a ustar / GNU / pax archive held in memory. A truncated or damaged
/// tail ends the listing; a first block that is not a tar header is `Unsupported`.
fn tar_entries(data: &[u8]) -> Result<Vec<TarEntry>> {
    let mut entries = Vec::new();
    let mut at = 0usize;
    let mut long_name: Option<String> = None;
    let mut pax_path: Option<String> = None;
    let mut pax_size: Option<u64> = None;
    for header_no in 0..TAR_MAX_HEADERS {
        let Some(block) = data.get(at..at.saturating_add(TAR_BLOCK)) else {
            break;
        };
        if block.iter().all(|&b| b == 0) {
            break;
        }
        if !tar_checksum_ok(block) {
            if header_no == 0 {
                return Err(DecodeError::Unsupported);
            }
            break;
        }
        let Some(declared) = tar_number(&block[124..136]) else {
            break;
        };
        let size = pax_size.take().unwrap_or(declared);
        let start = at + TAR_BLOCK;
        let Some(len) = usize::try_from(size)
            .ok()
            .filter(|&l| l <= data.len().saturating_sub(start))
        else {
            break;
        };
        let padded = len.div_ceil(TAR_BLOCK).saturating_mul(TAR_BLOCK);
        let kind = block[156];
        let payload = &data[start..start + len];
        match kind {
            b'L' => {
                long_name = (len <= TAR_META_MAX)
                    .then(|| String::from_utf8_lossy(c_string(payload)).into_owned());
            }
            b'x' => {
                if len <= TAR_META_MAX {
                    let (path, size) = pax_records(payload);
                    pax_path = path;
                    pax_size = size;
                }
            }
            b'0' | 0 | b'7' => {
                let name = pax_path
                    .take()
                    .or_else(|| long_name.take())
                    .unwrap_or_else(|| {
                        let name = String::from_utf8_lossy(c_string(&block[..100])).into_owned();
                        let is_ustar = block.get(257..262) == Some(b"ustar");
                        let prefix = c_string(&block[345..500]);
                        if is_ustar && !prefix.is_empty() {
                            format!("{}/{name}", String::from_utf8_lossy(prefix))
                        } else {
                            name
                        }
                    });
                let name = normalize(&name);
                if !name.is_empty() {
                    entries.push(TarEntry { name, start, len });
                }
                long_name = None;
            }
            _ => {
                long_name = None;
                pax_path = None;
            }
        }
        at = start.saturating_add(padded);
        if entries.len() >= MAX_ENTRIES {
            break;
        }
    }
    Ok(entries)
}

fn cbt(data: &[u8]) -> Result<DynamicImage> {
    let entries = tar_entries(data)?;
    let order = rank_images(entries.iter().map(|e| e.name.as_str()), true);
    let mut used = 0u64;
    let mut last = DecodeError::Unsupported;
    for &i in order.iter().take(MAX_DECODE_ATTEMPTS) {
        let e = &entries[i];
        let len = e.len as u64;
        used = used.saturating_add(len);
        if len > limits::ARCHIVE_ENTRY_MAX_BYTES || used > limits::ARCHIVE_TOTAL_MAX_BYTES {
            return Err(DecodeError::TooLarge);
        }
        match decode_bytes(&data[e.start..e.start + e.len]) {
            Ok(img) => return Ok(img),
            Err(err) if err.falls_through() => last = err,
            Err(err) => return Err(err),
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GenericImageView, ImageFormat, Rgba, RgbaImage};
    use std::io::Cursor;

    fn png(w: u32, h: u32, color: [u8; 4]) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        RgbaImage::from_pixel(w, h, Rgba(color))
            .write_to(&mut out, ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    fn gif(w: u32, h: u32, color: [u8; 4]) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        RgbaImage::from_pixel(w, h, Rgba(color))
            .write_to(&mut out, ImageFormat::Gif)
            .unwrap();
        out.into_inner()
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xEDB8_8320 & 0u32.wrapping_sub(crc & 1));
            }
        }
        !crc
    }

    /// A PNG that is only a signature and an IHDR declaring `w` x `h`.
    fn png_header_only(w: u32, h: u32) -> Vec<u8> {
        let mut ihdr = Vec::new();
        ihdr.extend(w.to_be_bytes());
        ihdr.extend(h.to_be_bytes());
        ihdr.extend([8, 6, 0, 0, 0]);
        let mut chunk = b"IHDR".to_vec();
        chunk.extend(&ihdr);
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        out.extend(13u32.to_be_bytes());
        out.extend(&chunk);
        out.extend(crc32(&chunk).to_be_bytes());
        out
    }

    fn px(img: &DynamicImage, x: u32, y: u32) -> [u8; 4] {
        img.get_pixel(x, y).0
    }

    fn is_clean_failure(r: Result<DynamicImage>) -> bool {
        matches!(
            r,
            Err(DecodeError::TooLarge | DecodeError::Malformed(_) | DecodeError::Unsupported)
        )
    }

    // ---- Mobipocket ------------------------------------------------------------------

    struct MobiSpec {
        first_image: u32,
        exth: Vec<(u32, Vec<u8>)>,
    }

    fn record0(spec: &MobiSpec) -> Vec<u8> {
        let header_len = 232usize;
        let mut r = vec![0u8; 16 + header_len];
        r[16..20].copy_from_slice(b"MOBI");
        r[20..24].copy_from_slice(&(header_len as u32).to_be_bytes());
        r[108..112].copy_from_slice(&spec.first_image.to_be_bytes());
        if !spec.exth.is_empty() {
            r[128..132].copy_from_slice(&0x40u32.to_be_bytes());
            let mut exth = b"EXTH".to_vec();
            let mut body = Vec::new();
            for (kind, value) in &spec.exth {
                body.extend(kind.to_be_bytes());
                body.extend(((value.len() + 8) as u32).to_be_bytes());
                body.extend(value);
            }
            exth.extend(((body.len() + 12) as u32).to_be_bytes());
            exth.extend((spec.exth.len() as u32).to_be_bytes());
            exth.extend(body);
            r.extend(exth);
        }
        r
    }

    fn palm(records: &[Vec<u8>]) -> Vec<u8> {
        let mut out = vec![0u8; 78];
        out[..4].copy_from_slice(b"Test");
        out[60..68].copy_from_slice(b"BOOKMOBI");
        out[76..78].copy_from_slice(&(records.len() as u16).to_be_bytes());
        let mut offset = 78 + records.len() * 8 + 2;
        for r in records {
            out.extend((offset as u32).to_be_bytes());
            out.extend([0, 0, 0, 0]);
            offset += r.len();
        }
        out.extend([0, 0]);
        for r in records {
            out.extend(r);
        }
        out
    }

    fn exth_u32(kind: u32, v: u32) -> (u32, Vec<u8>) {
        (kind, v.to_be_bytes().to_vec())
    }

    #[test]
    fn mobi_cover_offset_from_exth() {
        let book = palm(&[
            record0(&MobiSpec {
                first_image: 2,
                exth: vec![exth_u32(201, 1)],
            }),
            b"text record".to_vec(),
            png(2, 2, [255, 0, 0, 255]),
            png(5, 3, [0, 0, 255, 255]),
            png(7, 7, [0, 255, 0, 255]),
        ]);
        for ext in ["mobi", "azw", "azw3", "prc", "MOBI"] {
            let img = extract(&book, ext).unwrap();
            assert_eq!(img.dimensions(), (5, 3), "{ext}");
            assert_eq!(px(&img, 2, 1), [0, 0, 255, 255]);
        }
    }

    #[test]
    fn mobi_thumb_offset_then_first_image_record() {
        let thumb_only = palm(&[
            record0(&MobiSpec {
                first_image: 2,
                exth: vec![exth_u32(202, 1)],
            }),
            b"text".to_vec(),
            gif(2, 2, [255, 0, 0, 255]),
            gif(4, 4, [0, 255, 0, 255]),
        ]);
        let img = extract(&thumb_only, "mobi").unwrap();
        assert_eq!(img.dimensions(), (4, 4));
        // No EXTH at all: the first image record.
        let plain = palm(&[
            record0(&MobiSpec {
                first_image: 1,
                exth: vec![],
            }),
            png(6, 2, [9, 8, 7, 255]),
            png(3, 3, [1, 1, 1, 255]),
        ]);
        let img = extract(&plain, "azw").unwrap();
        assert_eq!(img.dimensions(), (6, 2));
        assert_eq!(px(&img, 5, 1), [9, 8, 7, 255]);
    }

    #[test]
    fn mobi_cover_offset_out_of_range_falls_back_to_first_image() {
        let book = palm(&[
            record0(&MobiSpec {
                first_image: 1,
                exth: vec![exth_u32(201, 900)],
            }),
            png(2, 5, [3, 4, 5, 255]),
        ]);
        assert_eq!(extract(&book, "mobi").unwrap().dimensions(), (2, 5));
    }

    #[test]
    fn kf8_joint_file_uses_the_kf8_header() {
        // Records: 0 MOBI6 header (no images), 1 text, 2 BOUNDARY, 3 KF8 header, 4 text,
        // 5 image (red), 6 image (cover, blue). The KF8 header numbers images from its
        // own first record.
        let book = palm(&[
            record0(&MobiSpec {
                first_image: u32::MAX,
                exth: vec![exth_u32(121, 3)],
            }),
            b"text".to_vec(),
            b"BOUNDARY".to_vec(),
            record0(&MobiSpec {
                first_image: 2,
                exth: vec![exth_u32(201, 1)],
            }),
            b"text".to_vec(),
            png(2, 2, [255, 0, 0, 255]),
            png(8, 4, [0, 0, 255, 255]),
        ]);
        let img = extract(&book, "azw3").unwrap();
        assert_eq!(img.dimensions(), (8, 4));
        assert_eq!(px(&img, 0, 0), [0, 0, 255, 255]);
    }

    #[test]
    fn mobi_foreign_and_hostile_inputs_fail_cleanly() {
        assert!(matches!(
            extract(b"", "mobi"),
            Err(DecodeError::Unsupported)
        ));
        assert!(matches!(
            extract(&[0u8; 100], "mobi"),
            Err(DecodeError::Unsupported)
        ));
        let book = palm(&[
            record0(&MobiSpec {
                first_image: 1,
                exth: vec![exth_u32(201, 0)],
            }),
            png(2, 2, [1, 2, 3, 255]),
        ]);
        // Truncated at every length: never a panic.
        for cut in 0..book.len() {
            let _ = extract(&book[..cut], "mobi");
        }
        // Record count far beyond the file.
        let mut lying = book.clone();
        lying[76..78].copy_from_slice(&u16::MAX.to_be_bytes());
        assert!(matches!(
            extract(&lying, "mobi"),
            Err(DecodeError::Malformed(_))
        ));
        // Record offsets pointing outside the file / backwards.
        let mut wild = book.clone();
        wild[78..82].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(is_clean_failure(extract(&wild, "mobi")));
        let mut backwards = book.clone();
        backwards[86..90].copy_from_slice(&80u32.to_be_bytes());
        assert!(is_clean_failure(extract(&backwards, "mobi")));
        // Header fields at their extremes.
        let mut spec = record0(&MobiSpec {
            first_image: u32::MAX - 1,
            exth: vec![exth_u32(201, u32::MAX)],
        });
        spec[20..24].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(is_clean_failure(extract(&palm(&[spec]), "mobi")));
        // An EXTH claiming 2^32 - 1 records and an over-long first record.
        let mut exth = record0(&MobiSpec {
            first_image: 1,
            exth: vec![exth_u32(201, 0)],
        });
        exth[248 + 8..248 + 12].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(extract(&palm(&[exth, png(2, 2, [0, 0, 0, 255])]), "mobi").is_ok());
        // A cover declaring 65535 x 65535 pixels.
        let bomb = palm(&[
            record0(&MobiSpec {
                first_image: 1,
                exth: vec![exth_u32(201, 0)],
            }),
            png_header_only(65535, 65535),
        ]);
        assert_eq!(extract(&bomb, "mobi"), Err(DecodeError::TooLarge));
        // Plain PalmDOC: no images to show.
        let mut doc = palm(&[vec![0u8; 40]]);
        doc[60..68].copy_from_slice(b"TEXtREAd");
        assert!(matches!(
            extract(&doc, "prc"),
            Err(DecodeError::Unsupported)
        ));
    }

    // ---- FictionBook 2 ----------------------------------------------------------------

    fn base64(data: &[u8]) -> String {
        const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut s = String::new();
        for c in data.chunks(3) {
            let n = (u32::from(c[0]) << 16)
                | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
                | u32::from(*c.get(2).unwrap_or(&0));
            s.push(A[(n >> 18) as usize & 63] as char);
            s.push(A[(n >> 12) as usize & 63] as char);
            s.push(if c.len() > 1 {
                A[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            s.push(if c.len() > 2 {
                A[n as usize & 63] as char
            } else {
                '='
            });
        }
        s
    }

    fn fb2_doc(coverpage: &str, binaries: &[(&str, &str, String)]) -> Vec<u8> {
        let mut s = String::from(
            "<?xml version=\"1.0\" encoding=\"windows-1251\"?>\n\
             <FictionBook xmlns=\"http://www.gribuser.ru/xml/fictionbook/2.0\" \
             xmlns:l=\"http://www.w3.org/1999/xlink\"><description><title-info>",
        );
        s.push_str(coverpage);
        s.push_str("</title-info></description><body><section><p>text</p></section></body>");
        for (id, ctype, payload) in binaries {
            s.push_str(&format!(
                "<binary id=\"{id}\" content-type=\"{ctype}\">{payload}</binary>"
            ));
        }
        s.push_str("</FictionBook>");
        s.into_bytes()
    }

    #[test]
    fn fb2_coverpage_binary_with_wrapped_base64() {
        let cover = base64(&png(9, 4, [10, 20, 30, 255]));
        // Line breaks every 20 characters and numeric entities between lines.
        let wrapped: String = cover
            .as_bytes()
            .chunks(20)
            .enumerate()
            .map(|(i, c)| {
                let sep = if i % 2 == 0 { "\n" } else { "&#10;" };
                format!("{}{sep}", std::str::from_utf8(c).unwrap())
            })
            .collect();
        let doc = fb2_doc(
            "<coverpage><image l:href=\"#cover.png\"/></coverpage>",
            &[
                (
                    "other.png",
                    "image/png",
                    base64(&png(2, 2, [255, 0, 0, 255])),
                ),
                ("cover.png", "image/png", wrapped),
            ],
        );
        let img = extract(&doc, "fb2").unwrap();
        assert_eq!(img.dimensions(), (9, 4));
        assert_eq!(px(&img, 4, 2), [10, 20, 30, 255]);
    }

    #[test]
    fn fb2_without_coverpage_uses_first_image_and_other_prefix() {
        let doc = fb2_doc(
            "",
            &[
                ("notes", "text/plain", "aGVsbG8=".into()),
                ("a.png", "image/png", base64(&png(3, 6, [1, 2, 3, 255]))),
                ("b.png", "image/png", base64(&png(1, 1, [9, 9, 9, 255]))),
            ],
        );
        assert_eq!(extract(&doc, "fb2").unwrap().dimensions(), (3, 6));
        // `xlink:href` prefix and a non-UTF-8 byte in the description.
        let mut doc = fb2_doc(
            "<coverpage><image xlink:href=\"#c\"/></coverpage>",
            &[("c", "image/png", base64(&png(4, 4, [5, 5, 5, 255])))],
        );
        let at = find(&doc, b"<title-info>", 0).unwrap() + 12;
        doc.insert(at, 0xE9);
        assert_eq!(extract(&doc, "fb2").unwrap().dimensions(), (4, 4));
    }

    #[test]
    fn fb2_hostile_inputs_fail_cleanly() {
        assert!(matches!(extract(b"", "fb2"), Err(DecodeError::Unsupported)));
        assert!(matches!(
            extract(b"<html><body>not a book</body></html>", "fb2"),
            Err(DecodeError::Unsupported)
        ));
        let no_binary = fb2_doc("<coverpage><image l:href=\"#x\"/></coverpage>", &[]);
        assert!(matches!(
            extract(&no_binary, "fb2"),
            Err(DecodeError::Unsupported)
        ));
        let bad_b64 = fb2_doc(
            "<coverpage><image l:href=\"#x\"/></coverpage>",
            &[("x", "image/png", "!!!! not base64 ####".into())],
        );
        assert!(matches!(
            extract(&bad_b64, "fb2"),
            Err(DecodeError::Malformed(_))
        ));
        let bad_entity = fb2_doc(
            "<coverpage><image l:href=\"#x\"/></coverpage>",
            &[("x", "image/png", "QUJD&nothingtoseehere".into())],
        );
        assert!(matches!(
            extract(&bad_entity, "fb2"),
            Err(DecodeError::Malformed(_))
        ));
        // Truncated at every length (mid-tag, mid-payload): never a panic.
        let doc = fb2_doc(
            "<coverpage><image l:href=\"#x\"/></coverpage>",
            &[("x", "image/png", base64(&png(2, 2, [1, 1, 1, 255])))],
        );
        for cut in 0..doc.len() {
            let _ = extract(&doc[..cut], "fb2");
        }
        // A cover declaring 65535 x 65535 and one declaring u32::MAX squared.
        for (w, h) in [(65535, 65535), (u32::MAX, u32::MAX)] {
            let bomb = fb2_doc(
                "<coverpage><image l:href=\"#x\"/></coverpage>",
                &[("x", "image/png", base64(&png_header_only(w, h)))],
            );
            assert!(is_clean_failure(extract(&bomb, "fb2")), "{w}x{h}");
        }
        let bomb = fb2_doc(
            "<coverpage><image l:href=\"#x\"/></coverpage>",
            &[("x", "image/png", base64(&png_header_only(65535, 65535)))],
        );
        assert_eq!(extract(&bomb, "fb2"), Err(DecodeError::TooLarge));
        // Many tiny binaries stay linear.
        let many: Vec<(String, String)> = (0..2000)
            .map(|i| (format!("b{i}"), "aGk=".into()))
            .collect();
        let refs: Vec<(&str, &str, String)> = many
            .iter()
            .map(|(id, p)| (id.as_str(), "application/octet-stream", p.clone()))
            .collect();
        assert!(matches!(
            extract(&fb2_doc("", &refs), "fb2"),
            Err(DecodeError::Unsupported)
        ));
    }

    #[test]
    fn base64_decoder_is_tolerant_but_bounded() {
        let b64 = |t: &[u8]| base64_decode(t, 1 << 20);
        assert_eq!(b64(b"aGVs bG8=").unwrap(), b"hello");
        assert_eq!(b64(b"aGVs&#10;bG8").unwrap(), b"hello");
        assert_eq!(b64(b"-_8=").unwrap(), b"\xfb\xff");
        assert_eq!(b64(b"+/8=").unwrap(), b"\xfb\xff");
        assert!(b64(b"").unwrap().is_empty());
        assert!(b64(b"a&b").is_err());
        // The output cap is enforced while decoding.
        assert_eq!(base64_decode(b"aGVsbG8=", 3), Err(DecodeError::TooLarge));
        assert_eq!(base64_decode(b"aGVsbG8=", 5).unwrap(), b"hello");
    }

    // ---- TAR comics -------------------------------------------------------------------

    fn tar_header(name: &str, size: u64, kind: u8, ustar_prefix: &str) -> [u8; 512] {
        let mut h = [0u8; 512];
        h[..name.len().min(100)].copy_from_slice(&name.as_bytes()[..name.len().min(100)]);
        h[100..107].copy_from_slice(b"0000644");
        h[108..115].copy_from_slice(b"0000000");
        h[116..123].copy_from_slice(b"0000000");
        h[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
        h[136..147].copy_from_slice(b"00000000000");
        h[156] = kind;
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        h[345..345 + ustar_prefix.len()].copy_from_slice(ustar_prefix.as_bytes());
        seal(&mut h);
        h
    }

    fn seal(h: &mut [u8; 512]) {
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    }

    fn tar_entry(out: &mut Vec<u8>, header: [u8; 512], data: &[u8]) {
        out.extend(header);
        out.extend(data);
        out.extend(vec![0u8; (512 - data.len() % 512) % 512]);
    }

    fn tar_file(out: &mut Vec<u8>, name: &str, data: &[u8]) {
        tar_entry(out, tar_header(name, data.len() as u64, b'0', ""), data);
    }

    fn finish_tar(mut out: Vec<u8>) -> Vec<u8> {
        out.extend(vec![0u8; 1024]);
        out
    }

    #[test]
    fn cbt_first_page_in_natural_order() {
        let mut t = Vec::new();
        tar_file(&mut t, "comic/page10.png", &png(10, 10, [10, 0, 0, 255]));
        tar_file(&mut t, "comic/page2.png", &png(2, 2, [2, 0, 0, 255]));
        tar_file(&mut t, "comic/readme.txt", b"hello");
        tar_file(&mut t, "comic/page1.png", &png(7, 3, [1, 2, 3, 255]));
        let img = extract(&finish_tar(t), "cbt").unwrap();
        assert_eq!(img.dimensions(), (7, 3));
        assert_eq!(px(&img, 6, 2), [1, 2, 3, 255]);
    }

    #[test]
    fn cbt_prefers_cover_and_skips_directories_and_metadata() {
        let mut t = Vec::new();
        tar_entry(&mut t, tar_header("comic/", 0, b'5', ""), b"");
        tar_file(
            &mut t,
            "__MACOSX/comic/._001.png",
            &png(1, 1, [0, 0, 0, 255]),
        );
        tar_file(&mut t, "comic/001.png", &png(3, 3, [1, 1, 1, 255]));
        tar_file(
            &mut t,
            "comic/zz_Cover.PNG",
            &png(5, 5, [200, 100, 50, 255]),
        );
        let img = extract(&finish_tar(t), "cbt").unwrap();
        assert_eq!(img.dimensions(), (5, 5));
        assert_eq!(px(&img, 0, 0), [200, 100, 50, 255]);
    }

    #[test]
    fn cbt_long_names_gnu_pax_and_ustar_prefix() {
        let long = format!("{}/001.png", "d".repeat(150));
        // GNU 'L' record supplies the real name of the next entry.
        let mut t = Vec::new();
        tar_entry(
            &mut t,
            tar_header("././@LongLink", long.len() as u64 + 1, b'L', ""),
            format!("{long}\0").as_bytes(),
        );
        tar_file(&mut t, "truncated-name", &png(4, 2, [1, 1, 1, 255]));
        tar_file(&mut t, "plain.txt", b"x");
        let entries = tar_entries(&finish_tar(t)).unwrap();
        assert_eq!(entries[0].name, long);
        assert_eq!(entries[1].name, "plain.txt");

        // pax: path and size override.
        let record = |k: &str, v: &str| {
            let body = format!(" {k}={v}\n");
            let mut len = body.len() + 1;
            len = (body.len() + len.to_string().len()).max(len);
            format!("{len}{body}")
        };
        let pax = format!(
            "{}{}",
            record("path", "pax/dir/002.png"),
            record("mtime", "1.5")
        );
        let img_bytes = png(6, 6, [9, 9, 9, 255]);
        let mut t = Vec::new();
        tar_entry(
            &mut t,
            tar_header("PaxHeader", pax.len() as u64, b'x', ""),
            pax.as_bytes(),
        );
        tar_file(&mut t, "short", &img_bytes);
        let entries = tar_entries(&finish_tar(t)).unwrap();
        assert_eq!(entries[0].name, "pax/dir/002.png");
        assert_eq!(entries[0].len, img_bytes.len());

        // ustar prefix + name.
        let mut t = Vec::new();
        tar_entry(
            &mut t,
            tar_header("001.png", img_bytes.len() as u64, b'0', "some/prefix"),
            &img_bytes,
        );
        let t = finish_tar(t);
        assert_eq!(tar_entries(&t).unwrap()[0].name, "some/prefix/001.png");
        assert_eq!(extract(&t, "cbt").unwrap().dimensions(), (6, 6));
    }

    // Written by Windows' bsdtar (`tar -cf`, and `--format=pax`) from 200x150 PNG pages:
    // red page1, green page2, blue page3, yellow page4 (and a text file).
    const BSDTAR_COMIC: &[u8] = include_bytes!("../../tests/fixtures/archives/comic.cbt");
    const BSDTAR_PAX_COMIC: &[u8] = include_bytes!("../../tests/fixtures/archives/comic_pax.cbt");

    #[test]
    fn comics_written_by_bsdtar() {
        for (name, t) in [("ustar", BSDTAR_COMIC), ("pax", BSDTAR_PAX_COMIC)] {
            let img = extract(t, "cbt").unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(img.dimensions(), (200, 150), "{name}");
            assert_eq!(px(&img, 100, 75), [220, 40, 40, 255], "{name}");
        }
        let names: Vec<String> = tar_entries(BSDTAR_COMIC)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(
            names,
            [
                "page1.png",
                "page2.png",
                "page3.png",
                "page4.png",
                "notes.txt"
            ]
        );
    }

    #[test]
    fn cbt_base256_size_field() {
        let data = png(3, 5, [7, 7, 7, 255]);
        let mut h = tar_header("a.png", 0, b'0', "");
        h[124] = 0x80;
        h[125..132].fill(0);
        h[132..136].copy_from_slice(&(data.len() as u32).to_be_bytes());
        seal(&mut h);
        let mut t = Vec::new();
        tar_entry(&mut t, h, &data);
        assert_eq!(extract(&finish_tar(t), "cbt").unwrap().dimensions(), (3, 5));
    }

    #[test]
    fn cbt_foreign_damaged_and_hostile_inputs_fail_cleanly() {
        assert!(matches!(extract(b"", "cbt"), Err(DecodeError::Unsupported)));
        assert!(matches!(
            extract(&[b'x'; 2048], "cbt"),
            Err(DecodeError::Unsupported)
        ));
        let mut t = Vec::new();
        tar_file(&mut t, "a.png", &png(2, 2, [1, 2, 3, 255]));
        tar_file(&mut t, "b.png", &png(2, 2, [1, 2, 3, 255]));
        let t = finish_tar(t);
        // Truncated at every length: never a panic.
        for cut in 0..t.len() {
            let _ = extract(&t[..cut], "cbt");
        }
        // A bad checksum after the first entry ends the listing, first entry still shown.
        let mut bad_sum = t.clone();
        let second = 512 + 512; // header + one data block of the first entry
        bad_sum[second + 148] = b'9';
        assert_eq!(extract(&bad_sum, "cbt").unwrap().dimensions(), (2, 2));
        // Declared sizes beyond the file or beyond usize: the entry is dropped.
        for size in [u64::from(u32::MAX), 0o77777777777u64] {
            let mut lie = Vec::new();
            tar_entry(&mut lie, tar_header("a.png", size, b'0', ""), b"short");
            assert!(is_clean_failure(extract(&finish_tar(lie), "cbt")));
        }
        let mut huge = tar_header("a.png", 0, b'0', "");
        huge[124] = 0xFF;
        huge[125..136].fill(0xFF);
        seal(&mut huge);
        let mut lie = Vec::new();
        tar_entry(&mut lie, huge, b"short");
        assert!(is_clean_failure(extract(&finish_tar(lie), "cbt")));
        // Header flood: the walk is bounded and fast.
        let mut flood = Vec::new();
        for i in 0..(MAX_ENTRIES + 100) {
            tar_file(&mut flood, &format!("f{i}.txt"), b"");
        }
        assert!(tar_entries(&flood).unwrap().len() <= MAX_ENTRIES);
        // Oversized GNU long-name / pax records are ignored rather than buffered.
        let mut t = Vec::new();
        tar_entry(
            &mut t,
            tar_header("L", (TAR_META_MAX + 1) as u64, b'L', ""),
            &vec![b'n'; TAR_META_MAX + 1],
        );
        tar_file(&mut t, "real.png", &png(2, 3, [1, 1, 1, 255]));
        assert_eq!(extract(&finish_tar(t), "cbt").unwrap().dimensions(), (2, 3));
        // A page declaring 65535 x 65535 pixels.
        let mut t = Vec::new();
        tar_file(&mut t, "001.png", &png_header_only(65535, 65535));
        assert_eq!(extract(&finish_tar(t), "cbt"), Err(DecodeError::TooLarge));
    }

    /// DA-6: each `<binary ` without a `>` in the next 4096 bytes cost a 4 KiB scan.
    #[test]
    fn fb2_unterminated_binary_tags_cost_one_pass() {
        let mut doc = b"<?xml version=\"1.0\"?><FictionBook><description/>".to_vec();
        doc.extend(b"<binary ".repeat(512 * 1024));
        let start = std::time::Instant::now();
        assert!(matches!(
            extract(&doc, "fb2"),
            Err(DecodeError::Unsupported)
        ));
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "fb2 took {:?}",
            start.elapsed()
        );
        // A terminated tag after the run is still found: the cached `>` is reused, not
        // trusted past the 4096-byte tag limit.
        let mut doc = b"<?xml version=\"1.0\"?><FictionBook><description/>".to_vec();
        doc.extend(b"<binary ".repeat(1024));
        doc.extend(vec![b'x'; 5000]);
        doc.extend(
            format!(
                "<binary id=\"x\" content-type=\"image/png\">{}</binary>",
                base64(&png(3, 2, [9, 9, 9, 255]))
            )
            .as_bytes(),
        );
        assert_eq!(extract(&doc, "fb2").unwrap().dimensions(), (3, 2));
    }
}
