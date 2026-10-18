//! STL: binary (`80-byte header, u32 count, 50 bytes per triangle`) and ASCII
//! (`facet normal ... vertex x y z` x3 `endfacet`). Normals are ignored; shading uses
//! the geometric face normal.
//!
//! Binary files are recognised by size (`84 + 50 * count`), which beats the `solid`
//! prefix many binary exporters also write; a longer file is accepted as padded binary
//! unless it starts with `solid`.

use super::{Deadline, DecodeError, MAX_TRIANGLES, Mesh, Result, parse_f32};

/// Tokens between deadline checks.
const CHECK_EVERY: usize = 1 << 16;

pub fn parse<'a>(data: &'a [u8], deadline: &Deadline) -> Result<Mesh<'a>> {
    let count = data
        .get(80..84)
        .map(|b| u64::from(u32::from_le_bytes([b[0], b[1], b[2], b[3]])));
    let binary_len = count.map(|n| 84 + 50 * n);
    let exact = binary_len == Some(data.len() as u64);
    let solid = data.trim_ascii_start().len() >= 5
        && data.trim_ascii_start()[..5].eq_ignore_ascii_case(b"solid");
    if exact || (!solid && binary_len.is_some_and(|l| l <= data.len() as u64)) {
        return read_binary(data, count.unwrap_or(0));
    }
    if solid {
        return ascii(data, deadline);
    }
    Err(DecodeError::malformed("not an STL file"))
}

fn read_binary(data: &[u8], count: u64) -> Result<Mesh<'_>> {
    if count == 0 {
        return Err(DecodeError::malformed("STL has no triangles"));
    }
    if count > MAX_TRIANGLES as u64 {
        return Err(DecodeError::TooLarge);
    }
    // `count` was verified against the file length by the caller.
    let records = data
        .get(84..84 + 50 * count as usize)
        .ok_or_else(|| DecodeError::malformed("STL shorter than its triangle count"))?;
    Ok(Mesh::StlBinary(records))
}

fn ascii<'a>(data: &[u8], deadline: &Deadline) -> Result<Mesh<'a>> {
    let mut corners: Vec<[f32; 3]> = Vec::new();
    let mut tokens = data
        .split(|b| b.is_ascii_whitespace())
        .filter(|t| !t.is_empty());
    let mut seen = 0usize;
    while let Some(token) = tokens.next() {
        seen += 1;
        if seen.is_multiple_of(CHECK_EVERY) {
            deadline.check()?;
        }
        if !token.eq_ignore_ascii_case(b"vertex") {
            continue;
        }
        let mut v = [0f32; 3];
        for c in &mut v {
            *c = tokens
                .next()
                .and_then(parse_f32)
                .ok_or_else(|| DecodeError::malformed("bad STL vertex"))?;
        }
        if corners.len() / 3 >= MAX_TRIANGLES {
            return Err(DecodeError::TooLarge);
        }
        corners.push(v);
    }
    corners.truncate(corners.len() / 3 * 3);
    if corners.is_empty() {
        return Err(DecodeError::malformed("STL has no triangles"));
    }
    Ok(Mesh::Flat(corners))
}

#[cfg(test)]
pub(crate) mod fixtures {
    /// Binary STL of the triangles `tris` (each three `[x, y, z]` corners).
    pub fn binary(tris: &[[[f32; 3]; 3]]) -> Vec<u8> {
        let mut out = vec![0u8; 80];
        out.extend_from_slice(&(tris.len() as u32).to_le_bytes());
        for t in tris {
            out.extend_from_slice(&[0; 12]);
            for v in t {
                for c in v {
                    out.extend_from_slice(&c.to_le_bytes());
                }
            }
            out.extend_from_slice(&[0; 2]);
        }
        out
    }

    /// The twelve triangles of an axis-aligned cube spanning `lo..hi`.
    pub fn cube(lo: f32, hi: f32) -> Vec<[[f32; 3]; 3]> {
        let p = |i: u8| {
            [
                if i & 1 == 0 { lo } else { hi },
                if i & 2 == 0 { lo } else { hi },
                if i & 4 == 0 { lo } else { hi },
            ]
        };
        const QUADS: [[u8; 4]; 6] = [
            [0, 1, 3, 2],
            [4, 6, 7, 5],
            [0, 4, 5, 1],
            [2, 3, 7, 6],
            [0, 2, 6, 4],
            [1, 5, 7, 3],
        ];
        QUADS
            .iter()
            .flat_map(|q| [[p(q[0]), p(q[1]), p(q[2])], [p(q[0]), p(q[2]), p(q[3])]])
            .collect()
    }
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
    fn binary_stl_parses_in_place() {
        let data = binary(&cube(0.0, 1.0));
        let mesh = parse(&data, &dl()).unwrap();
        assert_eq!(mesh.triangle_count(), 12);
        assert_eq!(mesh.triangle(0).unwrap()[1], [1.0, 0.0, 0.0]);
        assert_eq!(mesh.triangle(12), None);
    }

    #[test]
    fn binary_with_solid_header_and_padding() {
        let mut data = binary(&cube(0.0, 1.0));
        data[..5].copy_from_slice(b"solid");
        assert_eq!(parse(&data, &dl()).unwrap().triangle_count(), 12);
        let mut padded = binary(&cube(0.0, 1.0));
        padded.extend_from_slice(&[0; 7]);
        assert_eq!(parse(&padded, &dl()).unwrap().triangle_count(), 12);
    }

    #[test]
    fn ascii_stl() {
        let text = "solid t\n facet normal 0 0 1\n  outer loop\n   vertex 0 0 0\n   vertex 1.5e0 0 0\n   \
                    vertex 0 1 -2.5\n  endloop\n endfacet\n facet normal 0 0 1\n outer loop\n vertex 1 1 1\n \
                    vertex 2 2 2\n vertex 3 3 3\n endloop\nendfacet\nendsolid t\n";
        let mesh = parse(text.as_bytes(), &dl()).unwrap();
        assert_eq!(mesh.triangle_count(), 2);
        assert_eq!(
            mesh.triangle(0).unwrap(),
            [[0.0; 3], [1.5, 0.0, 0.0], [0.0, 1.0, -2.5]]
        );
        // A trailing partial facet is dropped.
        let cut = format!("{text}facet normal 0 0 0\nvertex 9 9 9\nvertex 8 8 8\n");
        assert_eq!(parse(cut.as_bytes(), &dl()).unwrap().triangle_count(), 2);
    }

    #[test]
    fn hostile_stl() {
        assert!(parse(b"", &dl()).is_err());
        assert!(parse(&[0u8; 20], &dl()).is_err());
        // Count lies: claims 100 triangles, holds one, header does not say "solid".
        let mut lying = binary(&cube(0.0, 1.0)[..1]);
        lying[80..84].copy_from_slice(&100u32.to_le_bytes());
        assert!(matches!(
            parse(&lying, &dl()),
            Err(DecodeError::Malformed(_))
        ));
        // Count u32::MAX.
        lying[80..84].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            parse(&lying, &dl()),
            Err(DecodeError::Malformed(_))
        ));
        // Zero triangles.
        assert!(matches!(
            parse(&binary(&[]), &dl()),
            Err(DecodeError::Malformed(_))
        ));
        // Truncated ASCII vertex and ASCII without facets.
        assert!(matches!(
            parse(b"solid x\nvertex 1 2", &dl()),
            Err(DecodeError::Malformed(_))
        ));
        assert!(matches!(
            parse(b"solid x\nendsolid x\n", &dl()),
            Err(DecodeError::Malformed(_))
        ));
        assert!(matches!(
            parse(b"solid x\nvertex 1 2 abc\n", &dl()),
            Err(DecodeError::Malformed(_))
        ));
    }

    #[test]
    fn expired_deadline_stops_a_long_ascii_file() {
        let mut text = String::from("solid big\n");
        for _ in 0..CHECK_EVERY {
            text.push_str("vertex 0 0 0\n");
        }
        let expired = Deadline::after(Duration::ZERO);
        assert_eq!(
            parse(text.as_bytes(), &expired).err(),
            Some(DecodeError::Timeout)
        );
    }
}
