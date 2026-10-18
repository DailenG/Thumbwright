//! Minimal EXIF support for `info`, conversion carry-over and lossless rotation: a bounds
//! checked TIFF/IFD reader, extraction of the EXIF block from JPEG, PNG and WebP, the
//! in-place normalisation applied when pixels have already been oriented, and merging of
//! an EXIF block into a TIFF file written by the `image` crate.
//!
//! Every offset read from the file is checked against the buffer, no IFD pointer is
//! followed more than one level deep (IFD0, then the EXIF and GPS IFDs and the interop
//! IFD below the EXIF IFD), so hostile metadata cannot loop or over-allocate.

const TAG_IMAGE_DESCRIPTION: u16 = 0x010E;
const TAG_MAKE: u16 = 0x010F;
const TAG_MODEL: u16 = 0x0110;
const TAG_ORIENTATION: u16 = 0x0112;
const TAG_X_RESOLUTION: u16 = 0x011A;
const TAG_Y_RESOLUTION: u16 = 0x011B;
const TAG_RESOLUTION_UNIT: u16 = 0x0128;
const TAG_SOFTWARE: u16 = 0x0131;
const TAG_DATE_TIME: u16 = 0x0132;
const TAG_ARTIST: u16 = 0x013B;
const TAG_SUB_IFDS: u16 = 0x014A;
const TAG_COPYRIGHT: u16 = 0x8298;
const TAG_EXIF_IFD: u16 = 0x8769;
const TAG_GPS_IFD: u16 = 0x8825;
const TAG_DATE_TIME_ORIGINAL: u16 = 0x9003;
const TAG_MAKER_NOTE: u16 = 0x927C;
const TAG_PIXEL_X: u16 = 0xA002;
const TAG_PIXEL_Y: u16 = 0xA003;
const TAG_INTEROP_IFD: u16 = 0xA005;

const TYPE_ASCII: u16 = 2;
const TYPE_SHORT: u16 = 3;
const TYPE_LONG: u16 = 4;
const TYPE_RATIONAL: u16 = 5;

/// Size in bytes of one element of each TIFF field type (1..=12).
fn element_size(typ: u16) -> Option<usize> {
    Some(match typ {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 => 4,
        5 | 10 | 12 => 8,
        _ => return None,
    })
}

/// Byte-swap unit: rationals are two independent 32-bit words.
fn swap_unit(typ: u16) -> Option<usize> {
    match typ {
        5 | 10 => Some(4),
        t => element_size(t),
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Entry {
    pub tag: u16,
    pub typ: u16,
    pub count: u32,
    /// Offset of the 12-byte directory entry; its value field is at `at + 8`.
    at: usize,
}

#[derive(Clone, Copy)]
pub struct Tiff<'a> {
    d: &'a [u8],
    le: bool,
}

impl<'a> Tiff<'a> {
    pub fn parse(d: &'a [u8]) -> Option<Self> {
        let le = match d.get(..2)? {
            b"II" => true,
            b"MM" => false,
            _ => return None,
        };
        let t = Self { d, le };
        (t.u16(2)? == 42).then_some(t)
    }

    fn u16(&self, o: usize) -> Option<u16> {
        let b: [u8; 2] = self.d.get(o..o.checked_add(2)?)?.try_into().ok()?;
        Some(if self.le {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    }

    fn u32(&self, o: usize) -> Option<u32> {
        let b: [u8; 4] = self.d.get(o..o.checked_add(4)?)?.try_into().ok()?;
        Some(if self.le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }

    pub fn ifd0(&self) -> Option<usize> {
        Some(self.u32(4)? as usize)
    }

    /// Entries of the IFD at `ifd`.
    pub fn entries(&self, ifd: usize) -> Option<Vec<Entry>> {
        let n = self.u16(ifd)? as usize;
        let end = ifd.checked_add(2 + n * 12)?;
        if end > self.d.len() {
            return None;
        }
        (0..n)
            .map(|i| {
                let at = ifd + 2 + i * 12;
                Some(Entry {
                    tag: self.u16(at)?,
                    typ: self.u16(at + 2)?,
                    count: self.u32(at + 4)?,
                    at,
                })
            })
            .collect()
    }

    fn find(&self, ifd: usize, tag: u16) -> Option<Entry> {
        self.entries(ifd)?.into_iter().find(|e| e.tag == tag)
    }

    /// Size of the value of `e` in bytes.
    fn value_len(e: &Entry) -> Option<usize> {
        usize::try_from(u64::from(e.count) * element_size(e.typ)? as u64).ok()
    }

    /// The raw (file byte order) value bytes of `e`, inline or at its offset.
    fn data(&self, e: &Entry) -> Option<&'a [u8]> {
        let len = Self::value_len(e)?;
        if len <= 4 {
            self.d.get(e.at + 8..e.at + 8 + len)
        } else {
            let off = self.u32(e.at + 8)? as usize;
            self.d.get(off..off.checked_add(len)?)
        }
    }

    fn ascii(&self, e: &Entry) -> Option<String> {
        if e.typ != TYPE_ASCII {
            return None;
        }
        let raw = self.data(e)?;
        let raw = raw.split(|&b| b == 0).next()?;
        let s = String::from_utf8_lossy(raw).trim().to_string();
        (!s.is_empty()).then_some(s)
    }

    fn short(&self, e: &Entry) -> Option<u16> {
        (e.typ == TYPE_SHORT && e.count >= 1).then(|| self.u16(self.data_off(e)?))?
    }

    /// File offset of the first value byte of `e`.
    fn data_off(&self, e: &Entry) -> Option<usize> {
        if Self::value_len(e)? <= 4 {
            Some(e.at + 8)
        } else {
            Some(self.u32(e.at + 8)? as usize)
        }
    }

    fn rational(&self, e: &Entry, i: usize) -> Option<f64> {
        if e.typ != TYPE_RATIONAL || i >= e.count as usize {
            return None;
        }
        let o = self.data_off(e)?.checked_add(i * 8)?;
        let (n, d) = (self.u32(o)?, self.u32(o + 4)?);
        (d != 0).then(|| f64::from(n) / f64::from(d))
    }

    /// Offset held by a LONG pointer entry (EXIF IFD, GPS IFD, ...).
    fn pointer(&self, ifd: usize, tag: u16) -> Option<usize> {
        let e = self.find(ifd, tag)?;
        (e.typ == TYPE_LONG && e.count == 1).then(|| self.u32(e.at + 8).map(|v| v as usize))?
    }
}

/// Camera and capture fields reported by `twctl info`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    pub make: Option<String>,
    pub model: Option<String>,
    /// EXIF `YYYY:MM:DD HH:MM:SS` text, exactly as stored.
    pub datetime: Option<String>,
    pub dpi: Option<(f64, f64)>,
    /// Signed decimal degrees (latitude, longitude).
    pub gps: Option<(f64, f64)>,
}

fn gps_coordinate(t: &Tiff, gps: usize, tag: u16, ref_tag: u16, positive: u8) -> Option<f64> {
    let e = t.find(gps, tag)?;
    if e.count != 3 {
        return None;
    }
    let v = t.rational(&e, 0)? + t.rational(&e, 1)? / 60.0 + t.rational(&e, 2)? / 3600.0;
    let r = t.find(gps, ref_tag)?;
    let sign = match *t.data(&r)?.first()? {
        c if c.eq_ignore_ascii_case(&positive) => 1.0,
        b'S' | b's' | b'W' | b'w' => -1.0,
        _ => return None,
    };
    Some(sign * v)
}

/// Reads the summary fields from a TIFF-structured EXIF block (or a TIFF file's IFD0).
pub fn summarize(tiff: &[u8]) -> Summary {
    let mut s = Summary::default();
    let Some(t) = Tiff::parse(tiff) else {
        return s;
    };
    let Some(ifd0) = t.ifd0() else { return s };
    let Some(entries) = t.entries(ifd0) else {
        return s;
    };
    let text = |tag| {
        entries
            .iter()
            .find(|e| e.tag == tag)
            .and_then(|e| t.ascii(e))
    };
    s.make = text(TAG_MAKE);
    s.model = text(TAG_MODEL);
    s.datetime = text(TAG_DATE_TIME);
    if let Some(exif) = t.pointer(ifd0, TAG_EXIF_IFD)
        && let Some(e) = t.find(exif, TAG_DATE_TIME_ORIGINAL)
        && let Some(v) = t.ascii(&e)
    {
        s.datetime = Some(v);
    }
    let res = |tag| {
        entries
            .iter()
            .find(|e| e.tag == tag)
            .and_then(|e| t.rational(e, 0))
    };
    if let (Some(x), Some(y)) = (res(TAG_X_RESOLUTION), res(TAG_Y_RESOLUTION))
        && x > 0.0
        && y > 0.0
    {
        let per_inch = match entries
            .iter()
            .find(|e| e.tag == TAG_RESOLUTION_UNIT)
            .and_then(|e| t.short(e))
        {
            Some(3) => 2.54,
            None | Some(2) => 1.0,
            Some(_) => 0.0,
        };
        if per_inch > 0.0 {
            s.dpi = Some((x * per_inch, y * per_inch));
        }
    }
    if let Some(gps) = t.pointer(ifd0, TAG_GPS_IFD) {
        let lat = gps_coordinate(&t, gps, 2, 1, b'N');
        let lon = gps_coordinate(&t, gps, 4, 3, b'E');
        if let (Some(lat), Some(lon)) = (lat, lon)
            && lat.abs() <= 90.0
            && lon.abs() <= 180.0
        {
            s.gps = Some((lat, lon));
        }
    }
    s
}

/// EXIF orientation (1-8) stored in an EXIF block; `None` when absent or invalid.
pub fn orientation(tiff: &[u8]) -> Option<u16> {
    let t = Tiff::parse(tiff)?;
    let e = t.find(t.ifd0()?, TAG_ORIENTATION)?;
    t.short(&e).filter(|o| (1..=8).contains(o))
}

fn be16(d: &[u8], o: usize) -> Option<usize> {
    Some(u16::from_be_bytes(d.get(o..o + 2)?.try_into().ok()?) as usize)
}

/// Visits the JPEG segments before the scan data: `(marker, payload)`.
pub fn jpeg_segments(d: &[u8]) -> Vec<(u8, &[u8])> {
    let mut out = Vec::new();
    if !d.starts_with(&[0xFF, 0xD8]) {
        return out;
    }
    let mut i = 2;
    while let (Some(&0xFF), Some(&m)) = (d.get(i), d.get(i + 1)) {
        if m == 0xFF {
            i += 1;
            continue;
        }
        if m == 0xDA || m == 0xD9 {
            break;
        }
        if m == 0x01 || (0xD0..=0xD7).contains(&m) {
            i += 2;
            continue;
        }
        let Some(len) = be16(d, i + 2).filter(|&l| l >= 2) else {
            break;
        };
        let Some(payload) = d.get(i + 4..i + 2 + len) else {
            break;
        };
        out.push((m, payload));
        i += 2 + len;
    }
    out
}

pub fn png_chunks(d: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    let mut i = 8;
    std::iter::from_fn(move || {
        let len = u32::from_be_bytes(d.get(i..i + 4)?.try_into().ok()?) as usize;
        let typ: [u8; 4] = d.get(i + 4..i + 8)?.try_into().ok()?;
        let body = d.get(i + 8..i.checked_add(8 + len)?)?;
        i = i.checked_add(12 + len)?;
        Some((typ, body))
    })
}

pub fn webp_chunks(d: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    let mut i = 12;
    std::iter::from_fn(move || {
        let typ: [u8; 4] = d.get(i..i + 4)?.try_into().ok()?;
        let len = u32::from_le_bytes(d.get(i + 4..i + 8)?.try_into().ok()?) as usize;
        let body = d.get(i + 8..i.checked_add(8 + len)?)?;
        i = i.checked_add(8 + len + (len & 1))?;
        Some((typ, body))
    })
}

pub fn is_png(d: &[u8]) -> bool {
    d.starts_with(b"\x89PNG\r\n\x1a\n")
}

pub fn is_webp(d: &[u8]) -> bool {
    d.len() >= 12 && &d[..4] == b"RIFF" && &d[8..12] == b"WEBP"
}

/// ICC profile (TIFF tag 34675) and XMP packet (tag 700) are stored as byte arrays in IFD0.
pub const TAG_ICC_PROFILE: u16 = 0x8773;
pub const TAG_XMP: u16 = 0x02BC;

/// The raw bytes of a BYTE, ASCII or UNDEFINED tag of a TIFF file's IFD0 (an ICC profile
/// or an XMP packet); `None` when absent, of another type, or out of bounds.
pub fn tiff_bytes(file: &[u8], tag: u16) -> Option<&[u8]> {
    let t = Tiff::parse(file)?;
    let e = t.find(t.ifd0()?, tag)?;
    matches!(e.typ, 1 | 2 | 7).then(|| t.data(&e))?
}

/// Whether `file` is a classic TIFF (the structure `from_tiff` reads).
pub fn is_tiff(file: &[u8]) -> bool {
    Tiff::parse(file).is_some()
}

/// The TIFF-structured EXIF block of a JPEG, PNG or WebP file (no `Exif\0\0` prefix).
pub fn from_container(d: &[u8]) -> Option<&[u8]> {
    if d.starts_with(&[0xFF, 0xD8]) {
        jpeg_segments(d)
            .into_iter()
            .find(|(m, p)| *m == 0xE1 && p.starts_with(b"Exif\0\0"))
            .map(|(_, p)| &p[6..])
    } else if is_png(d) {
        png_chunks(d).find(|(t, _)| t == b"eXIf").map(|(_, b)| b)
    } else if is_webp(d) {
        webp_chunks(d)
            .find(|(t, _)| t == b"EXIF")
            .map(|(_, b)| b.strip_prefix(b"Exif\0\0").unwrap_or(b))
    } else {
        None
    }
}

/// Resolution stored by the container itself: JFIF density or PNG `pHYs`, in dpi.
fn container_dpi(d: &[u8]) -> Option<(f64, f64)> {
    let dpi = |x: f64, y: f64, scale: f64| (x > 0.0 && y > 0.0).then_some((x * scale, y * scale));
    if d.starts_with(&[0xFF, 0xD8]) {
        let (_, p) = jpeg_segments(d)
            .into_iter()
            .find(|(m, p)| *m == 0xE0 && p.starts_with(b"JFIF\0") && p.len() >= 12)?;
        let (x, y) = (be16(p, 8)? as f64, be16(p, 10)? as f64);
        match p[7] {
            1 => dpi(x, y, 1.0),
            2 => dpi(x, y, 2.54),
            _ => None,
        }
    } else if is_png(d) {
        let (_, b) = png_chunks(d).find(|(t, _)| t == b"pHYs")?;
        if b.len() != 9 || b[8] != 1 {
            return None;
        }
        let x = f64::from(u32::from_be_bytes(b[..4].try_into().ok()?));
        let y = f64::from(u32::from_be_bytes(b[4..8].try_into().ok()?));
        dpi(x, y, 0.0254)
    } else {
        None
    }
}

/// Summary of a whole file: its EXIF block, or the file itself when it is a TIFF, with
/// the container's own resolution as the fallback for dpi.
pub fn summarize_file(d: &[u8]) -> Summary {
    let mut s = match from_container(d) {
        Some(block) => summarize(block),
        None => summarize(d),
    };
    if s.dpi.is_none() {
        s.dpi = container_dpi(d);
    }
    s
}

fn put_u16(d: &mut [u8], o: usize, v: u16, le: bool) {
    let b = if le { v.to_le_bytes() } else { v.to_be_bytes() };
    d[o..o + 2].copy_from_slice(&b);
}

fn put_u32(d: &mut [u8], o: usize, v: u32, le: bool) {
    let b = if le { v.to_le_bytes() } else { v.to_be_bytes() };
    d[o..o + 4].copy_from_slice(&b);
}

/// End offset of everything reachable from IFD0, or `None` when a SubIFD makes the
/// reachable set unknowable.
fn reachable_end(t: &Tiff, ifd0: usize, ifd0_entries: &[Entry]) -> Option<usize> {
    let mut end = 8;
    let mut visit = |ifd: usize, entries: &[Entry]| -> Option<()> {
        end = end.max(ifd + 2 + entries.len() * 12 + 4);
        for e in entries {
            let len = Tiff::value_len(e)?;
            if len > 4 {
                end = end.max((t.u32(e.at + 8)? as usize).checked_add(len)?);
            }
        }
        Some(())
    };
    if ifd0_entries.iter().any(|e| e.tag == TAG_SUB_IFDS) {
        return None;
    }
    visit(ifd0, ifd0_entries)?;
    for tag in [TAG_EXIF_IFD, TAG_GPS_IFD] {
        if let Some(off) = t.pointer(ifd0, tag) {
            let entries = t.entries(off)?;
            visit(off, &entries)?;
            if tag == TAG_EXIF_IFD
                && let Some(io) = t.pointer(off, TAG_INTEROP_IFD)
            {
                visit(io, &t.entries(io)?)?;
            }
        }
    }
    Some(end)
}

/// Prepares a source EXIF block for pixels that were already oriented and possibly
/// resized to `dims`: Orientation becomes 1, the EXIF pixel dimensions follow `dims`, and
/// the embedded thumbnail (IFD1), which would show the old orientation, is dropped along
/// with any bytes nothing references any more. Layout is otherwise untouched, so maker
/// notes keep valid internal offsets. `None` when the block cannot be handled safely.
pub fn normalise(tiff: &[u8], dims: (u32, u32)) -> Option<Vec<u8>> {
    let t = Tiff::parse(tiff)?;
    let le = t.le;
    let ifd0 = t.ifd0()?;
    let entries = t.entries(ifd0)?;
    let mut out = tiff.to_vec();
    if let Some(e) = entries.iter().find(|e| e.tag == TAG_ORIENTATION) {
        if e.typ != TYPE_SHORT || e.count != 1 {
            return None;
        }
        put_u16(&mut out, e.at + 8, 1, le);
    }
    let next = ifd0 + 2 + entries.len() * 12;
    if next + 4 <= out.len() {
        put_u32(&mut out, next, 0, le);
    }
    if let Some(exif) = t.pointer(ifd0, TAG_EXIF_IFD) {
        for e in t.entries(exif)? {
            let v = match e.tag {
                TAG_PIXEL_X => dims.0,
                TAG_PIXEL_Y => dims.1,
                _ => continue,
            };
            match (e.typ, e.count) {
                (TYPE_SHORT, 1) => put_u16(&mut out, e.at + 8, u16::try_from(v).ok()?, le),
                (TYPE_LONG, 1) => put_u32(&mut out, e.at + 8, v, le),
                _ => return None,
            }
        }
    }
    if let Some(end) = reachable_end(&t, ifd0, &entries) {
        out.truncate(end.min(out.len()));
    }
    Some(out)
}

/// One directory entry with its value as little-endian bytes.
struct OutEntry {
    tag: u16,
    typ: u16,
    count: u32,
    data: Vec<u8>,
}

/// Most value bytes `read_entries` copies out of one IFD. Entries may share the bytes they
/// point at, so hostile counts could otherwise multiply a small file.
const MAX_IFD_BYTES: usize = 4 << 20;

/// Entries of `ifd` as little-endian `OutEntry`s, minus those `skip` rejects. `None` when
/// a kept entry is out of bounds or the copied values would exceed [`MAX_IFD_BYTES`].
fn read_entries(t: &Tiff, ifd: usize, skip: impl Fn(u16) -> bool) -> Option<Vec<OutEntry>> {
    let mut out = Vec::new();
    let mut total = 0usize;
    for e in t.entries(ifd)? {
        if skip(e.tag) {
            continue;
        }
        let mut data = t.data(&e)?.to_vec();
        total = total
            .checked_add(data.len())
            .filter(|&n| n <= MAX_IFD_BYTES)?;
        let unit = swap_unit(e.typ)?;
        if !t.le && unit > 1 {
            data.chunks_exact_mut(unit).for_each(<[u8]>::reverse);
        }
        out.push(OutEntry {
            tag: e.tag,
            typ: e.typ,
            count: e.count,
            data,
        });
    }
    Some(out)
}

/// Appends a little-endian IFD (sorted by tag, values after the table) to `out`;
/// returns its offset.
fn write_ifd(out: &mut Vec<u8>, entries: &mut [OutEntry], next: u32) -> Option<u32> {
    entries.sort_by_key(|e| e.tag);
    if out.len() % 2 == 1 {
        out.push(0);
    }
    let at = out.len();
    let values_at = at + 2 + entries.len() * 12 + 4;
    let mut table = Vec::with_capacity(values_at - at);
    let mut values: Vec<u8> = Vec::new();
    table.extend_from_slice(&u16::try_from(entries.len()).ok()?.to_le_bytes());
    for e in entries.iter() {
        table.extend_from_slice(&e.tag.to_le_bytes());
        table.extend_from_slice(&e.typ.to_le_bytes());
        table.extend_from_slice(&e.count.to_le_bytes());
        if e.data.len() <= 4 {
            table.extend_from_slice(&e.data);
            table.resize(table.len() + 4 - e.data.len(), 0);
        } else {
            let off = u32::try_from(values_at + values.len()).ok()?;
            table.extend_from_slice(&off.to_le_bytes());
            values.extend_from_slice(&e.data);
            if values.len() % 2 == 1 {
                values.push(0);
            }
        }
    }
    table.extend_from_slice(&next.to_le_bytes());
    out.extend_from_slice(&table);
    out.extend_from_slice(&values);
    u32::try_from(at).ok()
}

fn long_entry(tag: u16, v: u32) -> OutEntry {
    OutEntry {
        tag,
        typ: TYPE_LONG,
        count: 1,
        data: v.to_le_bytes().to_vec(),
    }
}

/// IFD0 tags worth carrying into a TIFF (descriptive text; resolution is handled apart).
const TIFF_CARRY: [u16; 7] = [
    TAG_IMAGE_DESCRIPTION,
    TAG_MAKE,
    TAG_MODEL,
    TAG_SOFTWARE,
    TAG_DATE_TIME,
    TAG_ARTIST,
    TAG_COPYRIGHT,
];

/// A compact EXIF block for a TIFF *file* (the file is not an EXIF block itself: its IFD0
/// also describes strips, tiles and compression, and its offsets point into the pixel
/// data). IFD0 keeps the descriptive tags, resolution and Orientation, the EXIF IFD keeps
/// everything but the maker note (its offsets are relative to the whole file) and the
/// interop pointer, and the GPS IFD is copied whole. `None` when the file carries none of
/// these or cannot be parsed.
pub fn from_tiff(file: &[u8]) -> Option<Vec<u8>> {
    let t = Tiff::parse(file)?;
    let ifd0 = t.ifd0()?;
    let mut entries = read_entries(&t, ifd0, |tag| {
        !(TIFF_CARRY.contains(&tag)
            || [
                TAG_X_RESOLUTION,
                TAG_Y_RESOLUTION,
                TAG_RESOLUTION_UNIT,
                TAG_ORIENTATION,
            ]
            .contains(&tag))
    })?;
    // The writers of plain TIFFs record a placeholder 1/1 resolution with "no absolute unit".
    if entries
        .iter()
        .any(|e| e.tag == TAG_RESOLUTION_UNIT && e.data.first() == Some(&1))
    {
        entries.retain(|e| {
            ![TAG_X_RESOLUTION, TAG_Y_RESOLUTION, TAG_RESOLUTION_UNIT].contains(&e.tag)
        });
    }
    let mut out = b"II*\0\x08\0\0\0".to_vec();
    for (pointer, skip_maker) in [(TAG_EXIF_IFD, true), (TAG_GPS_IFD, false)] {
        let Some(off) = t.pointer(ifd0, pointer) else {
            continue;
        };
        let mut sub = read_entries(&t, off, |tag| {
            skip_maker && (tag == TAG_MAKER_NOTE || tag == TAG_INTEROP_IFD)
        })?;
        if !sub.is_empty() {
            let at = write_ifd(&mut out, &mut sub, 0)?;
            entries.push(long_entry(pointer, at));
        }
    }
    if entries.is_empty() {
        return None;
    }
    let at = write_ifd(&mut out, &mut entries, 0)?;
    out[4..8].copy_from_slice(&at.to_le_bytes());
    Some(out)
}

/// Adds metadata to a little-endian TIFF file written by the `image` crate.
///
/// `exif` (a source EXIF block) contributes descriptive IFD0 tags, resolution when the file
/// has none, the EXIF IFD (without maker note or interop pointer) and the GPS IFD.
/// Orientation is not copied (the pixels are already upright). `icc` becomes tag 34675 and
/// `xmp` tag 700. A new IFD0 is appended and the header repointed; the old directory stays
/// as unused bytes. Returns the input unchanged when the file cannot be parsed; an `exif`
/// block that cannot be parsed is skipped, `icc` and `xmp` are still added.
pub fn embed_in_tiff(
    tiff: Vec<u8>,
    exif: Option<&[u8]>,
    icc: Option<&[u8]>,
    xmp: Option<&[u8]>,
    dims: (u32, u32),
) -> Vec<u8> {
    embed(&tiff, exif, icc, xmp, dims).unwrap_or(tiff)
}

fn byte_entry(tag: u16, typ: u16, data: &[u8]) -> Option<OutEntry> {
    Some(OutEntry {
        tag,
        typ,
        count: u32::try_from(data.len()).ok()?,
        data: data.to_vec(),
    })
}

fn embed(
    tiff: &[u8],
    exif: Option<&[u8]>,
    icc: Option<&[u8]>,
    xmp: Option<&[u8]>,
    dims: (u32, u32),
) -> Option<Vec<u8>> {
    let dst = Tiff::parse(tiff).filter(|t| t.le)?;
    let src = exif
        .and_then(Tiff::parse)
        .and_then(|t| Some((t, t.ifd0()?)));
    let mut entries = read_entries(&dst, dst.ifd0()?, |_| false)?;
    // The TIFF writer records a placeholder 1/1 resolution with "no absolute unit";
    // the source's real resolution replaces it.
    let res_tags = [TAG_X_RESOLUTION, TAG_Y_RESOLUTION, TAG_RESOLUTION_UNIT];
    if src.is_some()
        && entries
            .iter()
            .any(|e| e.tag == TAG_RESOLUTION_UNIT && e.data.first() == Some(&1))
    {
        entries.retain(|e| !res_tags.contains(&e.tag));
    }
    let has = |entries: &[OutEntry], tag| entries.iter().any(|e| e.tag == tag);
    let has_res = res_tags.iter().any(|&t| has(&entries, t));
    let mut out = tiff.to_vec();
    if let Some((src, src0)) = src {
        let carried = read_entries(&src, src0, |tag| {
            !(TIFF_CARRY.contains(&tag) || (!has_res && res_tags.contains(&tag)))
        })?;
        let existing: Vec<u16> = entries.iter().map(|e| e.tag).collect();
        entries.extend(carried.into_iter().filter(|c| !existing.contains(&c.tag)));
        if !has(&entries, TAG_EXIF_IFD)
            && let Some(off) = src.pointer(src0, TAG_EXIF_IFD)
        {
            let mut sub = read_entries(&src, off, |t| t == TAG_MAKER_NOTE || t == TAG_INTEROP_IFD)?;
            for e in &mut sub {
                let v = match e.tag {
                    TAG_PIXEL_X => dims.0,
                    TAG_PIXEL_Y => dims.1,
                    _ => continue,
                };
                *e = long_entry(e.tag, v);
            }
            if !sub.is_empty() {
                let at = write_ifd(&mut out, &mut sub, 0)?;
                entries.push(long_entry(TAG_EXIF_IFD, at));
            }
        }
        if !has(&entries, TAG_GPS_IFD)
            && let Some(off) = src.pointer(src0, TAG_GPS_IFD)
        {
            let mut sub = read_entries(&src, off, |_| false)?;
            if !sub.is_empty() {
                let at = write_ifd(&mut out, &mut sub, 0)?;
                entries.push(long_entry(TAG_GPS_IFD, at));
            }
        }
    }
    if let Some(icc) = icc.filter(|_| !has(&entries, TAG_ICC_PROFILE)) {
        entries.push(byte_entry(TAG_ICC_PROFILE, 7, icc)?);
    }
    if let Some(xmp) = xmp.filter(|_| !has(&entries, TAG_XMP)) {
        entries.push(byte_entry(TAG_XMP, 1, xmp)?);
    }
    let at = write_ifd(&mut out, &mut entries, 0)?;
    out[4..8].copy_from_slice(&at.to_le_bytes());
    Some(out)
}

#[cfg(test)]
pub mod testutil {
    use super::*;

    fn ascii(tag: u16, s: &str) -> OutEntry {
        let mut data = s.as_bytes().to_vec();
        data.push(0);
        OutEntry {
            tag,
            typ: TYPE_ASCII,
            count: data.len() as u32,
            data,
        }
    }

    fn short(tag: u16, v: u16) -> OutEntry {
        OutEntry {
            tag,
            typ: TYPE_SHORT,
            count: 1,
            data: v.to_le_bytes().to_vec(),
        }
    }

    fn rationals(tag: u16, v: &[(u32, u32)]) -> OutEntry {
        OutEntry {
            tag,
            typ: TYPE_RATIONAL,
            count: v.len() as u32,
            data: v
                .iter()
                .flat_map(|(n, d)| n.to_le_bytes().into_iter().chain(d.to_le_bytes()))
                .collect(),
        }
    }

    /// A camera-like little-endian EXIF block: IFD0 (Make, Model, DateTime, Orientation,
    /// resolution 300 dpi), EXIF IFD (DateTimeOriginal, pixel dimensions, a maker note),
    /// GPS IFD (47 36.372 N, 122 19.926 W) and, last, an IFD1 thumbnail of 4096 filler
    /// bytes, the layout cameras write.
    pub fn camera_exif(orientation: u16, dims: (u32, u32)) -> Vec<u8> {
        // The layout does not depend on the pointer values, so the first pass finds them.
        let (_, ptrs) = build_camera_exif(orientation, dims, [0; 3]);
        build_camera_exif(orientation, dims, ptrs).0
    }

    fn build_camera_exif(orientation: u16, dims: (u32, u32), p: [u32; 3]) -> (Vec<u8>, [u32; 3]) {
        let mut out = b"II*\0\x08\0\0\0".to_vec();
        let mut ifd0 = vec![
            ascii(TAG_MAKE, "Thumbcam"),
            ascii(TAG_MODEL, "TC-100 Mark II"),
            short(TAG_ORIENTATION, orientation),
            rationals(TAG_X_RESOLUTION, &[(300, 1)]),
            rationals(TAG_Y_RESOLUTION, &[(300, 1)]),
            short(TAG_RESOLUTION_UNIT, 2),
            ascii(TAG_DATE_TIME, "2023:07:04 12:34:57"),
            long_entry(TAG_EXIF_IFD, p[0]),
            long_entry(TAG_GPS_IFD, p[1]),
        ];
        write_ifd(&mut out, &mut ifd0, p[2]).unwrap();
        let mut exif = vec![
            ascii(TAG_DATE_TIME_ORIGINAL, "2023:07:04 12:34:56"),
            long_entry(TAG_PIXEL_X, dims.0),
            long_entry(TAG_PIXEL_Y, dims.1),
            OutEntry {
                tag: TAG_MAKER_NOTE,
                typ: 7,
                count: 8,
                data: b"MAKERNOT".to_vec(),
            },
        ];
        let exif_at = write_ifd(&mut out, &mut exif, 0).unwrap();
        let mut gps = vec![
            ascii(1, "N"),
            rationals(2, &[(47, 1), (36372, 1000), (0, 1)]),
            ascii(3, "W"),
            rationals(4, &[(122, 1), (19926, 1000), (0, 1)]),
        ];
        let gps_at = write_ifd(&mut out, &mut gps, 0).unwrap();
        if out.len() % 2 == 1 {
            out.push(0);
        }
        // Two inline entries: a 30-byte table, then the thumbnail.
        let thumb_at = out.len() as u32 + 30;
        let mut ifd1 = vec![long_entry(0x0201, thumb_at), long_entry(0x0202, 4096)];
        let ifd1_at = write_ifd(&mut out, &mut ifd1, 0).unwrap();
        assert_eq!(out.len() as u32, thumb_at);
        out.extend(std::iter::repeat_n(0xAB, 4096));
        (out, [exif_at, gps_at, ifd1_at])
    }

    /// Inserts `exif` as an APP1 segment right after SOI (and any JFIF APP0).
    pub fn inject_into_jpeg(jpeg: &[u8], exif: &[u8]) -> Vec<u8> {
        let mut at = 2;
        if jpeg.get(2..4) == Some(&[0xFF, 0xE0]) {
            at = 4 + u16::from_be_bytes([jpeg[4], jpeg[5]]) as usize;
        }
        let mut out = jpeg[..at].to_vec();
        out.extend_from_slice(&[0xFF, 0xE1]);
        out.extend_from_slice(&((exif.len() + 8) as u16).to_be_bytes());
        out.extend_from_slice(b"Exif\0\0");
        out.extend_from_slice(exif);
        out.extend_from_slice(&jpeg[at..]);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    #[test]
    fn summary_reads_camera_fields() {
        let s = summarize(&camera_exif(6, (64, 48)));
        assert_eq!(s.make.as_deref(), Some("Thumbcam"));
        assert_eq!(s.model.as_deref(), Some("TC-100 Mark II"));
        assert_eq!(
            s.datetime.as_deref(),
            Some("2023:07:04 12:34:56"),
            "DateTimeOriginal wins over DateTime"
        );
        assert_eq!(s.dpi, Some((300.0, 300.0)));
        let (lat, lon) = s.gps.unwrap();
        assert!((lat - 47.6062).abs() < 1e-6 && (lon + 122.3321).abs() < 1e-6);
        assert_eq!(orientation(&camera_exif(6, (1, 1))), Some(6));
    }

    #[test]
    fn big_endian_blocks_are_read() {
        // IFD0 with one entry: Make "Hi" (inline), big-endian.
        let mut d = b"MM\0*\0\0\0\x08".to_vec();
        d.extend_from_slice(&[
            0, 1, 0x01, 0x0F, 0, 2, 0, 0, 0, 3, b'H', b'i', 0, 0, 0, 0, 0, 0,
        ]);
        assert_eq!(summarize(&d).make.as_deref(), Some("Hi"));
    }

    #[test]
    fn hostile_blocks_are_rejected_without_panicking() {
        // Entry count far beyond the buffer, offsets past the end, pointer loops.
        let mut d = b"II*\0\x08\0\0\0\xFF\xFF".to_vec();
        d.extend_from_slice(&[0; 20]);
        assert_eq!(summarize(&d), Summary::default());
        let mut looped = b"II*\0\x08\0\0\0\x01\0".to_vec();
        looped.extend_from_slice(&[0x69, 0x87, 4, 0, 1, 0, 0, 0, 8, 0, 0, 0, 8, 0, 0, 0]);
        assert_eq!(summarize(&looped), Summary::default());
        assert!(normalise(&looped, (1, 1)).is_some_and(|o| o.len() <= looped.len()));
        assert!(normalise(b"II*", (1, 1)).is_none());
        // A rational with a zero denominator is not a resolution.
        let mut zero = camera_exif(1, (1, 1));
        let off = {
            let t = Tiff::parse(&zero).unwrap();
            let x = t.find(t.ifd0().unwrap(), TAG_X_RESOLUTION).unwrap();
            t.u32(x.at + 8).unwrap() as usize
        };
        zero[off + 4..off + 8].fill(0);
        assert_eq!(summarize(&zero).dpi, None);
    }

    #[test]
    fn normalise_resets_orientation_dims_and_thumbnail() {
        let src = camera_exif(6, (64, 48));
        let out = normalise(&src, (48, 64)).unwrap();
        let t = Tiff::parse(&out).unwrap();
        assert_eq!(orientation(&out), Some(1));
        let ifd0 = t.ifd0().unwrap();
        let exif = t.pointer(ifd0, TAG_EXIF_IFD).unwrap();
        let px = |tag| {
            let e = t.find(exif, tag).unwrap();
            t.u32(e.at + 8).unwrap()
        };
        assert_eq!((px(TAG_PIXEL_X), px(TAG_PIXEL_Y)), (48, 64));
        let n = t.entries(ifd0).unwrap().len();
        assert_eq!(t.u32(ifd0 + 2 + n * 12), Some(0), "IFD1 link removed");
        assert!(
            out.len() < src.len() - 4000,
            "thumbnail bytes dropped: {} vs {}",
            out.len(),
            src.len()
        );
        let s = summarize(&out);
        assert_eq!(s.make.as_deref(), Some("Thumbcam"));
        assert!(s.gps.is_some() && s.dpi.is_some());
        assert_eq!(
            Tiff::parse(&out)
                .unwrap()
                .find(exif, TAG_MAKER_NOTE)
                .map(|e| Tiff::value_len(&e)),
            Some(Some(8)),
            "maker note survives with its offsets intact"
        );
    }

    #[test]
    fn container_extraction_and_dpi_fallback() {
        let exif = camera_exif(1, (2, 2));
        // PNG: signature, eXIf chunk, pHYs chunk (CRCs are not checked by the reader).
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&(exif.len() as u32).to_be_bytes());
        png.extend_from_slice(b"eXIf");
        png.extend_from_slice(&exif);
        png.extend_from_slice(&[0; 4]);
        assert_eq!(from_container(&png), Some(exif.as_slice()));
        assert_eq!(summarize_file(&png).make.as_deref(), Some("Thumbcam"));
        let mut phys = b"\x89PNG\r\n\x1a\n".to_vec();
        phys.extend_from_slice(&9u32.to_be_bytes());
        phys.extend_from_slice(b"pHYs");
        phys.extend_from_slice(&11811u32.to_be_bytes());
        phys.extend_from_slice(&11811u32.to_be_bytes());
        phys.extend_from_slice(&[1, 0, 0, 0, 0]);
        let (x, y) = summarize_file(&phys).dpi.unwrap();
        assert!((x - 300.0).abs() < 0.1 && (y - 300.0).abs() < 0.1);
        // WebP EXIF chunk, with the optional (non-standard) Exif prefix.
        let mut webp = b"RIFF\0\0\0\0WEBPEXIF".to_vec();
        webp.extend_from_slice(&((exif.len() + 6) as u32).to_le_bytes());
        webp.extend_from_slice(b"Exif\0\0");
        webp.extend_from_slice(&exif);
        assert_eq!(from_container(&webp), Some(exif.as_slice()));
        // JPEG: JFIF 96 dpi when there is no EXIF.
        let jfif = [
            0xFF, 0xD8, 0xFF, 0xE0, 0, 16, b'J', b'F', b'I', b'F', 0, 1, 1, 1, 0, 96, 0, 96, 0, 0,
        ];
        assert_eq!(summarize_file(&jfif).dpi, Some((96.0, 96.0)));
    }

    #[test]
    fn embed_in_tiff_adds_exif_ifd_and_gps() {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::new(8, 6));
        let mut tiff = Vec::new();
        img.write_to(
            &mut std::io::Cursor::new(&mut tiff),
            image::ImageFormat::Tiff,
        )
        .unwrap();
        let exif = camera_exif(6, (64, 48));
        let out = embed_in_tiff(tiff.clone(), Some(&exif), None, None, (8, 6));
        assert!(out.len() > tiff.len());
        let s = summarize(&out);
        assert_eq!(s.make.as_deref(), Some("Thumbcam"));
        assert_eq!(s.model.as_deref(), Some("TC-100 Mark II"));
        assert_eq!(s.datetime.as_deref(), Some("2023:07:04 12:34:56"));
        assert!(s.gps.is_some());
        assert_eq!(
            s.dpi,
            Some((300.0, 300.0)),
            "source resolution replaces the writer's placeholder"
        );
        assert_eq!(orientation(&out), None, "orientation is not carried");
        let t = Tiff::parse(&out).unwrap();
        let exif_ifd = t.pointer(t.ifd0().unwrap(), TAG_EXIF_IFD).unwrap();
        assert!(t.find(exif_ifd, TAG_MAKER_NOTE).is_none());
        assert_eq!(
            t.find(exif_ifd, TAG_PIXEL_X).map(|e| t.u32(e.at + 8)),
            Some(Some(8))
        );
        let back = image::load_from_memory_with_format(&out, image::ImageFormat::Tiff).unwrap();
        assert_eq!((back.width(), back.height()), (8, 6));
    }

    /// A little-endian TIFF with one IFD0 of `(tag, type, count, value-or-offset)` entries
    /// at offset 8, followed by `tail` (so offsets into it start at `8 + 2 + 12 * n + 4`).
    fn raw_tiff(entries: &[(u16, u16, u32, u32)], tail: &[u8]) -> Vec<u8> {
        let mut d = b"II*\0\x08\0\0\0".to_vec();
        d.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for &(tag, typ, count, value) in entries {
            d.extend_from_slice(&tag.to_le_bytes());
            d.extend_from_slice(&typ.to_le_bytes());
            d.extend_from_slice(&count.to_le_bytes());
            d.extend_from_slice(&value.to_le_bytes());
        }
        d.extend_from_slice(&[0; 4]);
        d.extend_from_slice(tail);
        d
    }

    fn tiff_file_with_metadata() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::new(8, 6));
        let mut tiff = Vec::new();
        img.write_to(
            &mut std::io::Cursor::new(&mut tiff),
            image::ImageFormat::Tiff,
        )
        .unwrap();
        let icc = vec![0x5A; 700];
        let xmp = b"<x:xmpmeta/>".to_vec();
        let out = embed_in_tiff(
            tiff,
            Some(&camera_exif(6, (64, 48))),
            Some(&icc),
            Some(&xmp),
            (8, 6),
        );
        (out, icc, xmp)
    }

    #[test]
    fn embed_in_tiff_adds_icc_and_xmp_even_without_exif() {
        let (with_all, icc, xmp) = tiff_file_with_metadata();
        assert_eq!(tiff_bytes(&with_all, TAG_ICC_PROFILE), Some(&icc[..]));
        assert_eq!(tiff_bytes(&with_all, TAG_XMP), Some(&xmp[..]));
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::new(8, 6));
        let mut plain = Vec::new();
        img.write_to(
            &mut std::io::Cursor::new(&mut plain),
            image::ImageFormat::Tiff,
        )
        .unwrap();
        let only = embed_in_tiff(plain.clone(), None, Some(&icc), None, (8, 6));
        assert_eq!(tiff_bytes(&only, TAG_ICC_PROFILE), Some(&icc[..]));
        assert!(tiff_bytes(&only, TAG_XMP).is_none());
        assert!(image::load_from_memory_with_format(&only, image::ImageFormat::Tiff).is_ok());
        // An unparsable EXIF block is skipped, the profile still lands.
        let bad = embed_in_tiff(plain, Some(b"garbage"), Some(&icc), None, (8, 6));
        assert_eq!(tiff_bytes(&bad, TAG_ICC_PROFILE), Some(&icc[..]));
    }

    #[test]
    fn from_tiff_is_a_compact_block_without_strip_layout_or_maker_note() {
        let (file, ..) = tiff_file_with_metadata();
        let block = from_tiff(&file).expect("EXIF from a TIFF file");
        assert!(block.len() < 800, "{} bytes", block.len());
        let s = summarize(&block);
        assert_eq!(s.make.as_deref(), Some("Thumbcam"));
        assert_eq!(s.model.as_deref(), Some("TC-100 Mark II"));
        assert_eq!(s.datetime.as_deref(), Some("2023:07:04 12:34:56"));
        assert!(s.gps.is_some());
        assert_eq!(s.dpi, Some((300.0, 300.0)));
        let t = Tiff::parse(&block).unwrap();
        let ifd0 = t.ifd0().unwrap();
        // Only descriptive tags and the two sub-IFD pointers: no StripOffsets (273),
        // ImageWidth (256) or the ICC and XMP arrays, whose bytes belong to the file.
        for e in t.entries(ifd0).unwrap() {
            assert!(
                TIFF_CARRY.contains(&e.tag)
                    || [
                        TAG_X_RESOLUTION,
                        TAG_Y_RESOLUTION,
                        TAG_RESOLUTION_UNIT,
                        TAG_ORIENTATION,
                        TAG_EXIF_IFD,
                        TAG_GPS_IFD
                    ]
                    .contains(&e.tag),
                "unexpected IFD0 tag {:#x}",
                e.tag
            );
        }
        let exif_ifd = t.pointer(ifd0, TAG_EXIF_IFD).unwrap();
        assert!(t.find(exif_ifd, TAG_MAKER_NOTE).is_none());
        // normalise accepts it like any EXIF block.
        assert!(normalise(&block, (8, 6)).is_some());
        // No EXIF at all: nothing to carry.
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::new(4, 4));
        let mut bare = Vec::new();
        img.write_to(
            &mut std::io::Cursor::new(&mut bare),
            image::ImageFormat::Tiff,
        )
        .unwrap();
        assert!(from_tiff(&bare).is_none());
    }

    #[test]
    fn hostile_tiff_sources_yield_nothing_instead_of_panicking_or_ballooning() {
        // EXIF IFD pointer past the end of the file.
        let f = raw_tiff(&[(TAG_EXIF_IFD, TYPE_LONG, 1, 0x00FF_FFFF)], &[]);
        assert!(from_tiff(&f).is_none());
        // Entry count promising more entries than the file holds.
        let mut f = raw_tiff(&[], &[]);
        f[8..10].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(from_tiff(&f).is_none());
        assert!(tiff_bytes(&f, TAG_XMP).is_none());
        // IFD0 offset past the end.
        let mut f = raw_tiff(&[], &[]);
        f[4..8].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
        assert!(from_tiff(&f).is_none());
        // A kept value whose offset and length leave the file.
        let f = raw_tiff(&[(TAG_MAKE, TYPE_ASCII, 1000, 0x4000)], b"short");
        assert!(from_tiff(&f).is_none());
        // A count whose byte length overflows 32 bits.
        let f = raw_tiff(&[(TAG_MAKE, TYPE_RATIONAL, u32::MAX, 18)], b"short");
        assert!(from_tiff(&f).is_none());
        // Seven descriptive tags pointing at the same 1 MiB: 7 MiB of copies would be
        // built from a 1 MiB file, so the cap refuses.
        let tail = vec![b'a'; 1 << 20];
        let base = 2 + 12 * 7 + 4 + 8;
        let tags = [
            TAG_IMAGE_DESCRIPTION,
            TAG_MAKE,
            TAG_MODEL,
            TAG_SOFTWARE,
            TAG_DATE_TIME,
            TAG_ARTIST,
            TAG_COPYRIGHT,
        ];
        let entries: Vec<_> = tags
            .iter()
            .map(|&t| (t, TYPE_ASCII, 1 << 20, base as u32))
            .collect();
        assert!(from_tiff(&raw_tiff(&entries, &tail)).is_none());
        // The same file with one such tag is fine.
        let one = raw_tiff(&[(TAG_MAKE, TYPE_ASCII, 1 << 20, 26)], &tail);
        assert!(from_tiff(&one).is_some());
        // ICC/XMP tags of the wrong type or out of bounds are not bytes to carry.
        let f = raw_tiff(&[(TAG_ICC_PROFILE, TYPE_SHORT, 4, 18)], &[0; 16]);
        assert!(tiff_bytes(&f, TAG_ICC_PROFILE).is_none());
        let f = raw_tiff(&[(TAG_XMP, 1, 100, 0x00FF_0000)], &[0; 16]);
        assert!(tiff_bytes(&f, TAG_XMP).is_none());
        let f = raw_tiff(&[(TAG_XMP, 7, 4, 0x41424344)], &[]);
        assert_eq!(tiff_bytes(&f, TAG_XMP), Some(&b"DCBA"[..]));
    }
}
