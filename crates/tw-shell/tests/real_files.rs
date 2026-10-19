//! Real application files (see `crates/tw-core/tests/fixtures/README.md` for how each was
//! made) decoded through the same `Engine` the shell extension uses, so the decoders that
//! run out of process do so in the real `tw-worker.exe`: `open-heic` for the HEIC still
//! (alone and inside a `.livp`), Media Foundation for a BDAV `.m2ts`.
#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tw_core::decode::Thumbnail;
use tw_core::decode::route::Decoder;
use tw_core::pipeline::{Engine, Trace};

fn worker() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tw-worker"))
}

fn fixture(rel: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tw-core/tests/fixtures")
        .join(rel);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn thumbnail(rel: &str, ext: &str, video_percent: u8) -> Result<Thumbnail, String> {
    traced(fixture(rel), rel, ext, video_percent).map(|(t, _)| t)
}

/// [`thumbnail`] of `data` (named `what` in errors), also naming the tier that produced it.
fn traced(
    data: Vec<u8>,
    what: &str,
    ext: &str,
    video_percent: u8,
) -> Result<(Thumbnail, Trace), String> {
    Engine::new(worker())
        .with_video_percent(video_percent)
        .thumbnail_traced(Arc::new(data), Some(ext), 256)
        .map_err(|e| format!("{what}: {e:?}"))
}

/// RGB of the pixel at `(x, y)` (the thumbnail is top-down BGRA, opaque here).
fn rgb(t: &Thumbnail, x: u32, y: u32) -> [u8; 3] {
    let at = ((y * t.width + x) * 4) as usize;
    assert_eq!(t.bgra[at + 3], 255, "({x},{y}) is not opaque");
    [t.bgra[at + 2], t.bgra[at + 1], t.bgra[at]]
}

/// A mostly-saturated primary: `high` channels at least 200, the others at most 70. Chroma
/// matrices differ between decoders by tens of levels (the fixtures carry no colour
/// description for WIC to honour), so exact values are not asserted.
fn assert_primary(got: [u8; 3], high: [bool; 3], what: &str) {
    let ok = got
        .iter()
        .zip(high)
        .all(|(c, h)| if h { *c >= 200 } else { *c <= 70 });
    assert!(ok, "{what}: {got:?}");
}

/// The HEIC still, decoded in the worker by `open-heic`: no OS codec and no Store extension
/// is involved, so this must succeed on every machine.
fn heic_still() -> Thumbnail {
    let (t, trace) = traced(fixture("heic/quad.heic"), "quad.heic", "heic", 30).unwrap();
    assert_eq!(trace.decoder, Decoder::Heif, "{trace}");
    assert_eq!(trace.attempt, 1, "{trace}");
    assert!(
        trace.worker_pid.is_some(),
        "HEIF runs in the worker: {trace}"
    );
    t
}

/// Four flat quadrants: red, green / blue, yellow.
fn assert_quadrants(t: &Thumbnail, what: &str) {
    assert_eq!((t.width, t.height), (160, 120), "{what}");
    assert_primary(rgb(t, 20, 20), [true, false, false], what);
    assert_primary(rgb(t, 140, 20), [false, true, false], what);
    assert_primary(rgb(t, 20, 100), [false, false, true], what);
    assert_primary(rgb(t, 140, 100), [true, true, false], what);
}

#[test]
fn heic_still_decodes_through_the_worker() {
    assert_quadrants(&heic_still(), "quad.heic");
}

/// A JPEG under a `.heic` name is not a HEIF file: `open-heic` declines it, and the
/// in-process raster tier takes it, still without any OS codec.
#[test]
fn a_jpeg_named_heic_still_decodes() {
    let mut jpeg = std::io::Cursor::new(Vec::new());
    image::RgbImage::from_pixel(64, 48, image::Rgb([220, 20, 20]))
        .write_to(&mut jpeg, image::ImageFormat::Jpeg)
        .unwrap();
    let (t, trace) = traced(jpeg.into_inner(), "mislabelled", "heic", 30).unwrap();
    assert_eq!(trace.decoder, Decoder::Raster, "{trace}");
    assert_eq!((t.width, t.height), (64, 48));
    assert_primary(rgb(&t, 32, 24), [true, false, false], "mislabelled jpeg");
}

/// The `.livp` holds `IMG_0001.HEIC` (the same still) and `IMG_0001.MOV`; the thumbnail must
/// be the still, not a frame of the movie (which is a 320x180 red/green/blue sequence).
#[test]
fn livp_thumbnail_is_the_heic_still() {
    let t = thumbnail("livp/live-photo.livp", "livp", 30).unwrap();
    assert_quadrants(&t, "live-photo.livp");
}

/// The owner's own iPhone files (not committed; set `TW_HEIC_LOCAL` to the directory that
/// holds them): `classic-car.heic` has an embedded thumbnail and `irot` 3 (portrait once
/// rotated), `chef-with-trumpet.heic` is a tiled grid without one, and `IMG_0074.heic` is
/// JPEG data under a `.heic` name. Run with
/// `TW_HEIC_LOCAL=C:\wt\heicspike\in cargo test -p tw-shell --test real_files --release -- --ignored`.
#[test]
#[ignore = "needs the owner's HEIC files in TW_HEIC_LOCAL"]
fn local_iphone_files_decode_without_os_codecs() {
    use tw_core::decode::{heif, to_thumbnail};

    let dir = PathBuf::from(std::env::var("TW_HEIC_LOCAL").expect("TW_HEIC_LOCAL"));
    let read = |name: &str| std::fs::read(dir.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));

    // Embedded thumbnail, upright: the worker's thumbnail is portrait and shows the same
    // picture the same way up as the full image shrunk to the same size.
    let car = read("classic-car.heic");
    let started = std::time::Instant::now();
    let (t, trace) = traced(car.clone(), "classic-car.heic", "heic", 30).unwrap();
    eprintln!(
        "classic-car.heic: {trace}, {}x{}, {:?}",
        t.width,
        t.height,
        started.elapsed()
    );
    assert_eq!(trace.decoder, Decoder::Heif, "{trace}");
    assert!(t.height > t.width, "not portrait: {}x{}", t.width, t.height);
    assert_eq!(t.width.max(t.height), 256);
    let full = to_thumbnail(heif::decode(&car, u32::MAX).unwrap(), 256).unwrap();
    assert_eq!((full.width, full.height), (t.width, t.height));
    let diff: u64 = t
        .bgra
        .iter()
        .zip(&full.bgra)
        .map(|(a, b)| u64::from(a.abs_diff(*b)))
        .sum();
    let mean = diff as f64 / t.bgra.len() as f64;
    assert!(
        mean < 20.0,
        "thumbnail and full image disagree (mean {mean:.1}): rotated differently?"
    );

    // Grid without a thumbnail: the primary image is decoded.
    let chef = read("chef-with-trumpet.heic");
    let (t, trace) = traced(chef, "chef-with-trumpet.heic", "heic", 30).unwrap();
    assert_eq!(trace.decoder, Decoder::Heif, "{trace}");
    assert_eq!(t.width.max(t.height), 256);

    // JPEG bytes named .heic: not HEIF, decoded as a raster image.
    let jpeg = read("IMG_0074.heic");
    let (t, trace) = traced(jpeg, "IMG_0074.heic", "heic", 30).unwrap();
    assert_eq!(trace.decoder, Decoder::Raster, "{trace}");
    assert_eq!(t.width.max(t.height), 256);
}

/// A BDAV `.m2ts` (192-byte source packets): one second each of red, green (0, 128, 0) and
/// blue at 1280x720. Media Foundation seeks a transport stream by estimated byte position,
/// so the offsets below sit well inside each second rather than at the boundaries.
#[test]
fn m2ts_bdav_frame_comes_from_the_requested_offset() {
    let at = |percent: u8| {
        let t = thumbnail("m2ts/bdav-192.m2ts", "m2ts", percent).unwrap();
        assert_eq!((t.width, t.height), (256, 144), "percent {percent}");
        rgb(&t, 128, 72)
    };
    let (first, middle, last) = (at(0), at(40), at(75));
    assert!(
        first[0] >= 200 && first[1] <= 40 && first[2] <= 40,
        "0%: {first:?}"
    );
    assert!(
        middle[1] >= 100 && middle[0] <= 40 && middle[2] <= 40,
        "40%: {middle:?}"
    );
    assert!(
        last[2] >= 200 && last[0] <= 40 && last[1] <= 40,
        "75%: {last:?}"
    );
}
