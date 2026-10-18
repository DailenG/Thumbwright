//! Android binary resources, just enough to find an app's launcher icon: the compiled
//! `AndroidManifest.xml` (AXML) and `resources.arsc` (the resource table).
//!
//! Layouts are those of the public AOSP `ResourceTypes.h`: every structure is a chunk with a
//! 16-bit type, a 16-bit header size and a 32-bit total size, little endian. All parsing is
//! bounds-checked over the in-memory bytes (no allocation proportional to a declared count
//! beyond what the bytes can hold), chunk walks are bounded, and strings are decoded lazily.

const RES_STRING_POOL: u16 = 0x0001;
const RES_TABLE: u16 = 0x0002;
const RES_XML: u16 = 0x0003;
const RES_XML_START_ELEMENT: u16 = 0x0102;
const RES_XML_RESOURCE_MAP: u16 = 0x0180;
const RES_TABLE_PACKAGE: u16 = 0x0200;
const RES_TABLE_TYPE: u16 = 0x0201;

const TYPE_REFERENCE: u8 = 0x01;
const TYPE_STRING: u8 = 0x03;
const TYPE_INT_COLOR_ARGB8: u8 = 0x1c;
const TYPE_INT_COLOR_RGB8: u8 = 0x1d;

const ATTR_ICON: u32 = 0x0101_0002;
const ATTR_ROUND_ICON: u32 = 0x0101_052c;
const ATTR_DRAWABLE: u32 = 0x0101_0199;

const NO_INDEX: u32 = 0xFFFF_FFFF;
/// Chunks visited in one walk.
const MAX_CHUNKS: usize = 1 << 20;
/// Attributes read from one element.
const MAX_ATTRIBUTES: usize = 256;
/// Longest string decoded from a pool (UTF-16 units / bytes).
const MAX_STRING: usize = 4096;

fn le16(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        d.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn le32(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        d.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

/// One chunk: its type, header size and bytes (header included), within `parent`.
struct Chunk<'a> {
    kind: u16,
    header: usize,
    bytes: &'a [u8],
}

/// Walks the chunks laid out back to back in `body`.
fn chunks(body: &[u8]) -> impl Iterator<Item = Chunk<'_>> {
    let mut at = 0usize;
    let mut n = 0usize;
    std::iter::from_fn(move || {
        n += 1;
        if n > MAX_CHUNKS {
            return None;
        }
        let kind = le16(body, at)?;
        let header = usize::from(le16(body, at + 2)?);
        let size = usize::try_from(le32(body, at + 4)?).ok()?;
        if size < 8 || header < 8 || header > size {
            return None;
        }
        let bytes = body.get(at..at.checked_add(size)?)?;
        // Chunks are 4-byte aligned in practice; tolerate sizes that are not.
        at = at.checked_add(size)?;
        Some(Chunk {
            kind,
            header,
            bytes,
        })
    })
}

// ---------------------------------------------------------------------------------------
// String pool

struct Pool<'a> {
    chunk: &'a [u8],
    header: usize,
    count: usize,
    utf8: bool,
    strings_start: usize,
}

impl<'a> Pool<'a> {
    fn parse(chunk: &'a [u8]) -> Option<Self> {
        let header = usize::from(le16(chunk, 2)?);
        let count = usize::try_from(le32(chunk, 8)?).ok()?;
        let flags = le32(chunk, 16)?;
        let strings_start = usize::try_from(le32(chunk, 20)?).ok()?;
        // The offset table must fit in the chunk.
        let table_end = header.checked_add(count.checked_mul(4)?)?;
        (table_end <= chunk.len() && strings_start <= chunk.len()).then_some(Self {
            chunk,
            header,
            count,
            utf8: flags & (1 << 8) != 0,
            strings_start,
        })
    }

    fn get(&self, i: usize) -> Option<String> {
        if i >= self.count {
            return None;
        }
        let off = usize::try_from(le32(self.chunk, self.header + i * 4)?).ok()?;
        let mut at = self.strings_start.checked_add(off)?;
        if self.utf8 {
            // UTF-16 length, then byte length; each one or two bytes.
            let skip = |at: usize| -> Option<(usize, usize)> {
                let b = usize::from(*self.chunk.get(at)?);
                if b & 0x80 != 0 {
                    Some((((b & 0x7f) << 8) | usize::from(*self.chunk.get(at + 1)?), 2))
                } else {
                    Some((b, 1))
                }
            };
            let (_, n) = skip(at)?;
            at += n;
            let (len, n) = skip(at)?;
            at += n;
            let bytes = self.chunk.get(at..at.checked_add(len.min(MAX_STRING))?)?;
            Some(String::from_utf8_lossy(bytes).into_owned())
        } else {
            let mut len = usize::from(le16(self.chunk, at)?);
            at += 2;
            if len & 0x8000 != 0 {
                len = ((len & 0x7fff) << 16) | usize::from(le16(self.chunk, at)?);
                at += 2;
            }
            let end = at.checked_add(len.min(MAX_STRING).checked_mul(2)?)?;
            let units: Vec<u16> = self
                .chunk
                .get(at..end)?
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect();
            Some(String::from_utf16_lossy(&units))
        }
    }
}

// ---------------------------------------------------------------------------------------
// Binary XML

/// A drawable reference found in binary XML or in the resource table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ref {
    /// Resource id (`0xPPTTEEEE`).
    Res(u32),
    /// A file path inside the package (the attribute is a literal string).
    Path(String),
    /// An inline ARGB colour.
    Color(u32),
}

struct Attr {
    name: usize,
    value: Ref,
}

/// A binary XML document reduced to what is needed: the start elements, with their
/// attributes, in document order.
struct Axml<'a> {
    pool: Pool<'a>,
    /// Resource id of each attribute-name string (`RES_XML_RESOURCE_MAP`), if present.
    res_map: Vec<u32>,
    elements: Vec<(usize, Vec<Attr>)>,
}

impl<'a> Axml<'a> {
    fn parse(data: &'a [u8]) -> Option<Self> {
        if le16(data, 0)? != RES_XML {
            return None;
        }
        let header = usize::from(le16(data, 2)?);
        let total = usize::try_from(le32(data, 4)?).ok()?;
        let body = data.get(header..total.min(data.len()))?;
        let mut pool = None;
        let mut res_map = Vec::new();
        let mut elements = Vec::new();
        for c in chunks(body) {
            match c.kind {
                RES_STRING_POOL if pool.is_none() => pool = Pool::parse(c.bytes),
                RES_XML_RESOURCE_MAP => {
                    res_map = c
                        .bytes
                        .get(c.header..)?
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|v| u32::from_le_bytes(*v))
                        .collect();
                }
                RES_XML_START_ELEMENT => {
                    // 16-byte node header, then the element body.
                    let b = c.bytes.get(c.header..)?;
                    let name = usize::try_from(le32(b, 4)?).ok()?;
                    let attr_start = usize::from(le16(b, 8)?);
                    let attr_size = usize::from(le16(b, 10)?).max(20);
                    let count = usize::from(le16(b, 12)?).min(MAX_ATTRIBUTES);
                    let mut attrs = Vec::new();
                    for i in 0..count {
                        let at = attr_start.checked_add(i.checked_mul(attr_size)?)?;
                        let Some(a) = b.get(at..at.checked_add(20)?) else {
                            break;
                        };
                        let name = usize::try_from(le32(a, 4)?).ok()?;
                        let raw = le32(a, 8)?;
                        let data_type = *a.get(15)?;
                        let value = le32(a, 16)?;
                        let value = match data_type {
                            TYPE_REFERENCE => Ref::Res(value),
                            TYPE_STRING => {
                                let idx = if raw != NO_INDEX { raw } else { value };
                                match pool
                                    .as_ref()
                                    .and_then(|p| p.get(usize::try_from(idx).ok()?))
                                {
                                    Some(s) => Ref::Path(s),
                                    None => continue,
                                }
                            }
                            TYPE_INT_COLOR_ARGB8 => Ref::Color(value),
                            TYPE_INT_COLOR_RGB8 => Ref::Color(value | 0xFF00_0000),
                            _ => continue,
                        };
                        attrs.push(Attr { name, value });
                    }
                    elements.push((name, attrs));
                }
                _ => {}
            }
        }
        Some(Self {
            pool: pool?,
            res_map,
            elements,
        })
    }

    fn name(&self, idx: usize) -> Option<String> {
        self.pool.get(idx)
    }

    /// Whether attribute-name string `idx` is the framework attribute `id` (by resource
    /// map, which survives name obfuscation) or, without a map, is called `name`.
    fn is_attr(&self, idx: usize, id: u32, name: &str) -> bool {
        match self.res_map.get(idx) {
            Some(&r) if r != 0 => r == id,
            _ => self.name(idx).is_some_and(|n| n == name),
        }
    }

    fn attr(&self, attrs: &[Attr], id: u32, name: &str) -> Option<Ref> {
        attrs
            .iter()
            .find(|a| self.is_attr(a.name, id, name))
            .map(|a| a.value.clone())
    }
}

/// `android:icon`, then `android:roundIcon`, of the manifest's `<application>` element.
pub fn manifest_icons(manifest: &[u8]) -> Vec<Ref> {
    let Some(x) = Axml::parse(manifest) else {
        return Vec::new();
    };
    let Some((_, attrs)) = x
        .elements
        .iter()
        .find(|(n, _)| x.name(*n).is_some_and(|n| n == "application"))
    else {
        return Vec::new();
    };
    [
        x.attr(attrs, ATTR_ICON, "icon"),
        x.attr(attrs, ATTR_ROUND_ICON, "roundIcon"),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// `(foreground, background)` drawables of an `<adaptive-icon>` document.
pub fn adaptive_layers(xml: &[u8]) -> Option<(Option<Ref>, Option<Ref>)> {
    let x = Axml::parse(xml)?;
    let root = x.name(x.elements.first()?.0)?;
    if root != "adaptive-icon" {
        return None;
    }
    let layer = |tag: &str| {
        x.elements
            .iter()
            .find(|(n, _)| x.name(*n).is_some_and(|n| n == tag))
            .and_then(|(_, attrs)| x.attr(attrs, ATTR_DRAWABLE, "drawable"))
    };
    Some((layer("foreground"), layer("background")))
}

// ---------------------------------------------------------------------------------------
// Resource table

/// What a resource entry holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    /// File path (or string) from the global string pool.
    Path(String),
    Reference(u32),
    /// ARGB.
    Color(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// Configuration density in dpi; 0 when unspecified, `0xFFFE` for `anydpi`,
    /// `0xFFFF` for `nodpi`.
    pub density: u16,
    pub value: Value,
}

pub struct Table<'a> {
    body: &'a [u8],
    global: Pool<'a>,
}

impl<'a> Table<'a> {
    pub fn parse(data: &'a [u8]) -> Option<Self> {
        if le16(data, 0)? != RES_TABLE {
            return None;
        }
        let header = usize::from(le16(data, 2)?);
        let total = usize::try_from(le32(data, 4)?).ok()?;
        let body = data.get(header..total.min(data.len()))?;
        let global = chunks(body)
            .find(|c| c.kind == RES_STRING_POOL)
            .and_then(|c| Pool::parse(c.bytes))?;
        Some(Self { body, global })
    }

    /// Every configuration's value of resource `id` (`0xPPTTEEEE`).
    pub fn lookup(&self, id: u32) -> Vec<Resolved> {
        let (package, type_id, entry) = (id >> 24, (id >> 16) & 0xff, id & 0xffff);
        let mut out = Vec::new();
        for pkg in chunks(self.body).filter(|c| c.kind == RES_TABLE_PACKAGE) {
            if le32(pkg.bytes, 8) != Some(package) {
                continue;
            }
            let Some(inner) = pkg.bytes.get(pkg.header..) else {
                continue;
            };
            for t in chunks(inner).filter(|c| c.kind == RES_TABLE_TYPE) {
                if u32::from(t.bytes.get(8).copied().unwrap_or(0)) != type_id {
                    continue;
                }
                if let Some(r) = self.type_entry(&t, entry) {
                    out.push(r);
                }
            }
        }
        out
    }

    fn type_entry(&self, t: &Chunk<'_>, entry: u32) -> Option<Resolved> {
        let b = t.bytes;
        let flags = *b.get(9)?;
        let count = usize::try_from(le32(b, 12)?).ok()?;
        let entries_start = usize::try_from(le32(b, 16)?).ok()?;
        let wanted = usize::try_from(entry).ok()?;
        let offset = if flags & 0x01 != 0 {
            // Sparse: (index, offset / 4) pairs of u16, sorted by index.
            (0..count).find_map(|i| {
                let at = t.header.checked_add(i.checked_mul(4)?)?;
                (usize::from(le16(b, at)?) == wanted)
                    .then(|| usize::from(le16(b, at + 2)?).checked_mul(4))
                    .flatten()
            })?
        } else if flags & 0x02 != 0 {
            let v = le16(b, t.header.checked_add(wanted.checked_mul(2)?)?)?;
            if wanted >= count || v == 0xFFFF {
                return None;
            }
            usize::from(v).checked_mul(4)?
        } else {
            if wanted >= count {
                return None;
            }
            let v = le32(b, t.header.checked_add(wanted.checked_mul(4)?)?)?;
            if v == NO_INDEX {
                return None;
            }
            usize::try_from(v).ok()?
        };
        let at = entries_start.checked_add(offset)?;
        let entry_flags = le16(b, at + 2)?;
        // Complex (bag) entries hold no single value.
        if entry_flags & 0x0001 != 0 {
            return None;
        }
        let (data_type, data) = if entry_flags & 0x0008 != 0 {
            // Compact: u16 key, u16 flags (value type in the high byte), u32 data.
            ((entry_flags >> 8) as u8, le32(b, at + 4)?)
        } else {
            let v = at.checked_add(usize::from(le16(b, at)?))?;
            (*b.get(v.checked_add(3)?)?, le32(b, v + 4)?)
        };
        let value = match data_type {
            TYPE_STRING => Value::Path(self.global.get(usize::try_from(data).ok()?)?),
            TYPE_REFERENCE => Value::Reference(data),
            TYPE_INT_COLOR_ARGB8 => Value::Color(data),
            TYPE_INT_COLOR_RGB8 => Value::Color(data | 0xFF00_0000),
            _ => return None,
        };
        // ResTable_config: size (u32) ... density (u16) at byte 14.
        let cfg_size = le32(b, 20)?;
        let density = if cfg_size >= 16 { le16(b, 20 + 14)? } else { 0 };
        Some(Resolved { density, value })
    }
}

#[cfg(test)]
pub(in crate::decode) mod tests {
    use super::*;

    /// A chunk with the given type, header bytes after the 8 common ones, and body.
    fn chunk(kind: u16, header_rest: &[u8], body: &[u8]) -> Vec<u8> {
        let header = 8 + header_rest.len();
        let size = header + body.len();
        let mut v = Vec::new();
        v.extend(kind.to_le_bytes());
        v.extend((header as u16).to_le_bytes());
        v.extend((size as u32).to_le_bytes());
        v.extend(header_rest);
        v.extend(body);
        v
    }

    /// A UTF-16 string pool of `strings`.
    fn pool(strings: &[&str], utf8: bool) -> Vec<u8> {
        let mut blob = Vec::new();
        let mut offsets = Vec::new();
        for s in strings {
            offsets.push(blob.len() as u32);
            if utf8 {
                blob.push(s.encode_utf16().count() as u8);
                blob.push(s.len() as u8);
                blob.extend(s.as_bytes());
                blob.push(0);
            } else {
                blob.extend((s.encode_utf16().count() as u16).to_le_bytes());
                for u in s.encode_utf16() {
                    blob.extend(u.to_le_bytes());
                }
                blob.extend([0, 0]);
            }
        }
        while blob.len() % 4 != 0 {
            blob.push(0);
        }
        let table = offsets.len() * 4;
        let strings_start = 28 + table;
        let mut header_rest = Vec::new();
        header_rest.extend((strings.len() as u32).to_le_bytes());
        header_rest.extend(0u32.to_le_bytes());
        header_rest.extend((if utf8 { 1u32 << 8 } else { 0 }).to_le_bytes());
        header_rest.extend((strings_start as u32).to_le_bytes());
        header_rest.extend(0u32.to_le_bytes());
        let mut body: Vec<u8> = offsets.iter().flat_map(|o| o.to_le_bytes()).collect();
        body.extend(blob);
        chunk(RES_STRING_POOL, &header_rest, &body)
    }

    pub(in crate::decode) fn attr(name: u32, raw: u32, data_type: u8, data: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend(NO_INDEX.to_le_bytes());
        v.extend(name.to_le_bytes());
        v.extend(raw.to_le_bytes());
        v.extend(8u16.to_le_bytes());
        v.push(0);
        v.push(data_type);
        v.extend(data.to_le_bytes());
        v
    }

    pub(in crate::decode) fn element(name: u32, attrs: &[Vec<u8>]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend(NO_INDEX.to_le_bytes());
        body.extend(name.to_le_bytes());
        body.extend(20u16.to_le_bytes());
        body.extend(20u16.to_le_bytes());
        body.extend((attrs.len() as u16).to_le_bytes());
        body.extend([0u8; 6]);
        for a in attrs {
            body.extend(a);
        }
        // Node header: line number and comment.
        chunk(
            RES_XML_START_ELEMENT,
            &[1, 0, 0, 0, 0xff, 0xff, 0xff, 0xff],
            &body,
        )
    }

    pub(in crate::decode) fn xml(
        strings: &[&str],
        res_map: &[u32],
        elements: &[Vec<u8>],
        utf8: bool,
    ) -> Vec<u8> {
        let mut body = pool(strings, utf8);
        if !res_map.is_empty() {
            let ids: Vec<u8> = res_map.iter().flat_map(|r| r.to_le_bytes()).collect();
            body.extend(chunk(RES_XML_RESOURCE_MAP, &[], &ids));
        }
        for e in elements {
            body.extend(e);
        }
        chunk(RES_XML, &[], &body)
    }

    #[test]
    fn manifest_application_icon_by_resource_map() {
        // Attribute names are obfuscated ("a", "b"); only the resource map identifies them.
        let m = xml(
            &["a", "b", "application", "manifest"],
            &[ATTR_ICON, ATTR_ROUND_ICON],
            &[
                element(3, &[]),
                element(
                    2,
                    &[
                        attr(1, NO_INDEX, TYPE_REFERENCE, 0x7f08_0001),
                        attr(0, NO_INDEX, TYPE_REFERENCE, 0x7f08_0002),
                    ],
                ),
            ],
            false,
        );
        // Declared order in the document is round icon first; `icon` still comes first out.
        assert_eq!(
            manifest_icons(&m),
            vec![Ref::Res(0x7f08_0002), Ref::Res(0x7f08_0001)]
        );
    }

    #[test]
    fn manifest_icon_by_name_utf8_pool_and_literal_path() {
        let m = xml(
            &["icon", "application", "res/mipmap/ic.png"],
            &[],
            &[element(1, &[attr(0, 2, TYPE_STRING, 2)])],
            true,
        );
        assert_eq!(
            manifest_icons(&m),
            vec![Ref::Path("res/mipmap/ic.png".into())]
        );
    }

    #[test]
    fn adaptive_icon_layers() {
        let m = xml(
            &["drawable", "adaptive-icon", "foreground", "background"],
            &[ATTR_DRAWABLE],
            &[
                element(1, &[]),
                element(3, &[attr(0, NO_INDEX, TYPE_INT_COLOR_RGB8, 0x0012_3456)]),
                element(2, &[attr(0, NO_INDEX, TYPE_REFERENCE, 0x7f07_0000)]),
            ],
            false,
        );
        assert_eq!(
            adaptive_layers(&m),
            Some((Some(Ref::Res(0x7f07_0000)), Some(Ref::Color(0xFF12_3456))))
        );
        assert_eq!(manifest_icons(&m), vec![]);
    }

    /// `(type id, flags, entries as (entry id, density, value type, value data))`
    pub(in crate::decode) fn type_chunk(
        type_id: u8,
        density: u16,
        entries: &[(u16, u8, u32)],
        sparse: bool,
    ) -> Vec<u8> {
        let count = entries
            .iter()
            .map(|e| usize::from(e.0) + 1)
            .max()
            .unwrap_or(0);
        let mut cfg = vec![0u8; 32];
        cfg[..4].copy_from_slice(&32u32.to_le_bytes());
        cfg[14..16].copy_from_slice(&density.to_le_bytes());
        let mut entry_blob = Vec::new();
        let mut offsets = vec![NO_INDEX; count];
        let mut sparse_pairs = Vec::new();
        for &(id, data_type, data) in entries {
            let off = entry_blob.len() as u32;
            offsets[usize::from(id)] = off;
            sparse_pairs.push((id, (off / 4) as u16));
            entry_blob.extend(8u16.to_le_bytes());
            entry_blob.extend(0u16.to_le_bytes());
            entry_blob.extend(0u32.to_le_bytes());
            entry_blob.extend(8u16.to_le_bytes());
            entry_blob.push(0);
            entry_blob.push(data_type);
            entry_blob.extend(data.to_le_bytes());
        }
        let mut table = Vec::new();
        if sparse {
            for (id, off) in &sparse_pairs {
                table.extend(id.to_le_bytes());
                table.extend(off.to_le_bytes());
            }
        } else {
            for o in &offsets {
                table.extend(o.to_le_bytes());
            }
        }
        let header_len = 8 + 4 + 4 + 4 + cfg.len();
        let mut rest = vec![type_id, u8::from(sparse), 0, 0];
        rest.extend((if sparse { sparse_pairs.len() } else { count } as u32).to_le_bytes());
        rest.extend(((header_len + table.len()) as u32).to_le_bytes());
        rest.extend(&cfg);
        let mut body = table;
        body.extend(entry_blob);
        chunk(RES_TABLE_TYPE, &rest, &body)
    }

    pub(in crate::decode) fn table(
        strings: &[&str],
        package_id: u32,
        types: &[Vec<u8>],
    ) -> Vec<u8> {
        let mut pkg_rest = Vec::new();
        pkg_rest.extend(package_id.to_le_bytes());
        pkg_rest.extend([0u8; 256]);
        pkg_rest.extend([0u8; 20]);
        let body: Vec<u8> = types.concat();
        let pkg = chunk(RES_TABLE_PACKAGE, &pkg_rest, &body);
        let mut inner = pool(strings, false);
        inner.extend(pkg);
        chunk(RES_TABLE, &1u32.to_le_bytes(), &inner)
    }

    #[test]
    fn table_lookup_across_configs_dense_and_sparse() {
        let t = table(
            &["res/mipmap-mdpi/ic.png", "res/mipmap-xxhdpi/ic.png"],
            0x7f,
            &[
                type_chunk(2, 160, &[(1, TYPE_STRING, 0)], false),
                type_chunk(2, 480, &[(1, TYPE_STRING, 1)], true),
                type_chunk(2, 0, &[(0, TYPE_REFERENCE, 0x7f02_0001)], false),
                type_chunk(3, 0, &[(1, TYPE_STRING, 0)], false),
            ],
        );
        let table = Table::parse(&t).unwrap();
        let mut got = table.lookup(0x7f02_0001);
        got.sort_by_key(|r| r.density);
        assert_eq!(
            got,
            vec![
                Resolved {
                    density: 160,
                    value: Value::Path("res/mipmap-mdpi/ic.png".into())
                },
                Resolved {
                    density: 480,
                    value: Value::Path("res/mipmap-xxhdpi/ic.png".into())
                },
            ]
        );
        assert_eq!(
            table.lookup(0x7f02_0000),
            vec![Resolved {
                density: 0,
                value: Value::Reference(0x7f02_0001)
            }]
        );
        assert!(table.lookup(0x7f02_0005).is_empty());
        assert!(table.lookup(0x7e02_0001).is_empty());
    }

    #[test]
    fn hostile_tables_and_manifests_do_not_panic() {
        let t = table(
            &["a"],
            0x7f,
            &[type_chunk(2, 0, &[(1, TYPE_STRING, 0)], false)],
        );
        // Every truncation and a lying size field per byte position.
        for cut in 0..t.len() {
            if let Some(tab) = Table::parse(&t[..cut]) {
                let _ = tab.lookup(0x7f02_0001);
            }
        }
        for i in 0..t.len() {
            let mut bad = t.clone();
            bad[i] = 0xFF;
            if let Some(tab) = Table::parse(&bad) {
                let _ = tab.lookup(0x7f02_0001);
            }
        }
        let m = xml(
            &["a"],
            &[],
            &[element(0, &[attr(0, 0, TYPE_STRING, 0)])],
            false,
        );
        for cut in 0..m.len() {
            let _ = manifest_icons(&m[..cut]);
            let _ = adaptive_layers(&m[..cut]);
        }
        for i in 0..m.len() {
            let mut bad = m.clone();
            bad[i] = 0xFF;
            let _ = manifest_icons(&bad);
            let _ = adaptive_layers(&bad);
        }
        // A pool claiming 2^32 - 1 strings.
        let mut huge = pool(&["a"], false);
        huge[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Pool::parse(&huge).is_none());
        assert!(Table::parse(&[]).is_none());
    }

    #[test]
    fn compact_entries() {
        // Dense chunk with one entry, rewritten as a compact entry (u16 key, u16 flags with
        // the value type in the high byte, u32 data) as `aapt2 --enable-compact-entries` emits.
        let mut chunk = type_chunk(2, 240, &[(0, TYPE_REFERENCE, 0x7f02_0007)], false);
        let start = usize::try_from(le32(&chunk, 16).unwrap()).unwrap();
        chunk[start..start + 2].copy_from_slice(&0u16.to_le_bytes());
        chunk[start + 2..start + 4]
            .copy_from_slice(&(0x0008u16 | (u16::from(TYPE_REFERENCE) << 8)).to_le_bytes());
        chunk[start + 4..start + 8].copy_from_slice(&0x7f02_0007u32.to_le_bytes());
        let t = table(&["x"], 0x7f, &[chunk]);
        assert_eq!(
            Table::parse(&t).unwrap().lookup(0x7f02_0000),
            vec![Resolved {
                density: 240,
                value: Value::Reference(0x7f02_0007)
            }]
        );
    }
}
