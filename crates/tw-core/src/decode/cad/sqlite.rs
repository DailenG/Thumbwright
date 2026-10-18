//! Minimal read-only SQLite file reader: enough to find a table in `sqlite_master` and
//! walk its rows (rowid tables only), reassembling payloads that spill into overflow
//! pages.
//!
//! The file is untrusted. Every offset goes through `get(..)`, every page is visited at
//! most once per walk (cycle and fan-out guard), traversal is iterative, and a single
//! record is capped by [`MAX_RECORD_BYTES`] and by the size of the file before any
//! buffer is allocated.

use std::borrow::Cow;

use crate::decode::{DecodeError, Result};

const MAGIC: &[u8; 16] = b"SQLite format 3\0";
/// Largest single record (row payload) this reader reassembles.
pub const MAX_RECORD_BYTES: u64 = super::MAX_PREVIEW_BYTES as u64;

const LEAF_TABLE: u8 = 0x0D;
const INTERIOR_TABLE: u8 = 0x05;

pub struct Db<'a> {
    data: &'a [u8],
    page_size: usize,
    usable: usize,
    pages: usize,
}

/// One column value of a decoded record. Reals are carried as raw bytes (unused here).
#[derive(Debug, PartialEq)]
pub enum Value<'a> {
    Null,
    Int(i64),
    Real,
    Blob(&'a [u8]),
    Text(&'a [u8]),
}

fn be16(d: &[u8], at: usize) -> Option<usize> {
    Some(usize::from(u16::from_be_bytes(
        d.get(at..at.checked_add(2)?)?.try_into().ok()?,
    )))
}

fn be32(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        d.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

/// SQLite varint (1 to 9 bytes); returns the value and its length.
pub fn varint(d: &[u8], at: usize) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for i in 0..8 {
        let b = *d.get(at.checked_add(i)?)?;
        v = v << 7 | u64::from(b & 0x7F);
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    let b = *d.get(at.checked_add(8)?)?;
    Some((v << 8 | u64::from(b), 9))
}

impl<'a> Db<'a> {
    pub fn open(data: &'a [u8]) -> Result<Self> {
        if data.get(..16) != Some(&MAGIC[..]) {
            return Err(DecodeError::Unsupported);
        }
        let raw = be16(data, 16).ok_or_else(|| DecodeError::malformed("short SQLite header"))?;
        let page_size = match raw {
            1 => 65536,
            n if (512..=32768).contains(&n) && n.is_power_of_two() => n,
            _ => return Err(DecodeError::malformed("bad SQLite page size")),
        };
        let reserved = usize::from(*data.get(20).unwrap_or(&0));
        let usable = page_size.saturating_sub(reserved);
        if usable < 480 {
            return Err(DecodeError::malformed("bad SQLite reserved space"));
        }
        // 1 = UTF-8 (0 in files that never stored text).
        if !matches!(be32(data, 56), Some(0 | 1)) {
            return Err(DecodeError::Unsupported);
        }
        let pages = data.len() / page_size;
        if pages == 0 {
            return Err(DecodeError::malformed("SQLite file shorter than one page"));
        }
        Ok(Self {
            data,
            page_size,
            usable,
            pages,
        })
    }

    fn page(&self, n: u32) -> Option<&'a [u8]> {
        let start = (n as usize).checked_sub(1)?.checked_mul(self.page_size)?;
        self.data.get(start..start.checked_add(self.page_size)?)
    }

    /// Root page of table `name` (case-insensitive), `None` if the schema has no such table.
    pub fn table_root(&self, name: &str) -> Result<Option<u32>> {
        let mut found = None;
        self.for_each_row(1, &mut |_, payload| {
            if let Some(cols) = parse_record(payload)
                && let [Value::Text(kind), Value::Text(n), _, Value::Int(root), ..] =
                    cols.as_slice()
                && kind.eq_ignore_ascii_case(b"table")
                && n.eq_ignore_ascii_case(name.as_bytes())
            {
                found = u32::try_from(*root).ok();
                return Ok(false);
            }
            Ok(true)
        })?;
        Ok(found)
    }

    /// Calls `f(rowid, record payload)` for each row of the table rooted at `root`, in
    /// b-tree order, until `f` returns `Ok(false)`.
    pub fn for_each_row(
        &self,
        root: u32,
        f: &mut dyn FnMut(i64, &[u8]) -> Result<bool>,
    ) -> Result<()> {
        let mut seen = vec![false; self.pages + 1];
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            let index = n as usize;
            if index == 0 || index > self.pages {
                return Err(DecodeError::malformed("SQLite page number out of range"));
            }
            if std::mem::replace(&mut seen[index], true) {
                return Err(DecodeError::malformed("SQLite page visited twice"));
            }
            let page = self
                .page(n)
                .ok_or_else(|| DecodeError::malformed("SQLite page outside file"))?;
            let hdr = if n == 1 { 100 } else { 0 };
            let kind = *page
                .get(hdr)
                .ok_or_else(|| DecodeError::malformed("short SQLite page"))?;
            let cells =
                be16(page, hdr + 3).ok_or_else(|| DecodeError::malformed("short SQLite page"))?;
            match kind {
                INTERIOR_TABLE => {
                    let array = hdr + 12;
                    let mut children = Vec::with_capacity(cells + 1);
                    for i in 0..cells {
                        let cell = be16(page, array + 2 * i)
                            .ok_or_else(|| DecodeError::malformed("short cell array"))?;
                        children.push(
                            be32(page, cell)
                                .ok_or_else(|| DecodeError::malformed("short interior cell"))?,
                        );
                    }
                    children.push(
                        be32(page, hdr + 8)
                            .ok_or_else(|| DecodeError::malformed("short interior header"))?,
                    );
                    // Popped last-in-first-out: push in reverse to walk in key order.
                    stack.extend(children.into_iter().rev());
                }
                LEAF_TABLE => {
                    let array = hdr + 8;
                    for i in 0..cells {
                        let cell = be16(page, array + 2 * i)
                            .ok_or_else(|| DecodeError::malformed("short cell array"))?;
                        let (rowid, payload) = self.leaf_cell(page, cell)?;
                        if !f(rowid, &payload)? {
                            return Ok(());
                        }
                    }
                }
                _ => return Err(DecodeError::Unsupported),
            }
        }
        Ok(())
    }

    fn leaf_cell(&self, page: &'a [u8], at: usize) -> Result<(i64, Cow<'a, [u8]>)> {
        let short = || DecodeError::malformed("short leaf cell");
        let (size, n1) = varint(page, at).ok_or_else(short)?;
        let (rowid, n2) = varint(page, at + n1).ok_or_else(short)?;
        let body = at + n1 + n2;
        let usable = self.usable as u64;
        let max_local = usable - 35;
        if size > MAX_RECORD_BYTES {
            return Err(DecodeError::TooLarge);
        }
        if size <= max_local {
            let end = body.checked_add(size as usize).ok_or_else(short)?;
            return Ok((
                rowid as i64,
                Cow::Borrowed(page.get(body..end).ok_or_else(short)?),
            ));
        }
        // A record cannot be larger than the file that holds it.
        if size > self.data.len() as u64 {
            return Err(DecodeError::malformed("SQLite record larger than file"));
        }
        let min_local = (usable - 12) * 32 / 255 - 23;
        let k = min_local + (size - min_local) % (usable - 4);
        let local = if k <= max_local { k } else { min_local } as usize;
        let mut buf = Vec::with_capacity(size as usize);
        buf.extend_from_slice(page.get(body..body + local).ok_or_else(short)?);
        let mut next = be32(page, body + local).ok_or_else(short)?;
        while (buf.len() as u64) < size {
            let overflow = self
                .page(next)
                .ok_or_else(|| DecodeError::malformed("SQLite overflow page outside file"))?;
            let take = (self.usable - 4).min((size as usize) - buf.len());
            buf.extend_from_slice(
                overflow
                    .get(4..4 + take)
                    .ok_or_else(|| DecodeError::malformed("short overflow page"))?,
            );
            next = be32(overflow, 0).ok_or_else(short)?;
            if next == 0 && (buf.len() as u64) < size {
                return Err(DecodeError::malformed("SQLite overflow chain ends early"));
            }
        }
        Ok((rowid as i64, Cow::Owned(buf)))
    }
}

/// Decodes a record into its column values, `None` when the record is malformed.
pub fn parse_record(rec: &[u8]) -> Option<Vec<Value<'_>>> {
    let (header_len, n) = varint(rec, 0)?;
    let header_len = usize::try_from(header_len).ok()?;
    if header_len < n || header_len > rec.len() {
        return None;
    }
    let mut at = n;
    let mut body = header_len;
    let mut cols = Vec::new();
    while at < header_len {
        let (serial, n) = varint(rec, at)?;
        at += n;
        let (value, len) = match serial {
            0 => (Value::Null, 0),
            1..=6 => {
                let len = [0, 1, 2, 3, 4, 6, 8][serial as usize];
                let bytes = rec.get(body..body.checked_add(len)?)?;
                let mut v = if bytes[0] & 0x80 != 0 { -1i64 } else { 0 };
                for &b in bytes {
                    v = v << 8 | i64::from(b);
                }
                (Value::Int(v), len)
            }
            7 => {
                rec.get(body..body.checked_add(8)?)?;
                (Value::Real, 8)
            }
            8 => (Value::Int(0), 0),
            9 => (Value::Int(1), 0),
            10 | 11 => return None,
            s => {
                let blob = s % 2 == 0;
                let len = usize::try_from((s - if blob { 12 } else { 13 }) / 2).ok()?;
                let bytes = rec.get(body..body.checked_add(len)?)?;
                (
                    if blob {
                        Value::Blob(bytes)
                    } else {
                        Value::Text(bytes)
                    },
                    len,
                )
            }
        };
        body = body.checked_add(len)?;
        cols.push(value);
    }
    Some(cols)
}

#[cfg(test)]
pub(crate) mod fixtures {
    /// SQLite varint encoding.
    pub fn put_varint(out: &mut Vec<u8>, v: u64) {
        if v < 0x80 {
            out.push(v as u8);
        } else if v <= 0x3FFF {
            out.extend_from_slice(&[(v >> 7) as u8 | 0x80, (v & 0x7F) as u8]);
        } else {
            // Up to 8 bytes of 7 bits is enough for any fixture value.
            let mut groups = Vec::new();
            let mut x = v;
            while x > 0 {
                groups.push((x & 0x7F) as u8);
                x >>= 7;
            }
            groups.reverse();
            let last = groups.len() - 1;
            for (i, g) in groups.iter().enumerate() {
                out.push(if i < last { g | 0x80 } else { *g });
            }
        }
    }

    pub fn record(cols: &[Col]) -> Vec<u8> {
        let mut header = Vec::new();
        let mut body = Vec::new();
        for c in cols {
            match c {
                Col::Int(v) => {
                    // 8-byte big-endian for simplicity (serial type 6).
                    header.push(6);
                    body.extend_from_slice(&v.to_be_bytes());
                }
                Col::Text(t) => {
                    put_varint(&mut header, 13 + 2 * t.len() as u64);
                    body.extend_from_slice(t.as_bytes());
                }
                Col::Blob(b) => {
                    put_varint(&mut header, 12 + 2 * b.len() as u64);
                    body.extend_from_slice(b);
                }
            }
        }
        let mut rec = Vec::new();
        put_varint(&mut rec, header.len() as u64 + 1);
        rec.extend_from_slice(&header);
        rec.extend_from_slice(&body);
        rec
    }

    pub enum Col<'a> {
        Int(i64),
        Text(&'a str),
        Blob(&'a [u8]),
    }

    pub const PAGE: usize = 1024;
    const USABLE: usize = PAGE;

    /// A rowid table leaf cell for `rowid`; the part of `payload` beyond the local
    /// limit goes to overflow pages allocated from `overflow` (page numbers first,
    /// content appended to `extra`).
    pub fn leaf_cell(rowid: u64, payload: &[u8], first_overflow: u32) -> (Vec<u8>, Vec<Vec<u8>>) {
        let size = payload.len();
        let max_local = USABLE - 35;
        let mut cell = Vec::new();
        put_varint(&mut cell, size as u64);
        put_varint(&mut cell, rowid);
        if size <= max_local {
            cell.extend_from_slice(payload);
            return (cell, Vec::new());
        }
        let min_local = (USABLE - 12) * 32 / 255 - 23;
        let k = min_local + (size - min_local) % (USABLE - 4);
        let local = if k <= max_local { k } else { min_local };
        cell.extend_from_slice(&payload[..local]);
        let rest = &payload[local..];
        let chunks: Vec<&[u8]> = rest.chunks(USABLE - 4).collect();
        cell.extend_from_slice(&first_overflow.to_be_bytes());
        let mut pages = Vec::new();
        for (i, chunk) in chunks.iter().enumerate() {
            let next = if i + 1 < chunks.len() {
                first_overflow + i as u32 + 1
            } else {
                0
            };
            let mut p = next.to_be_bytes().to_vec();
            p.extend_from_slice(chunk);
            p.resize(PAGE, 0);
            pages.push(p);
        }
        (cell, pages)
    }

    /// Leaf table page holding `cells` (header at `hdr`: 100 on page 1, else 0).
    pub fn leaf_page(cells: &[Vec<u8>], hdr: usize) -> Vec<u8> {
        let mut p = vec![0u8; PAGE];
        p[hdr] = 0x0D;
        p[hdr + 3..hdr + 5].copy_from_slice(&(cells.len() as u16).to_be_bytes());
        let mut top = PAGE;
        for (i, c) in cells.iter().enumerate() {
            top -= c.len();
            p[top..top + c.len()].copy_from_slice(c);
            p[hdr + 8 + 2 * i..hdr + 10 + 2 * i].copy_from_slice(&(top as u16).to_be_bytes());
        }
        p[hdr + 5..hdr + 7].copy_from_slice(&(top as u16).to_be_bytes());
        p
    }

    pub fn interior_page(children: &[u32], right: u32) -> Vec<u8> {
        let mut p = vec![0u8; PAGE];
        p[0] = 0x05;
        p[3..5].copy_from_slice(&(children.len() as u16).to_be_bytes());
        p[8..12].copy_from_slice(&right.to_be_bytes());
        let mut top = PAGE;
        for (i, &child) in children.iter().enumerate() {
            let mut cell = child.to_be_bytes().to_vec();
            put_varint(&mut cell, i as u64 + 1);
            top -= cell.len();
            p[top..top + cell.len()].copy_from_slice(&cell);
            p[12 + 2 * i..14 + 2 * i].copy_from_slice(&(top as u16).to_be_bytes());
        }
        p
    }

    /// Database file: page 1 is the schema (`tables`: name, root page), then `pages`.
    pub fn database(tables: &[(&str, u32)], pages: Vec<Vec<u8>>) -> Vec<u8> {
        let cells: Vec<Vec<u8>> = tables
            .iter()
            .enumerate()
            .map(|(i, (name, root))| {
                let rec = record(&[
                    Col::Text("table"),
                    Col::Text(name),
                    Col::Text(name),
                    Col::Int(i64::from(*root)),
                    Col::Text("CREATE TABLE x(a)"),
                ]);
                let mut cell = Vec::new();
                put_varint(&mut cell, rec.len() as u64);
                put_varint(&mut cell, i as u64 + 1);
                cell.extend_from_slice(&rec);
                cell
            })
            .collect();
        let mut p1 = leaf_page(&cells, 100);
        p1[..16].copy_from_slice(b"SQLite format 3\0");
        p1[16..18].copy_from_slice(&(PAGE as u16).to_be_bytes());
        p1[56..60].copy_from_slice(&1u32.to_be_bytes());
        let mut out = p1;
        for p in pages {
            out.extend_from_slice(&p);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    fn rows(db: &Db, root: u32) -> Result<Vec<(i64, Vec<u8>)>> {
        let mut out = Vec::new();
        db.for_each_row(root, &mut |id, p| {
            out.push((id, p.to_vec()));
            Ok(true)
        })?;
        Ok(out)
    }

    #[test]
    fn varints() {
        for v in [0u64, 1, 127, 128, 16383, 16384, 1 << 28, 1 << 40] {
            let mut b = Vec::new();
            put_varint(&mut b, v);
            assert_eq!(varint(&b, 0), Some((v, b.len())), "{v}");
        }
        assert_eq!(varint(&[0xFF; 9], 0), Some((u64::MAX, 9)));
        assert_eq!(varint(&[0x80], 0), None);
    }

    #[test]
    fn schema_lookup_inline_and_overflow_rows() {
        let big: Vec<u8> = (0..5000u32).map(|i| (i * 7 % 251) as u8).collect();
        let small = record(&[Col::Int(7), Col::Blob(b"tiny")]);
        let big_rec = record(&[Col::Int(8), Col::Blob(&big)]);
        let (cell1, _) = leaf_cell(1, &small, 0);
        // Overflow pages start at page 4.
        let (cell2, overflow) = leaf_cell(2, &big_rec, 4);
        let mut pages = vec![
            leaf_page(std::slice::from_ref(&cell1), 0),
            leaf_page(&[cell2], 0),
        ];
        pages.extend(overflow);
        // Page 2 = Other (small row), page 3 = CanvasPreview (overflow row).
        let db = database(&[("Other", 2), ("CanvasPreview", 3)], pages);
        let d = Db::open(&db).unwrap();
        assert_eq!(d.table_root("canvaspreview").unwrap(), Some(3));
        assert_eq!(d.table_root("Missing").unwrap(), None);
        let r = rows(&d, 3).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, 2);
        assert_eq!(r[0].1, big_rec);
        let cols = parse_record(&r[0].1).unwrap();
        assert_eq!(cols[1], Value::Blob(&big));
        assert_eq!(rows(&d, 2).unwrap()[0].1, small);
    }

    #[test]
    fn interior_pages_are_walked_in_order() {
        let mk = |id: u64, v: i64| leaf_cell(id, &record(&[Col::Int(v)]), 0).0;
        let pages = vec![
            interior_page(&[3], 4),
            leaf_page(&[mk(1, 10), mk(2, 20)], 0),
            leaf_page(&[mk(3, 30)], 0),
        ];
        let db = database(&[("T", 2)], pages);
        let d = Db::open(&db).unwrap();
        let ids: Vec<i64> = rows(&d, 2).unwrap().iter().map(|r| r.0).collect();
        assert_eq!(ids, [1, 2, 3]);
    }

    #[test]
    fn hostile_databases() {
        assert_eq!(
            Db::open(b"not sqlite").err(),
            Some(DecodeError::Unsupported)
        );
        let mut short = b"SQLite format 3\0".to_vec();
        short.extend_from_slice(&[0; 8]);
        assert!(matches!(Db::open(&short), Err(DecodeError::Malformed(_))));

        // Interior page that points at itself.
        let db = database(&[("T", 2)], vec![interior_page(&[2], 2)]);
        let d = Db::open(&db).unwrap();
        assert!(matches!(rows(&d, 2), Err(DecodeError::Malformed(_))));
        // Page numbers outside the file, and zero.
        let db = database(&[("T", 2)], vec![interior_page(&[99], 100)]);
        let d = Db::open(&db).unwrap();
        assert!(matches!(rows(&d, 2), Err(DecodeError::Malformed(_))));
        assert!(matches!(rows(&d, 0), Err(DecodeError::Malformed(_))));

        // Record claiming 4 GiB, and one that is bigger than the file.
        for claimed in [u64::from(u32::MAX), 100_000] {
            let mut cell = Vec::new();
            put_varint(&mut cell, claimed);
            put_varint(&mut cell, 1);
            cell.extend_from_slice(&[0; 40]);
            cell.extend_from_slice(&0u32.to_be_bytes());
            let db = database(&[("T", 2)], vec![leaf_page(&[cell], 0)]);
            let d = Db::open(&db).unwrap();
            let err = rows(&d, 2).unwrap_err();
            assert!(matches!(
                err,
                DecodeError::TooLarge | DecodeError::Malformed(_)
            ));
        }

        // Overflow chain that ends early, and one that loops on itself.
        let big = vec![7u8; 3000];
        let (cell, mut overflow) = leaf_cell(1, &big, 3);
        overflow[0][..4].copy_from_slice(&0u32.to_be_bytes());
        let mut pages = vec![leaf_page(std::slice::from_ref(&cell), 0)];
        pages.extend(overflow);
        let db = database(&[("T", 2)], pages);
        let d = Db::open(&db).unwrap();
        assert!(matches!(rows(&d, 2), Err(DecodeError::Malformed(_))));
        let (cell, mut overflow) = leaf_cell(1, &big, 3);
        overflow[0][..4].copy_from_slice(&3u32.to_be_bytes());
        let mut pages = vec![leaf_page(&[cell], 0)];
        pages.extend(overflow);
        let db = database(&[("T", 2)], pages);
        let d = Db::open(&db).unwrap();
        assert_eq!(rows(&d, 2).unwrap()[0].1.len(), 3000);

        // Cell pointer beyond the page.
        let mut p = leaf_page(&[vec![1, 1, 0]], 0);
        p[8..10].copy_from_slice(&0xFFFFu16.to_be_bytes());
        let db = database(&[("T", 2)], vec![p]);
        let d = Db::open(&db).unwrap();
        assert!(matches!(rows(&d, 2), Err(DecodeError::Malformed(_))));
        // Truncated file: the table's page is missing.
        let db = database(&[("T", 2)], vec![]);
        let d = Db::open(&db).unwrap();
        assert!(matches!(rows(&d, 2), Err(DecodeError::Malformed(_))));
    }

    #[test]
    fn records_with_bad_headers_are_rejected() {
        assert_eq!(parse_record(&[]), None);
        assert_eq!(parse_record(&[9, 1]), None);
        // Text column claiming more bytes than present.
        assert_eq!(parse_record(&[2, 0x7F, 1]), None);
        assert_eq!(parse_record(&[2, 10]), None);
    }
}
