//! Wavefront OBJ: `v x y z [...]` vertices and `f` faces (`i`, `i/t`, `i//n`, `i/t/n`
//! corners, 1-based, negative indices relative to the vertices defined so far).
//! Polygons are triangulated as fans; every other statement (normals, texture
//! coordinates, materials, groups, lines, points) is ignored.

use super::{Deadline, DecodeError, MAX_TRIANGLES, MAX_VERTICES, Mesh, Result, parse_f32};

/// Lines between deadline checks.
const CHECK_EVERY: usize = 1 << 14;

pub fn parse<'a>(data: &[u8], deadline: &Deadline) -> Result<Mesh<'a>> {
    let mut verts: Vec<[f32; 3]> = Vec::new();
    let mut tris: Vec<[u32; 3]> = Vec::new();
    let mut face: Vec<u32> = Vec::new();

    for (n, line) in data.split(|&b| b == b'\n').enumerate() {
        if n % CHECK_EVERY == 0 {
            deadline.check()?;
        }
        let line = line.trim_ascii();
        let mut tokens = line
            .split(|b| b.is_ascii_whitespace())
            .filter(|t| !t.is_empty());
        match tokens.next() {
            Some(b"v") => {
                let mut v = [0f32; 3];
                for c in &mut v {
                    *c = tokens
                        .next()
                        .and_then(parse_f32)
                        .ok_or_else(|| DecodeError::malformed("bad OBJ vertex"))?;
                }
                if verts.len() >= MAX_VERTICES {
                    return Err(DecodeError::TooLarge);
                }
                verts.push(v);
            }
            Some(b"f") => {
                face.clear();
                let mut valid = true;
                for token in tokens {
                    match corner(token, verts.len()) {
                        Some(i) => {
                            if face.len() > MAX_TRIANGLES {
                                return Err(DecodeError::TooLarge);
                            }
                            face.push(i);
                        }
                        None => {
                            valid = false;
                            break;
                        }
                    }
                }
                if !valid || face.len() < 3 {
                    continue;
                }
                if tris.len() + face.len() - 2 > MAX_TRIANGLES {
                    return Err(DecodeError::TooLarge);
                }
                for w in face[1..].windows(2) {
                    tris.push([face[0], w[0], w[1]]);
                }
            }
            _ => {}
        }
    }
    // Positive indices may legally name vertices defined after the face.
    tris.retain(|t| t.iter().all(|&i| (i as usize) < verts.len()));
    if tris.is_empty() {
        return Err(DecodeError::malformed("OBJ has no faces"));
    }
    Ok(Mesh::Indexed { verts, tris })
}

/// Zero-based vertex index of an `i[/...]` face corner; `None` for 0 or an unusable token.
fn corner(token: &[u8], defined: usize) -> Option<u32> {
    let index = token.split(|&b| b == b'/').next()?;
    let v: i64 = std::str::from_utf8(index).ok()?.parse().ok()?;
    let zero_based = match v {
        0 => return None,
        1.. => v - 1,
        _ => i64::try_from(defined).ok()?.checked_add(v)?,
    };
    u32::try_from(zero_based).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn dl() -> Deadline {
        Deadline::after(Duration::from_secs(30))
    }

    #[test]
    fn quads_negative_indices_and_extra_statements() {
        let text = "# cube face\nmtllib x.mtl\nv 0 0 0\nv 1 0 0\nv 1 1 0.5\nv 0 1 0\nvn 0 0 1\nvt 0 0\n\
                    usemtl m\ng a\nf 1/1/1 2/1/1 3/1/1 4/1/1\nf -4//1 -3//1 -2//1\r\nl 1 2\nf 1 2\n";
        let mesh = parse(text.as_bytes(), &dl()).unwrap();
        // Quad -> 2 triangles, plus one triangle; the 2-corner face is dropped.
        assert_eq!(mesh.triangle_count(), 3);
        assert_eq!(mesh.triangle(1).unwrap()[2], [0.0, 1.0, 0.0]);
        assert_eq!(mesh.triangle(2).unwrap()[1], [1.0, 0.0, 0.0]);
    }

    #[test]
    fn forward_references_are_kept_and_out_of_range_dropped() {
        let text = "f 1 2 3\nf 1 2 9\nf -9 1 2\nv 0 0 0\nv 1 0 0\nv 0 1 0\n";
        let mesh = parse(text.as_bytes(), &dl()).unwrap();
        assert_eq!(mesh.triangle_count(), 1);
    }

    #[test]
    fn hostile_obj() {
        assert!(matches!(parse(b"", &dl()), Err(DecodeError::Malformed(_))));
        assert!(matches!(
            parse(b"v 1 2\n", &dl()),
            Err(DecodeError::Malformed(_))
        ));
        assert!(matches!(
            parse(b"v 1 2 x\n", &dl()),
            Err(DecodeError::Malformed(_))
        ));
        // Zero, huge and garbage indices.
        let text =
            "v 0 0 0\nv 1 0 0\nv 0 1 0\nf 0 1 2\nf 99999999999 1 2\nf a b c\nf -99999999999 1 2\n";
        assert!(matches!(
            parse(text.as_bytes(), &dl()),
            Err(DecodeError::Malformed(_))
        ));
        // Binary junk never panics.
        let junk: Vec<u8> = (0..4096u32).map(|i| (i * 31 % 256) as u8).collect();
        assert!(parse(&junk, &dl()).is_err());
    }

    #[test]
    fn one_giant_polygon_hits_the_triangle_cap() {
        let mut text = String::from("v 0 0 0\nv 1 0 0\nv 0 1 0\nf");
        for _ in 0..MAX_TRIANGLES + 5 {
            text.push_str(" 1");
        }
        assert_eq!(
            parse(text.as_bytes(), &dl()).err(),
            Some(DecodeError::TooLarge)
        );
    }

    #[test]
    fn expired_deadline() {
        let expired = Deadline::after(Duration::ZERO);
        assert_eq!(
            parse(b"v 0 0 0\n", &expired).err(),
            Some(DecodeError::Timeout)
        );
    }
}
