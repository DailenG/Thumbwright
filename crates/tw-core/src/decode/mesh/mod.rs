//! 3D meshes (`.stl`, `.obj`, `.ply`) shaded by a small software rasteriser.
//!
//! The model is parsed into triangles, fitted into a fixed 512x512 view (fixed rotation,
//! orthographic, z-buffer, flat Lambert shading on a neutral background), so the result
//! depends on the file only, never on the requested thumbnail size.
//!
//! The decoder is memory-safe and bounded, so it runs in-process: [`MAX_TRIANGLES`] and
//! [`MAX_VERTICES`] cap what is retained, declared counts are checked against the bytes
//! actually present before anything proportional to them is allocated, and every parse
//! and raster loop checks a [`Deadline`].

pub mod obj;
pub mod ply;
pub mod render;
pub mod stl;

use std::time::{Duration, Instant};

use image::DynamicImage;

use super::{DecodeError, Result};

pub const EXTENSIONS: &[&str] = &["stl", "obj", "ply"];

/// Triangles retained per model.
pub const MAX_TRIANGLES: usize = 5_000_000;
/// Vertices retained per model.
pub const MAX_VERTICES: usize = 5_000_000;
/// Wall-clock budget for parsing plus rendering (the in-process tier budget is 15 s).
const BUDGET: Duration = Duration::from_secs(6);

/// A point in time after which work is abandoned with [`DecodeError::Timeout`].
pub struct Deadline(Instant);

impl Deadline {
    pub fn after(budget: Duration) -> Self {
        Self(Instant::now() + budget)
    }

    pub fn check(&self) -> Result<()> {
        if Instant::now() >= self.0 {
            Err(DecodeError::Timeout)
        } else {
            Ok(())
        }
    }
}

/// Parsed triangles.
pub enum Mesh<'a> {
    /// The record array of a binary STL (50 bytes per triangle), read in place.
    StlBinary(&'a [u8]),
    /// Three consecutive corners per triangle.
    Flat(Vec<[f32; 3]>),
    /// Shared vertices plus index triples, every index in range.
    Indexed {
        verts: Vec<[f32; 3]>,
        tris: Vec<[u32; 3]>,
    },
}

impl Mesh<'_> {
    pub fn triangle_count(&self) -> usize {
        match self {
            Self::StlBinary(records) => records.len() / 50,
            Self::Flat(corners) => corners.len() / 3,
            Self::Indexed { tris, .. } => tris.len(),
        }
    }

    /// Corners of triangle `i`, `None` when out of range.
    pub fn triangle(&self, i: usize) -> Option<[[f32; 3]; 3]> {
        match self {
            Self::StlBinary(records) => {
                let rec = records.get(i.checked_mul(50)?..)?.get(12..48)?;
                let f = |k: usize| f32::from_le_bytes([rec[k], rec[k + 1], rec[k + 2], rec[k + 3]]);
                let v = |c: usize| [f(c * 12), f(c * 12 + 4), f(c * 12 + 8)];
                Some([v(0), v(1), v(2)])
            }
            Self::Flat(corners) => {
                let base = i.checked_mul(3)?;
                Some([
                    *corners.get(base)?,
                    *corners.get(base + 1)?,
                    *corners.get(base + 2)?,
                ])
            }
            Self::Indexed { verts, tris } => {
                let [a, b, c] = *tris.get(i)?;
                Some([
                    *verts.get(a as usize)?,
                    *verts.get(b as usize)?,
                    *verts.get(c as usize)?,
                ])
            }
        }
    }
}

pub fn decode(data: &[u8], ext: &str) -> Result<DynamicImage> {
    let deadline = Deadline::after(BUDGET);
    let (mesh, y_up) = match ext.to_ascii_lowercase().as_str() {
        "stl" => (stl::parse(data, &deadline)?, false),
        // Wavefront files are Y-up by convention; STL and PLY are overwhelmingly Z-up.
        "obj" => (obj::parse(data, &deadline)?, true),
        "ply" => (ply::parse(data, &deadline)?, false),
        _ => return Err(DecodeError::Unsupported),
    };
    render::render(&mesh, y_up, &deadline)
}

/// Parses an ASCII float token.
pub(crate) fn parse_f32(token: &[u8]) -> Option<f32> {
    std::str::from_utf8(token).ok()?.parse().ok()
}
