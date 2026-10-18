//! Minimal EXIF reader for the property handler: the handful of tags Explorer's Details
//! pane shows. Hostile-input safe: every read is bounds-checked, only IFD0 and the Exif
//! sub-IFD are visited (no chain following, so no loops), and entry counts are capped.

/// Decoded EXIF values (absent when the tag is missing or malformed).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Exif {
    pub make: Option<String>,
    pub model: Option<String>,
    /// 1..=8 as stored.
    pub orientation: Option<u16>,
    /// Pixels per inch.
    pub x_resolution: Option<f64>,
    pub y_resolution: Option<f64>,
    /// `DateTimeOriginal` as a Windows FILETIME (100 ns ticks since 1601), the camera's
    /// wall-clock time read as UTC (EXIF carries no zone).
    pub date_taken: Option<u64>,
    /// Seconds.
    pub exposure_time: Option<f64>,
    pub f_number: Option<f64>,
    pub iso: Option<u32>,
    /// Millimetres.
    pub focal_length: Option<f64>,
}

/// Entries read per IFD; real files have a few dozen.
const MAX_ENTRIES: usize = 512;
/// ASCII values longer than this are ignored.
const MAX_ASCII: usize = 256;

const TAG_MAKE: u16 = 0x010F;
const TAG_MODEL: u16 = 0x0110;
const TAG_ORIENTATION: u16 = 0x0112;
const TAG_X_RES: u16 = 0x011A;
const TAG_Y_RES: u16 = 0x011B;
const TAG_RES_UNIT: u16 = 0x0128;
const TAG_EXIF_IFD: u16 = 0x8769;
const TAG_EXPOSURE_TIME: u16 = 0x829A;
const TAG_F_NUMBER: u16 = 0x829D;
const TAG_ISO: u16 = 0x8827;
const TAG_DATE_ORIGINAL: u16 = 0x9003;
const TAG_FOCAL_LENGTH: u16 = 0x920A;

#[derive(Clone, Copy)]
struct Tiff<'a> {
    d: &'a [u8],
    le: bool,
}

impl Tiff<'_> {
    fn u16(&self, at: usize) -> Option<u16> {
        let b: [u8; 2] = self.d.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(if self.le {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    }

    fn u32(&self, at: usize) -> Option<u32> {
        let b: [u8; 4] = self.d.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(if self.le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }
}

/// One IFD entry: the value bytes live inline (<= 4 bytes) or at an offset.
struct Entry {
    tag: u16,
    ty: u16,
    count: u32,
    /// Offset of the value bytes within the TIFF data.
    value_at: usize,
}

fn entries(t: Tiff, ifd: usize) -> Vec<Entry> {
    let Some(n) = t.u16(ifd) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for i in 0..usize::from(n).min(MAX_ENTRIES) {
        let at = ifd + 2 + i * 12;
        let (Some(tag), Some(ty), Some(count)) = (t.u16(at), t.u16(at + 2), t.u32(at + 4)) else {
            break;
        };
        let size = match ty {
            1 | 2 | 7 => 1u64,
            3 => 2,
            4 => 4,
            5 => 8,
            _ => continue,
        } * u64::from(count);
        let value_at = if size <= 4 {
            at + 8
        } else {
            match t.u32(at + 8) {
                Some(o) => o as usize,
                None => continue,
            }
        };
        out.push(Entry {
            tag,
            ty,
            count,
            value_at,
        });
    }
    out
}

impl Entry {
    fn short(&self, t: Tiff) -> Option<u32> {
        match self.ty {
            3 if self.count >= 1 => t.u16(self.value_at).map(u32::from),
            4 if self.count >= 1 => t.u32(self.value_at),
            _ => None,
        }
    }

    fn rational(&self, t: Tiff) -> Option<f64> {
        if self.ty != 5 || self.count < 1 {
            return None;
        }
        let (n, d) = (t.u32(self.value_at)?, t.u32(self.value_at + 4)?);
        (d != 0).then(|| f64::from(n) / f64::from(d))
    }

    fn ascii(&self, t: Tiff) -> Option<String> {
        if self.ty != 2 || self.count == 0 || self.count as usize > MAX_ASCII {
            return None;
        }
        let raw =
            t.d.get(self.value_at..self.value_at.checked_add(self.count as usize)?)?;
        let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        let s = String::from_utf8_lossy(&raw[..end]).trim().to_owned();
        (!s.is_empty()).then_some(s)
    }
}

/// `YYYY:MM:DD HH:MM:SS` to a FILETIME.
fn parse_datetime(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 19
        || b[4] != b':'
        || b[7] != b':'
        || b[10] != b' '
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let part = s.get(r)?;
        if !part.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1601..=9999).contains(&y)
        || !(1..=12).contains(&mo)
        || !(1..=31).contains(&d)
        || h > 23
        || mi > 59
        || sec > 60
    {
        return None;
    }
    // Days from civil date (Howard Hinnant's algorithm), shifted to the 1601 epoch.
    let y_adj = if mo <= 2 { y - 1 } else { y };
    let era = y_adj.div_euclid(400);
    let yoe = y_adj - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days_since_1970 = era * 146_097 + doe - 719_468;
    let days_since_1601 = days_since_1970 + 134_774;
    let secs = days_since_1601 * 86_400 + h * 3600 + mi * 60 + sec;
    u64::try_from(secs).ok()?.checked_mul(10_000_000)
}

/// Parses an EXIF block: a TIFF stream, optionally preceded by the `Exif\0\0` marker.
pub fn parse(block: &[u8]) -> Exif {
    let data = block.strip_prefix(b"Exif\0\0").unwrap_or(block);
    let le = match data.get(..2) {
        Some(b"II") => true,
        Some(b"MM") => false,
        _ => return Exif::default(),
    };
    let t = Tiff { d: data, le };
    if t.u16(2) != Some(42) {
        return Exif::default();
    }
    let Some(ifd0) = t.u32(4) else {
        return Exif::default();
    };
    let mut out = Exif::default();
    let mut res_unit = 2u32;
    let mut exif_ifd = None;
    for e in entries(t, ifd0 as usize) {
        match e.tag {
            TAG_MAKE => out.make = e.ascii(t),
            TAG_MODEL => out.model = e.ascii(t),
            TAG_ORIENTATION => {
                out.orientation = e.short(t).and_then(|v| u16::try_from(v).ok());
            }
            TAG_X_RES => out.x_resolution = e.rational(t),
            TAG_Y_RES => out.y_resolution = e.rational(t),
            TAG_RES_UNIT => res_unit = e.short(t).unwrap_or(2),
            TAG_EXIF_IFD => exif_ifd = e.short(t),
            _ => {}
        }
    }
    if res_unit == 3 {
        // Pixels per centimetre.
        out.x_resolution = out.x_resolution.map(|r| r * 2.54);
        out.y_resolution = out.y_resolution.map(|r| r * 2.54);
    }
    if let Some(ifd) = exif_ifd {
        for e in entries(t, ifd as usize) {
            match e.tag {
                TAG_EXPOSURE_TIME => out.exposure_time = e.rational(t),
                TAG_F_NUMBER => out.f_number = e.rational(t),
                TAG_ISO => out.iso = e.short(t),
                TAG_DATE_ORIGINAL => {
                    out.date_taken = e.ascii(t).and_then(|s| parse_datetime(&s));
                }
                TAG_FOCAL_LENGTH => out.focal_length = e.rational(t),
                _ => {}
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Little-endian TIFF with IFD0 {Make, Orientation, Exif IFD} and the Exif IFD {ISO,
    /// FNumber, DateTimeOriginal}.
    fn sample() -> Vec<u8> {
        let mut d = b"II*\0".to_vec();
        d.extend_from_slice(&8u32.to_le_bytes());
        let entry = |tag: u16, ty: u16, count: u32, val: u32| {
            let mut e = Vec::new();
            e.extend_from_slice(&tag.to_le_bytes());
            e.extend_from_slice(&ty.to_le_bytes());
            e.extend_from_slice(&count.to_le_bytes());
            e.extend_from_slice(&val.to_le_bytes());
            e
        };
        // IFD0 at 8: 3 entries (2 + 36 + 4 = 42 bytes) -> make string at 50, exif IFD at 56.
        d.extend_from_slice(&3u16.to_le_bytes());
        d.extend(entry(TAG_MAKE, 2, 6, 50));
        d.extend(entry(TAG_ORIENTATION, 3, 1, 6));
        d.extend(entry(TAG_EXIF_IFD, 4, 1, 56));
        d.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(d.len(), 50);
        d.extend_from_slice(b"Canon\0");
        // Exif IFD at 56: 3 entries (2 + 36 + 4 = 42) -> rational at 98, date at 106.
        d.extend_from_slice(&3u16.to_le_bytes());
        d.extend(entry(TAG_ISO, 3, 1, 400));
        d.extend(entry(TAG_F_NUMBER, 5, 1, 98));
        d.extend(entry(TAG_DATE_ORIGINAL, 2, 20, 106));
        d.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(d.len(), 98);
        d.extend_from_slice(&28u32.to_le_bytes());
        d.extend_from_slice(&10u32.to_le_bytes());
        d.extend_from_slice(b"2024:02:29 13:45:10\0");
        d
    }

    #[test]
    fn reads_tags_from_both_ifds() {
        let e = parse(&sample());
        assert_eq!(e.make.as_deref(), Some("Canon"));
        assert_eq!(e.orientation, Some(6));
        assert_eq!(e.iso, Some(400));
        assert_eq!(e.f_number, Some(2.8));
        // 2024-02-29 is day 19 782 since 1970; the FILETIME epoch (1601) is 134 774 days earlier.
        assert_eq!(
            e.date_taken,
            Some(((19_782 + 134_774) * 86_400 + 13 * 3600 + 45 * 60 + 10) * 10_000_000)
        );
    }

    #[test]
    fn exif_marker_prefix_is_accepted() {
        let mut v = b"Exif\0\0".to_vec();
        v.extend(sample());
        assert_eq!(parse(&v), parse(&sample()));
    }

    #[test]
    fn truncated_and_hostile_blocks_never_panic() {
        let full = sample();
        for n in 0..full.len() {
            let _ = parse(&full[..n]);
        }
        // Offsets pointing far outside the block, and a huge entry count.
        let mut hostile = b"MM\0*".to_vec();
        hostile.extend_from_slice(&8u32.to_be_bytes());
        hostile.extend_from_slice(&u16::MAX.to_be_bytes());
        hostile.extend_from_slice(&TAG_MAKE.to_be_bytes());
        hostile.extend_from_slice(&2u16.to_be_bytes());
        hostile.extend_from_slice(&u32::MAX.to_be_bytes());
        hostile.extend_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(parse(&hostile), Exif::default());
    }

    #[test]
    fn impossible_dates_are_rejected() {
        assert_eq!(parse_datetime("2024:13:01 00:00:00"), None);
        assert_eq!(parse_datetime("2024:02:29 24:00:00"), None);
        assert_eq!(parse_datetime("not a date at all"), None);
        assert_eq!(
            parse_datetime("1970:01-01 00:00:00"),
            None,
            "separator must be a colon-digit layout"
        );
    }
}
