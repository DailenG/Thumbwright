//! Verification against real files (see `tests/fixtures/README.md` for sources and
//! licences).
//!
//! * JPEG XL: libjxl conformance streams, compared pixel by pixel with the reference
//!   PNG the suite ships for each (committed under `tests/fixtures/jxl`).
//! * RAW: the embedded JPEG is located by offset (found by scanning the files for JPEG
//!   start-of-image markers, independently of the parsers under test), decoded with the
//!   `image` crate and compared with what `raw::extract` returns. One small CC0 CRW is
//!   committed; the larger set is read from `TW_SAMPLES` by an ignored test that
//!   `scripts/fetch-samples.ps1` prepares.
//! * `.vips`, `.blend`, `.pdn`: files written by libvips, Blender and Paint.NET themselves
//!   (committed under `tests/fixtures/{vips,blend,pdn}`; provenance in the README), checked
//!   for dimensions and known pixels. `.livp` and `.m2ts` need OS codecs and are covered
//!   through the worker in `tw-shell/tests/real_files.rs`.

use std::path::{Path, PathBuf};

use image::metadata::Orientation;
use image::{DynamicImage, ImageFormat};
use tw_core::decode::{jxl, raw, route};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Largest per-channel difference between two images of the same size, comparing
/// colour weighted by alpha (colour under transparent pixels is meaningless).
fn max_diff(got: &DynamicImage, reference: &DynamicImage) -> u32 {
    assert_eq!(
        (got.width(), got.height()),
        (reference.width(), reference.height()),
        "dimensions differ"
    );
    let (a, b) = (got.to_rgba8(), reference.to_rgba8());
    let mut worst = 0u32;
    for (p, q) in a.pixels().zip(b.pixels()) {
        let (pa, qa) = (u32::from(p.0[3]), u32::from(q.0[3]));
        worst = worst.max(pa.abs_diff(qa));
        for c in 0..3 {
            let (x, y) = (u32::from(p.0[c]) * pa / 255, u32::from(q.0[c]) * qa / 255);
            worst = worst.max(x.abs_diff(y));
        }
    }
    worst
}

struct JxlCase {
    name: &'static str,
    width: u32,
    height: u32,
    alpha: bool,
    /// 8-bit rounding differences against the reference decode allowed.
    tolerance: u32,
}

const JXL_CASES: &[JxlCase] = &[
    // Modular XYB, 8-bit.
    JxlCase {
        name: "bicycles",
        width: 1024,
        height: 631,
        alpha: false,
        tolerance: 1,
    },
    // Orientation 7 (transpose) and two layers: stored 1386x924, shown 924x1386.
    JxlCase {
        name: "sunset_logo",
        width: 924,
        height: 1386,
        alpha: true,
        tolerance: 1,
    },
    // Container, JPEG reconstruction data, orientation 5, non-enumerable ICC profile.
    JxlCase {
        name: "bench_oriented_brg",
        width: 606,
        height: 500,
        alpha: false,
        tolerance: 1,
    },
    // Associated (premultiplied) alpha must come out as straight alpha.
    JxlCase {
        name: "alpha_premultiplied",
        width: 1024,
        height: 1024,
        alpha: true,
        tolerance: 1,
    },
    JxlCase {
        name: "alpha_nonpremultiplied",
        width: 1024,
        height: 1024,
        alpha: true,
        tolerance: 1,
    },
    JxlCase {
        name: "patches",
        width: 1600,
        height: 1096,
        alpha: true,
        tolerance: 2,
    },
];

#[test]
fn jxl_matches_conformance_reference_decodes() {
    for case in JXL_CASES {
        let dir = fixtures().join("jxl");
        let data = std::fs::read(dir.join(format!("{}.jxl", case.name))).unwrap();
        let reference = image::open(dir.join(format!("{}.ref.png", case.name))).unwrap();
        let got = jxl::decode(&data).unwrap_or_else(|e| panic!("{}: {e}", case.name));
        assert_eq!(
            (got.width(), got.height()),
            (case.width, case.height),
            "{}",
            case.name
        );
        assert_eq!(got.color().has_alpha(), case.alpha, "{}", case.name);
        let worst = max_diff(&got, &reference);
        assert!(
            worst <= case.tolerance,
            "{}: max difference {worst} against the reference exceeds {}",
            case.name,
            case.tolerance
        );
    }
}

#[test]
fn jxl_grayscale_stays_grayscale() {
    let dir = fixtures().join("jxl");
    let data = std::fs::read(dir.join("grayscale.jxl")).unwrap();
    let reference = image::open(dir.join("grayscale.ref.png")).unwrap();
    let got = jxl::decode(&data).unwrap();
    assert!(matches!(got, DynamicImage::ImageLuma8(_)));
    assert_eq!((got.width(), got.height()), (200, 200));
    assert!(max_diff(&got, &reference) <= 1);
}

/// What `raw::extract` must return for a sample: the JPEG at `jpeg_at`, decoded
/// independently, with `orientation` (EXIF code) applied.
struct Sample {
    file: &'static str,
    /// Final dimensions, after orientation.
    width: u32,
    height: u32,
    jpeg_at: usize,
    orientation: u8,
    /// Minolta maker-note previews store a stray byte in place of the leading 0xFF.
    patch_first_byte: bool,
}

const fn sample(
    file: &'static str,
    width: u32,
    height: u32,
    jpeg_at: usize,
    orientation: u8,
) -> Sample {
    Sample {
        file,
        width,
        height,
        jpeg_at,
        orientation,
        patch_first_byte: false,
    }
}

const fn minolta(file: &'static str, width: u32, height: u32, jpeg_at: usize) -> Sample {
    Sample {
        patch_first_byte: true,
        ..sample(file, width, height, jpeg_at, 1)
    }
}

fn expected_preview(data: &[u8], s: &Sample) -> DynamicImage {
    let patched;
    let bytes = if s.patch_first_byte {
        let mut copy = data[s.jpeg_at..data.len().min(s.jpeg_at + (1 << 20))].to_vec();
        copy[0] = 0xFF;
        patched = copy;
        &patched[..]
    } else {
        &data[s.jpeg_at..]
    };
    let mut img = image::load_from_memory_with_format(bytes, ImageFormat::Jpeg).unwrap();
    if let Some(o) = Orientation::from_exif(s.orientation) {
        img.apply_orientation(o);
    }
    img
}

fn check(data: &[u8], s: &Sample) {
    let got = raw::extract(data).unwrap_or_else(|e| panic!("{}: {e}", s.file));
    assert_eq!(
        (got.width(), got.height()),
        (s.width, s.height),
        "{}: dimensions",
        s.file
    );
    let want = expected_preview(data, s);
    assert_eq!(
        (want.width(), want.height()),
        (s.width, s.height),
        "{}: table dimensions",
        s.file
    );
    assert!(
        got.to_rgb8().as_raw() == want.to_rgb8().as_raw(),
        "{}: pixels differ from the embedded JPEG at {}",
        s.file,
        s.jpeg_at
    );
}

/// The only RAW small enough to commit: a Canon PowerShot G1 CIFF file (CRW) whose
/// JpgFromRaw record is a 640x480 JPEG.
#[test]
fn committed_crw_decodes_its_embedded_jpeg() {
    let data = std::fs::read(fixtures().join("raw/canon-powershot-g1.crw")).unwrap();
    check(
        &data,
        &sample("canon-powershot-g1.crw", 640, 480, 2_257_380, 1),
    );
}

const SAMPLES: &[Sample] = &[
    // Canon CR3 (ISO BMFF): PRVW box; the R10 file is shot in portrait (CMT1 orientation).
    sample("canon-eos-r6.cr3", 1620, 1080, 102_176, 1),
    sample("canon-eos-r6-mark-iii.cr3", 1620, 1080, 114_768, 1),
    sample("canon-eos-m50.cr3", 1620, 1080, 89_944, 1),
    sample("canon-eos-r10-rotated.cr3", 1080, 1620, 99_448, 8),
    // Canon CRW (CIFF): JpgFromRaw; the 300D file records a 270 degree rotation.
    sample("canon-eos-d30.crw", 1440, 960, 2_824_674, 1),
    sample("canon-eos-300d-rotated.crw", 1360, 2048, 6_929_664, 8),
    sample("canon-powershot-g5.crw", 640, 480, 4_861_832, 1),
    // Sigma X3F: IMA2 section directory (the SD Quattro file is the 4.x layout).
    sample("sigma-dp1.x3f", 2640, 1760, 260, 1),
    sample("sigma-sd15.x3f", 2640, 1760, 292, 1),
    sample("sigma-sd-quattro.x3f", 5424, 3616, 46_140_764, 1),
    // TIFF-based RAWs with maker notes.
    sample("canon-eos-20d.cr2", 1536, 1024, 9_728, 1),
    sample("canon-eos-40d-sraw.cr2", 1936, 1288, 42_426, 1),
    sample("canon-eos-kiss-digital-n-rotated.cr2", 1024, 1536, 9_728, 8),
    sample("nikon-d2h.nef", 570, 375, 8_256, 1),
    sample("nikon-d70s.nef", 3008, 2000, 131_072, 1),
    sample("nikon-d3.nef", 4256, 2832, 163_008, 1),
    sample("nikon-df-rotated.nef", 3280, 4928, 904_704, 6),
    sample("sony-a100.arw", 640, 480, 25_683, 1),
    sample("sony-ilce-7s.arw", 1616, 1080, 144_034, 1),
    sample("sony-a450-rotated.arw", 1080, 1616, 163_891, 6),
    sample("olympus-e10.orf", 160, 120, 6_424, 1),
    sample("olympus-e3.orf", 1600, 1200, 24_576, 1),
    sample("olympus-em5.orf", 3200, 2400, 52_224, 1),
    sample("panasonic-lx7.rw2", 1920, 1280, 1_536, 1),
    sample("panasonic-lx3.rw2", 1920, 1920, 1_536, 1),
    sample("panasonic-gh5.rw2", 1920, 1440, 4_608, 1),
    sample("pentax-k10d.pef", 3872, 2592, 8_212_260, 1),
    sample("fujifilm-s5000.raf", 1280, 960, 148, 1),
    sample("adobe-dng-converter-canon-5d3.dng", 3960, 2640, 259_634, 1),
    minolta("minolta-alpha-sweet-digital.mrw", 640, 480, 65_539),
];

/// `scripts/fetch-samples.ps1` downloads the files; then
/// `$env:TW_SAMPLES = (Resolve-Path target\samples).Path; cargo test -p tw-core --test samples -- --ignored`.
#[test]
#[ignore = "needs the downloaded sample set: set TW_SAMPLES (see scripts/fetch-samples.ps1)"]
fn real_raw_samples() {
    let dir = PathBuf::from(
        std::env::var_os("TW_SAMPLES").expect("TW_SAMPLES must name the sample directory"),
    );
    for s in SAMPLES {
        let path = dir.join(s.file);
        let data = std::fs::read(&path).unwrap_or_else(|e| {
            panic!(
                "{}: {e} (run scripts/fetch-samples.ps1 to download the sample set)",
                path.display()
            )
        });
        check(&data, s);
    }
}

/// Decodes a committed fixture the way the worker would: through the extension's routed
/// decoder chain (all stages in-process; none of these formats needs an OS codec).
fn decode_fixture(rel: &str) -> DynamicImage {
    let ext = Path::new(rel).extension().unwrap().to_str().unwrap();
    let data = std::fs::read(fixtures().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
    let chain = route::chain_for_ext(ext);
    assert!(!chain.is_empty(), "{ext} is not routed");
    route::run_chain(chain, &data, ext, 256).unwrap_or_else(|e| panic!("{rel}: {e:?}"))
}

fn assert_near(img: &DynamicImage, x: u32, y: u32, want: [u8; 4], tol: u8, what: &str) {
    let got = img.to_rgba8().get_pixel(x, y).0;
    assert!(
        got.iter().zip(want).all(|(g, w)| g.abs_diff(w) <= tol),
        "{what}: pixel ({x},{y}) is {got:?}, expected {want:?} +-{tol}"
    );
}

/// The 12x8 source image behind the `vips/` fixtures (`scripts/fixtures/make_vips_source.py`):
/// a gradient with three pixels set to known values.
fn vips_source_pixel(x: u32, y: u32) -> [u8; 3] {
    match (x, y) {
        (0, 0) => [255, 0, 0],
        (11, 7) => [0, 0, 255],
        (5, 3) => [10, 200, 30],
        _ => [(x * 20) as u8, (y * 30) as u8, 128],
    }
}

/// Real files written by `vips copy` (libvips 8.18.7). `rgb8` and `rgba8` are 8-bit sRGB
/// (interpretation 22), `rgb16` is RGB16 (25) with the values scaled by 256.
#[test]
fn vips_rgb_files_written_by_libvips() {
    for file in ["vips/rgb8.vips", "vips/rgb16.vips"] {
        let img = decode_fixture(file);
        assert_eq!((img.width(), img.height()), (12, 8), "{file}");
        for y in 0..8 {
            for x in 0..12 {
                let [r, g, b] = vips_source_pixel(x, y);
                assert_near(&img, x, y, [r, g, b, 255], 0, file);
            }
        }
    }
}

/// 4-band sRGB: band 3 (alpha) holds a copy of the red band.
#[test]
fn vips_rgba_file_keeps_alpha() {
    let img = decode_fixture("vips/rgba8.vips");
    assert_eq!((img.width(), img.height()), (12, 8));
    for y in 0..8 {
        for x in 0..12 {
            let [r, g, b] = vips_source_pixel(x, y);
            assert_near(&img, x, y, [r, g, b, r], 0, "rgba8");
        }
    }
}

/// `vips colourspace ... b-w` (type 1, 8 bit) and `... grey16` (type 26, 16 bit) of the same
/// source: the 16-bit file must decode to the same grey levels within rounding.
#[test]
fn vips_grey_files_written_by_libvips() {
    let g8 = decode_fixture("vips/bw8.vips").to_luma8();
    let g16 = decode_fixture("vips/grey16.vips").to_luma8();
    assert_eq!((g8.width(), g8.height()), (12, 8));
    assert_eq!((g16.width(), g16.height()), (12, 8));
    // Pure red is 127 in libvips' luminance weighting, (20, 0, 128) is 35.
    assert_eq!(g8.get_pixel(0, 0).0, [127]);
    assert_eq!(g8.get_pixel(1, 0).0, [35]);
    for (a, b) in g8.pixels().zip(g16.pixels()) {
        assert!(a.0[0].abs_diff(b.0[0]) <= 1, "{a:?} vs {b:?}");
    }
}

/// Saved by Blender 5.2.2 from a scene camera that frames a red cube with a grey cone
/// above-left (`scripts/fixtures/make_blend.py`): a 128x128 thumbnail with a transparent
/// background, stored bottom row first.
fn check_blend_thumbnail(img: &DynamicImage) {
    assert_eq!((img.width(), img.height()), (128, 128));
    // Cube face, shaded red.
    assert_near(img, 64, 64, [176, 54, 54, 255], 4, "cube");
    assert_near(img, 64, 80, [175, 46, 46, 255], 4, "cube");
    // The cone is grey and in the top-left corner: a vertical flip would put it at the
    // bottom.
    assert_near(img, 20, 5, [149, 151, 152, 255], 4, "cone");
    assert_near(img, 20, 122, [0, 0, 0, 0], 0, "empty bottom-left");
    // Transparent elsewhere.
    for (x, y) in [(0, 0), (127, 127), (110, 10), (10, 60), (100, 100)] {
        assert_eq!(img.to_rgba8().get_pixel(x, y).0[3], 0, "({x},{y})");
    }
}

#[test]
fn blend_thumbnail_of_a_real_blender_5_file() {
    check_blend_thumbnail(&decode_fixture("blend/cube-cone.blend"));
}

/// The same scene saved with `compress=True` (Blender 5 writes zstd).
#[test]
fn blend_thumbnail_of_a_zstd_compressed_file() {
    check_blend_thumbnail(&decode_fixture("blend/cube-cone-zstd.blend"));
}

/// Saved by Paint.NET 5.1.12 from an 800x600 image of four flat quadrants (red, green,
/// blue, yellow); the `thumb` element holds a 256x192 palette PNG.
#[test]
fn pdn_thumbnail_of_a_real_paint_net_file() {
    let img = decode_fixture("pdn/quadrants.pdn");
    assert_eq!((img.width(), img.height()), (256, 192));
    assert_near(&img, 20, 20, [255, 0, 0, 255], 0, "top-left");
    assert_near(&img, 235, 20, [0, 255, 0, 255], 0, "top-right");
    assert_near(&img, 20, 170, [0, 0, 255, 255], 0, "bottom-left");
    assert_near(&img, 235, 170, [255, 255, 0, 255], 0, "bottom-right");
}
