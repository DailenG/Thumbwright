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

use std::path::{Path, PathBuf};

use image::metadata::Orientation;
use image::{DynamicImage, ImageFormat};
use tw_core::decode::{jxl, raw};

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
