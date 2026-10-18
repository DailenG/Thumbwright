//! PLY (Stanford polygon file): `ascii 1.0`, `binary_little_endian 1.0` and
//! `binary_big_endian 1.0`. Vertex positions come from the `x`, `y`, `z` properties of
//! the `vertex` element and faces from the `vertex_indices` (or `vertex_index`) list of
//! the `face` element, triangulated as fans. All other elements and properties are
//! parsed only far enough to be skipped.
//!
//! Before anything proportional to a declared element count is allocated, the count is
//! checked against the bytes remaining in the file (each instance occupies at least
//! its fixed-size properties and list counts).

use super::{Deadline, DecodeError, MAX_TRIANGLES, MAX_VERTICES, Mesh, Result};

const MAX_HEADER_LINES: usize = 4096;
const MAX_HEADER_BYTES: usize = 64 * 1024;
/// Element instances between deadline checks.
const CHECK_EVERY: usize = 1 << 12;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Scalar {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    F32,
    F64,
}

impl Scalar {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "char" | "int8" => Self::I8,
            "uchar" | "uint8" => Self::U8,
            "short" | "int16" => Self::I16,
            "ushort" | "uint16" => Self::U16,
            "int" | "int32" => Self::I32,
            "uint" | "uint32" => Self::U32,
            "float" | "float32" => Self::F32,
            "double" | "float64" => Self::F64,
            _ => return None,
        })
    }

    fn size(self) -> usize {
        match self {
            Self::I8 | Self::U8 => 1,
            Self::I16 | Self::U16 => 2,
            Self::I32 | Self::U32 | Self::F32 => 4,
            Self::F64 => 8,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Ascii,
    Little,
    Big,
}

struct Property {
    name: String,
    ty: Scalar,
    /// Count type when the property is a list.
    list: Option<Scalar>,
}

struct Element {
    name: String,
    count: u64,
    props: Vec<Property>,
}

struct Header {
    format: Format,
    elements: Vec<Element>,
    body: usize,
}

fn header(data: &[u8]) -> Result<Header> {
    let bad = |m: &str| DecodeError::malformed(format!("PLY header: {m}"));
    let mut format = None;
    let mut elements: Vec<Element> = Vec::new();
    let mut pos = 0;
    for n in 0..MAX_HEADER_LINES {
        let rest = data.get(pos..).filter(|r| !r.is_empty());
        let rest = rest.ok_or_else(|| bad("no end_header"))?;
        let len = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
        pos += (len + 1).min(rest.len());
        if pos > MAX_HEADER_BYTES {
            return Err(bad("too long"));
        }
        let text = String::from_utf8_lossy(&rest[..len]);
        let line = text.trim();
        let mut t = line.split_ascii_whitespace();
        let word = t.next().unwrap_or("");
        if n == 0 {
            if word != "ply" {
                return Err(DecodeError::Unsupported);
            }
            continue;
        }
        match word {
            "format" => {
                format = Some(match t.next() {
                    Some("ascii") => Format::Ascii,
                    Some("binary_little_endian") => Format::Little,
                    Some("binary_big_endian") => Format::Big,
                    _ => return Err(bad("unknown format")),
                });
            }
            "element" => {
                let name = t.next().ok_or_else(|| bad("element name"))?;
                let count = t
                    .next()
                    .and_then(|c| c.parse::<u64>().ok())
                    .ok_or_else(|| bad("element count"))?;
                elements.push(Element {
                    name: name.to_string(),
                    count,
                    props: Vec::new(),
                });
            }
            "property" => {
                let el = elements
                    .last_mut()
                    .ok_or_else(|| bad("property before element"))?;
                let first = t.next().ok_or_else(|| bad("property type"))?;
                let prop = if first == "list" {
                    let count = t.next().and_then(Scalar::parse);
                    let item = t.next().and_then(Scalar::parse);
                    let name = t.next();
                    match (count, item, name) {
                        (Some(c), Some(i), Some(n)) => Property {
                            name: n.to_string(),
                            ty: i,
                            list: Some(c),
                        },
                        _ => return Err(bad("list property")),
                    }
                } else {
                    match (Scalar::parse(first), t.next()) {
                        (Some(ty), Some(n)) => Property {
                            name: n.to_string(),
                            ty,
                            list: None,
                        },
                        _ => return Err(bad("property")),
                    }
                };
                el.props.push(prop);
            }
            "end_header" => {
                return Ok(Header {
                    format: format.ok_or_else(|| bad("no format"))?,
                    elements,
                    body: pos,
                });
            }
            _ => {}
        }
    }
    Err(bad("too many lines"))
}

/// Sequential reader over the body in the file's encoding.
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
    format: Format,
}

impl Cursor<'_> {
    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn scalar(&mut self, ty: Scalar) -> Option<f64> {
        if self.format == Format::Ascii {
            let rest = self.data.get(self.pos..)?;
            let start = rest.iter().position(|b| !b.is_ascii_whitespace())?;
            let rest = &rest[start..];
            let len = rest
                .iter()
                .position(|b| b.is_ascii_whitespace())
                .unwrap_or(rest.len());
            self.pos += start + len;
            return std::str::from_utf8(&rest[..len]).ok()?.parse().ok();
        }
        let size = ty.size();
        let b = self.data.get(self.pos..self.pos.checked_add(size)?)?;
        self.pos += size;
        let be = self.format == Format::Big;
        fn arr<const N: usize>(b: &[u8], be: bool) -> [u8; N] {
            let mut a = [0u8; N];
            a.copy_from_slice(b);
            if be {
                a.reverse();
            }
            a
        }
        // `arr` normalises to little-endian order.
        Some(match ty {
            Scalar::U8 => f64::from(b[0]),
            Scalar::I8 => f64::from(b[0] as i8),
            Scalar::U16 => f64::from(u16::from_le_bytes(arr(b, be))),
            Scalar::I16 => f64::from(i16::from_le_bytes(arr(b, be))),
            Scalar::U32 => f64::from(u32::from_le_bytes(arr(b, be))),
            Scalar::I32 => f64::from(i32::from_le_bytes(arr(b, be))),
            Scalar::F32 => f64::from(f32::from_le_bytes(arr(b, be))),
            Scalar::F64 => f64::from_le_bytes(arr(b, be)),
        })
    }

    /// Skips `n` binary items of `ty` (ASCII reads the tokens instead).
    fn skip(&mut self, n: u64, ty: Scalar) -> Option<()> {
        if self.format == Format::Ascii {
            for _ in 0..n {
                self.scalar(ty)?;
            }
            return Some(());
        }
        let bytes = usize::try_from(n).ok()?.checked_mul(ty.size())?;
        if bytes > self.remaining() {
            return None;
        }
        self.pos += bytes;
        Some(())
    }
}

pub fn parse<'a>(data: &[u8], deadline: &Deadline) -> Result<Mesh<'a>> {
    let h = header(data)?;
    let truncated = || DecodeError::malformed("PLY body truncated");
    let mut cur = Cursor {
        data,
        pos: h.body,
        format: h.format,
    };
    let mut verts: Vec<[f32; 3]> = Vec::new();
    let mut tris: Vec<[u32; 3]> = Vec::new();
    let (mut have_vertex, mut have_face) = (false, false);

    for el in &h.elements {
        let is_vertex = el.name == "vertex" && !have_vertex;
        let is_face = el.name == "face" && !have_face;

        // Every instance occupies at least this many bytes.
        let min_each: u64 = el
            .props
            .iter()
            .map(|p| match (cur.format, p.list) {
                (Format::Ascii, _) => 2,
                (_, None) => p.ty.size() as u64,
                (_, Some(c)) => c.size() as u64,
            })
            .sum();
        if min_each > 0 {
            let need = el
                .count
                .checked_mul(min_each)
                .ok_or_else(|| DecodeError::malformed("PLY element count overflow"))?;
            // ASCII separators between tokens: the final one is optional.
            let need = if cur.format == Format::Ascii {
                need.saturating_sub(1)
            } else {
                need
            };
            if need > cur.remaining() as u64 {
                return Err(DecodeError::malformed(
                    "PLY element count exceeds file size",
                ));
            }
        }
        if is_vertex && el.count > MAX_VERTICES as u64 {
            return Err(DecodeError::TooLarge);
        }

        let axis = |name: &str| {
            el.props
                .iter()
                .position(|p| p.name == name && p.list.is_none())
        };
        let xyz = if is_vertex {
            match (axis("x"), axis("y"), axis("z")) {
                (Some(x), Some(y), Some(z)) => Some([x, y, z]),
                _ => return Err(DecodeError::malformed("PLY vertex lacks x, y, z")),
            }
        } else {
            None
        };
        let face_prop = if is_face {
            el.props.iter().position(|p| {
                p.list.is_some() && (p.name == "vertex_indices" || p.name == "vertex_index")
            })
        } else {
            None
        };
        if is_vertex {
            // Bounded by the byte check above: at least 3 bytes per vertex.
            verts.reserve(usize::try_from(el.count).unwrap_or(0));
        }

        if min_each == 0 {
            continue;
        }
        let mut face: Vec<u32> = Vec::new();
        for i in 0..el.count {
            if (i as usize).is_multiple_of(CHECK_EVERY) {
                deadline.check()?;
            }
            let mut pos = [0f32; 3];
            face.clear();
            for (pi, p) in el.props.iter().enumerate() {
                match p.list {
                    None => {
                        let v = cur.scalar(p.ty).ok_or_else(truncated)?;
                        if let Some(axes) = xyz
                            && let Some(k) = axes.iter().position(|&a| a == pi)
                        {
                            pos[k] = v as f32;
                        }
                    }
                    Some(count_ty) => {
                        let n = cur.scalar(count_ty).ok_or_else(truncated)?;
                        if !(0.0..4_294_967_296.0).contains(&n) {
                            return Err(DecodeError::malformed("PLY list length"));
                        }
                        let n = n as u64;
                        if face_prop == Some(pi) {
                            if cur.format != Format::Ascii
                                && n.saturating_mul(p.ty.size() as u64) > cur.remaining() as u64
                            {
                                return Err(truncated());
                            }
                            for _ in 0..n {
                                let idx = cur.scalar(p.ty).ok_or_else(truncated)?;
                                // Out-of-range or fractional indices poison the polygon.
                                face.push(if idx >= 0.0 && idx < f64::from(u32::MAX) {
                                    idx as u32
                                } else {
                                    u32::MAX
                                });
                            }
                        } else {
                            cur.skip(n, p.ty).ok_or_else(truncated)?;
                        }
                    }
                }
            }
            if is_vertex {
                verts.push(pos);
            } else if is_face && face.len() >= 3 {
                if tris.len() + face.len() - 2 > MAX_TRIANGLES {
                    return Err(DecodeError::TooLarge);
                }
                for w in face[1..].windows(2) {
                    tris.push([face[0], w[0], w[1]]);
                }
            }
        }
        have_vertex |= is_vertex;
        have_face |= is_face;
        if have_vertex && have_face {
            break;
        }
    }

    tris.retain(|t| t.iter().all(|&i| (i as usize) < verts.len()));
    if verts.is_empty() || tris.is_empty() {
        return Err(DecodeError::malformed("PLY has no faces"));
    }
    Ok(Mesh::Indexed { verts, tris })
}

#[cfg(test)]
pub(crate) mod fixtures {
    /// Tetrahedron body (positions `v`, triangles `f`) in `format`:
    /// `ascii`, `binary_little_endian` or `binary_big_endian`.
    pub fn tetra(format: &str, v: &[[f32; 3]; 4], f: &[[u32; 3]]) -> Vec<u8> {
        let mut out = format!(
            "ply\nformat {format} 1.0\ncomment test\nelement vertex 4\nproperty float x\nproperty float y\n\
             property float z\nproperty uchar red\nelement face {}\nproperty list uchar int vertex_indices\n\
             end_header\n",
            f.len()
        )
        .into_bytes();
        let be = format == "binary_big_endian";
        for p in v {
            if format == "ascii" {
                out.extend_from_slice(format!("{} {} {} 200\n", p[0], p[1], p[2]).as_bytes());
            } else {
                for c in p {
                    out.extend_from_slice(&if be { c.to_be_bytes() } else { c.to_le_bytes() });
                }
                out.push(200);
            }
        }
        for t in f {
            if format == "ascii" {
                out.extend_from_slice(format!("3 {} {} {}\n", t[0], t[1], t[2]).as_bytes());
            } else {
                out.push(3);
                for i in t {
                    out.extend_from_slice(&if be { i.to_be_bytes() } else { i.to_le_bytes() });
                }
            }
        }
        out
    }

    pub const V: [[f32; 3]; 4] = [
        [0.0, 0.0, 0.0],
        [1.0, 0.0, 0.0],
        [0.0, 1.0, 0.0],
        [0.0, 0.0, 1.0],
    ];
    pub const F: [[u32; 3]; 4] = [[0, 2, 1], [0, 1, 3], [0, 3, 2], [1, 2, 3]];
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use std::time::Duration;

    fn dl() -> Deadline {
        Deadline::after(Duration::from_secs(30))
    }

    #[test]
    fn all_three_encodings_agree() {
        for format in ["ascii", "binary_little_endian", "binary_big_endian"] {
            let mesh = parse(&tetra(format, &V, &F), &dl()).unwrap();
            assert_eq!(mesh.triangle_count(), 4, "{format}");
            assert_eq!(mesh.triangle(3).unwrap(), [V[1], V[2], V[3]], "{format}");
        }
    }

    #[test]
    fn quad_faces_and_skipped_elements() {
        let mut text = String::from(
            "ply\r\nformat ascii 1.0\r\nelement vertex 4\r\nproperty double x\r\nproperty double y\r\n\
             property double z\r\nelement edge 1\r\nproperty int a\r\nproperty int b\r\nelement face 1\r\n\
             property list uchar uint vertex_index\r\nproperty int flags\r\nend_header\r\n",
        );
        text.push_str("0 0 0\n1 0 0\n1 1 0\n0 1 0\n5 6\n4 0 1 2 3 7\n");
        let mesh = parse(text.as_bytes(), &dl()).unwrap();
        assert_eq!(mesh.triangle_count(), 2);
    }

    #[test]
    fn hostile_ply() {
        assert!(parse(b"", &dl()).is_err());
        assert_eq!(
            parse(b"solid x", &dl()).err(),
            Some(DecodeError::Unsupported)
        );
        let rejected = |data: &[u8]| assert!(parse(data, &dl()).is_err(), "{data:?}");
        rejected(b"ply\nformat ascii 1.0\n");
        rejected(b"ply\nformat ascii 1.0\nelement vertex 3\nproperty float x\nend_header\n1 2 3\n");
        rejected(b"ply\nelement vertex 1\nend_header\n");
        rejected(b"ply\nformat binary_little_endian 1.0\nproperty float x\nend_header\n");
        rejected(b"ply\nformat ascii 1.0\nelement vertex 1\nproperty banana x\nend_header\n");
        rejected(b"ply\nformat ascii 1.0\nelement vertex 1\nproperty float x\nend_header\n1\n");

        // Counts lie: far more vertices than bytes, u64 overflow, over the vertex cap.
        for count in ["1000000", "18446744073709551615", "4000000000", "5000001"] {
            let mut file = format!(
                "ply\nformat binary_little_endian 1.0\nelement vertex {count}\nproperty float x\n\
                 property float y\nproperty float z\nelement face 1\n\
                 property list uchar int vertex_indices\nend_header\n"
            )
            .into_bytes();
            file.extend_from_slice(&[0u8; 64]);
            assert!(
                matches!(
                    parse(&file, &dl()),
                    Err(DecodeError::Malformed(_) | DecodeError::TooLarge)
                ),
                "{count}"
            );
        }

        // List length lying: claims 255 indices in the last face, file ends.
        let mut f = tetra("binary_little_endian", &V, &F);
        let at = f.len() - 13;
        f[at] = 255;
        assert!(matches!(parse(&f, &dl()), Err(DecodeError::Malformed(_))));
        // Truncated body.
        let f = tetra("binary_big_endian", &V, &F);
        assert!(parse(&f[..f.len() - 5], &dl()).is_err());
        // Indices out of range leave no faces.
        let bad = tetra("ascii", &V, &[[0, 1, 99]]);
        assert!(matches!(parse(&bad, &dl()), Err(DecodeError::Malformed(_))));
        // Header larger than the allowance.
        let mut huge = b"ply\nformat ascii 1.0\n".to_vec();
        huge.extend(std::iter::repeat_n(b'c', MAX_HEADER_BYTES * 2));
        assert!(parse(&huge, &dl()).is_err());
    }
}
