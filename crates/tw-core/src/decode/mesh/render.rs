//! Software rasteriser: orthographic fixed-angle view, z-buffer, flat Lambert shading,
//! 2x2 supersampling, fixed 512x512 output on a neutral background.

use image::{DynamicImage, RgbaImage};

use super::{Deadline, DecodeError, Mesh, Result};

/// Output edge in pixels.
pub const SIZE: usize = 512;
const SS: usize = 2;
const N: usize = SIZE * SS;
/// Fraction of the frame the model's longer screen extent fills.
const FILL: f64 = 0.88;
const BACKGROUND: [u8; 3] = [236, 238, 241];
const BASE: [f64; 3] = [118.0, 150.0, 196.0];
/// Pixel tests between deadline checks.
const CHECK_WORK: u64 = 1 << 21;

type V3 = [f64; 3];

fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: V3, b: V3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: V3, b: V3) -> V3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn normalize(a: V3) -> V3 {
    let len = dot(a, a).sqrt();
    [a[0] / len, a[1] / len, a[2] / len]
}

/// Camera basis for a Z-up world: looking from the front-right-above.
struct Camera {
    right: V3,
    up: V3,
    forward: V3,
    light: V3,
}

impl Camera {
    fn new() -> Self {
        let forward = normalize([-0.6, 0.7, -0.55]);
        let right = normalize(cross(forward, [0.0, 0.0, 1.0]));
        let up = cross(right, forward);
        // Headlight leaning towards the upper left so faces differ in tone.
        let light = normalize([
            -forward[0] - 0.5 * right[0] + 0.7 * up[0],
            -forward[1] - 0.5 * right[1] + 0.7 * up[1],
            -forward[2] - 0.5 * right[2] + 0.7 * up[2],
        ]);
        Self {
            right,
            up,
            forward,
            light,
        }
    }
}

/// A triangle in world space (Z-up, finite), or `None` if any coordinate is not finite.
fn world(tri: [[f32; 3]; 3], y_up: bool) -> Option<[V3; 3]> {
    let conv = |p: [f32; 3]| -> Option<V3> {
        let [x, y, z] = p.map(f64::from);
        if !(x.is_finite() && y.is_finite() && z.is_finite()) {
            return None;
        }
        Some(if y_up { [x, -z, y] } else { [x, y, z] })
    };
    Some([conv(tri[0])?, conv(tri[1])?, conv(tri[2])?])
}

/// Screen-space vertex: x, y in samples, depth along the view direction.
#[derive(Clone, Copy)]
struct P {
    x: f64,
    y: f64,
    z: f64,
}

struct Frame {
    cam: Camera,
    cx: f64,
    cy: f64,
    scale: f64,
}

impl Frame {
    fn project(&self, p: V3) -> P {
        let half = N as f64 / 2.0;
        P {
            x: (dot(p, self.cam.right) - self.cx) * self.scale + half,
            y: half - (dot(p, self.cam.up) - self.cy) * self.scale,
            z: dot(p, self.cam.forward),
        }
    }
}

struct Work<'a> {
    done: u64,
    deadline: &'a Deadline,
}

impl Work<'_> {
    fn add(&mut self, units: u64) -> Result<()> {
        self.done += units;
        if self.done >= CHECK_WORK {
            self.done = 0;
            self.deadline.check()?;
        }
        Ok(())
    }
}

pub fn render(mesh: &Mesh, y_up: bool, deadline: &Deadline) -> Result<DynamicImage> {
    let cam = Camera::new();
    let mut work = Work { done: 0, deadline };

    // Pass 1: screen bounding box of the visible geometry.
    let (mut min_x, mut max_x, mut min_y, mut max_y) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    let mut valid = 0usize;
    for i in 0..mesh.triangle_count() {
        work.add(16)?;
        let Some(tri) = mesh.triangle(i).and_then(|t| world(t, y_up)) else {
            continue;
        };
        valid += 1;
        for p in tri {
            let (x, y) = (dot(p, cam.right), dot(p, cam.up));
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);
        }
    }
    let span = (max_x - min_x).max(max_y - min_y);
    if valid == 0 || !span.is_finite() {
        return Err(DecodeError::malformed("mesh has no finite triangles"));
    }
    // A degenerate (single point) model still renders: give it unit extent.
    let span = if span > 0.0 { span } else { 1.0 };
    let frame = Frame {
        cx: (min_x + max_x) / 2.0,
        cy: (min_y + max_y) / 2.0,
        scale: N as f64 * FILL / span,
        cam,
    };

    // Pass 2: z-buffered flat-shaded triangles.
    let mut depth = vec![f32::INFINITY; N * N];
    let mut color = vec![BACKGROUND; N * N];
    for i in 0..mesh.triangle_count() {
        work.add(16)?;
        let Some(tri) = mesh.triangle(i).and_then(|t| world(t, y_up)) else {
            continue;
        };
        let normal = cross(sub(tri[1], tri[0]), sub(tri[2], tri[0]));
        if dot(normal, normal) == 0.0 || !dot(normal, normal).is_finite() {
            continue;
        }
        let n = normalize(normal);
        let lambert = dot(n, frame.cam.light).abs();
        let shade = 0.28 + 0.72 * lambert;
        let rgb = BASE.map(|c| (c * shade).clamp(0.0, 255.0) as u8);
        let p = tri.map(|v| frame.project(v));
        fill(&p, rgb, &mut depth, &mut color, &mut work)?;
    }

    // Box-filter the supersamples; uncovered samples contribute the background.
    let mut out = Vec::with_capacity(SIZE * SIZE * 4);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let mut sum = [0u32; 3];
            for dy in 0..SS {
                for dx in 0..SS {
                    let px = color[(y * SS + dy) * N + x * SS + dx];
                    for c in 0..3 {
                        sum[c] += u32::from(px[c]);
                    }
                }
            }
            let samples = (SS * SS) as u32;
            out.extend(sum.map(|s| ((s + samples / 2) / samples) as u8));
            out.push(255);
        }
    }
    RgbaImage::from_raw(SIZE as u32, SIZE as u32, out)
        .map(DynamicImage::ImageRgba8)
        .ok_or_else(|| DecodeError::malformed("render buffer"))
}

fn fill(
    p: &[P; 3],
    rgb: [u8; 3],
    depth: &mut [f32],
    color: &mut [[u8; 3]],
    work: &mut Work,
) -> Result<()> {
    let area = (p[1].x - p[0].x) * (p[2].y - p[0].y) - (p[1].y - p[0].y) * (p[2].x - p[0].x);
    if !area.is_finite() || area.abs() < 1e-9 {
        return Ok(());
    }
    let lo = |a: f64, b: f64, c: f64| a.min(b).min(c);
    let hi = |a: f64, b: f64, c: f64| a.max(b).max(c);
    // `as` saturates, so absurd coordinates clamp instead of wrapping.
    let x0 = (lo(p[0].x, p[1].x, p[2].x).floor().max(0.0) as usize).min(N - 1);
    let x1 = (hi(p[0].x, p[1].x, p[2].x).ceil().max(0.0) as usize).min(N - 1);
    let y0 = (lo(p[0].y, p[1].y, p[2].y).floor().max(0.0) as usize).min(N - 1);
    let y1 = (hi(p[0].y, p[1].y, p[2].y).ceil().max(0.0) as usize).min(N - 1);
    if lo(p[0].x, p[1].x, p[2].x) >= N as f64
        || hi(p[0].x, p[1].x, p[2].x) < 0.0
        || lo(p[0].y, p[1].y, p[2].y) >= N as f64
        || hi(p[0].y, p[1].y, p[2].y) < 0.0
    {
        return Ok(());
    }
    work.add(((x1 - x0 + 1) * (y1 - y0 + 1)) as u64)?;
    let inv = 1.0 / area;
    // Tolerance keeps shared edges free of cracks.
    let eps = -1e-9;
    for y in y0..=y1 {
        let py = y as f64 + 0.5;
        for x in x0..=x1 {
            let px = x as f64 + 0.5;
            let w0 = ((p[1].x - px) * (p[2].y - py) - (p[1].y - py) * (p[2].x - px)) * inv;
            let w1 = ((p[2].x - px) * (p[0].y - py) - (p[2].y - py) * (p[0].x - px)) * inv;
            let w2 = 1.0 - w0 - w1;
            if w0 < eps || w1 < eps || w2 < eps {
                continue;
            }
            let z = (w0 * p[0].z + w1 * p[1].z + w2 * p[2].z) as f32;
            let at = y * N + x;
            if z < depth[at] {
                depth[at] = z;
                color[at] = rgb;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::stl::fixtures::{binary, cube};
    use super::super::*;
    use super::*;
    use std::time::Duration;

    fn render_stl(tris: &[[[f32; 3]; 3]]) -> RgbaImage {
        decode(&binary(tris), "stl").unwrap().to_rgba8()
    }

    fn px(img: &RgbaImage, x: u32, y: u32) -> [u8; 3] {
        let p = img.get_pixel(x, y).0;
        [p[0], p[1], p[2]]
    }

    fn coverage(img: &RgbaImage) -> usize {
        img.pixels()
            .filter(|p| [p[0], p[1], p[2]] != BACKGROUND)
            .count()
    }

    #[test]
    fn cube_renders_centered_and_deterministic() {
        let img = render_stl(&cube(0.0, 10.0));
        assert_eq!(img.dimensions(), (512, 512));
        assert_ne!(px(&img, 256, 256), BACKGROUND);
        assert_eq!(px(&img, 2, 2), BACKGROUND);
        assert_eq!(px(&img, 509, 509), BACKGROUND);
        // The cube covers a substantial, but not total, part of the frame.
        let c = coverage(&img);
        assert!(c > 512 * 512 / 8 && c < 512 * 512 * 3 / 4, "coverage {c}");
        assert_eq!(render_stl(&cube(0.0, 10.0)), img);
        // Scale and position independent up to edge rounding: same centre tone and
        // coverage within half a percent.
        let big = render_stl(&cube(-500.0, 500.0));
        assert_eq!(px(&big, 256, 256), px(&img, 256, 256));
        assert!(coverage(&big).abs_diff(c) * 200 < c);
    }

    #[test]
    fn visible_faces_have_distinct_tones() {
        let img = render_stl(&cube(0.0, 1.0));
        let mut counts = std::collections::HashMap::new();
        for p in img.pixels() {
            *counts.entry([p[0], p[1], p[2]]).or_insert(0usize) += 1;
        }
        counts.remove(&BACKGROUND);
        // Three visible faces, each a large flat-shaded region of its own tone.
        let large = counts.values().filter(|&&n| n > 2000).count();
        assert_eq!(large, 3, "{counts:?}");
    }

    #[test]
    fn nearer_triangle_wins_regardless_of_order() {
        let front = [[-1.0, -1.0, 0.0], [1.0, -1.0, 0.0], [0.0, 1.0, 0.0]];
        // Same triangle moved towards the camera (+x, -y, +z of the eye direction).
        let back = front.map(|p: [f32; 3]| [p[0] - 3.0, p[1] + 4.0, p[2] - 3.0]);
        let a = render_stl(&[front, back]);
        let b = render_stl(&[back, front]);
        assert_eq!(a, b);
    }

    #[test]
    fn obj_is_y_up_and_ply_is_z_up() {
        let obj = "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nv 0 0 1\nv 1 0 1\nv 1 1 1\nv 0 1 1\n\
                   f 1 2 3 4\nf 5 6 7 8\nf 1 2 6 5\nf 3 4 8 7\nf 1 4 8 5\nf 2 3 7 6\n";
        let img = decode(obj.as_bytes(), "obj").unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (512, 512));
        assert_ne!(px(&img, 256, 256), BACKGROUND);
        assert_eq!(px(&img, 2, 2), BACKGROUND);

        let ply = super::super::ply::fixtures::tetra(
            "binary_little_endian",
            &super::super::ply::fixtures::V,
            &super::super::ply::fixtures::F,
        );
        let img = decode(&ply, "ply").unwrap().to_rgba8();
        assert_ne!(px(&img, 256, 256), BACKGROUND);
        assert!(coverage(&img) > 1000);
    }

    #[test]
    fn degenerate_inputs() {
        // Collinear and NaN triangles only: nothing to draw.
        let line = [[0.0, 0.0, 0.0], [1.0, 1.0, 1.0], [2.0, 2.0, 2.0]];
        let nan = [[f32::NAN, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let inf = [[f32::INFINITY, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let img = render_stl(&[line, nan, inf]);
        assert_eq!(coverage(&img), 0);
        assert!(decode(&binary(&[nan, inf]), "stl").is_err());
        // Huge coordinates still render without panicking or overflowing.
        let big = [[-3e38, -3e38, 0.0], [3e38, -3e38, 0.0], [0.0, 3e38, 3e38]];
        let img = render_stl(&[big]);
        assert!(coverage(&img) > 0);
        // A single point cloud collapsed to one location has no area.
        let point = [[1.0, 1.0, 1.0]; 3];
        assert_eq!(coverage(&render_stl(&[point])), 0);
        assert_eq!(
            decode(b"x", "stl").err().map(|e| e.falls_through()),
            Some(true)
        );
        assert_eq!(decode(b"", "xyz").err(), Some(DecodeError::Unsupported));
    }

    #[test]
    fn expired_deadline_aborts_the_raster_loop() {
        let expired = Deadline::after(Duration::ZERO);
        // The pass-1 work counter crosses its check interval and observes the deadline.
        let big = Mesh::Flat(
            std::iter::repeat_n(
                [[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 1.0]],
                200_000,
            )
            .flatten()
            .collect(),
        );
        assert_eq!(
            render(&big, false, &expired).err(),
            Some(DecodeError::Timeout)
        );
    }

    #[test]
    fn many_tiny_triangles_finish_quickly() {
        // 1M disjoint tiny triangles: parse-free path through mesh.triangle().
        let mut corners = Vec::new();
        for i in 0..1_000_000u32 {
            let (x, y) = ((i % 1000) as f32, (i / 1000) as f32);
            corners.extend([[x, y, 0.0], [x + 0.5, y, 0.0], [x, y + 0.5, 0.0]]);
        }
        let start = std::time::Instant::now();
        let img = render(
            &Mesh::Flat(corners),
            false,
            &Deadline::after(Duration::from_secs(60)),
        )
        .unwrap();
        assert_eq!(img.width(), 512);
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "{:?}",
            start.elapsed()
        );
    }
}
