//! Real application files (see `crates/tw-core/tests/fixtures/README.md` for how each was
//! made) decoded through the same `Engine` the shell extension uses, so the decoders that
//! need OS codecs run in the real `tw-worker.exe`: WIC for the HEIC still inside a
//! `.livp`, Media Foundation for a BDAV `.m2ts`.
#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tw_core::decode::Thumbnail;
use tw_core::pipeline::Engine;

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
    Engine::new(worker())
        .with_video_percent(video_percent)
        .thumbnail(Arc::new(fixture(rel)), Some(ext), 256)
        .map_err(|e| format!("{rel}: {e:?}"))
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

/// The HEIC still, decoded on its own. `None` when the OS has no HEVC decoder (the HEVC
/// Video Extensions are an optional Store package), in which case the HEIC tests cannot say
/// anything about the code under test and are skipped.
fn heic_still() -> Option<Thumbnail> {
    match thumbnail("heic/quad.heic", "heic", 30) {
        Ok(t) => Some(t),
        Err(e) => {
            eprintln!("skipping: this OS cannot decode HEIC ({e})");
            None
        }
    }
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
    if let Some(t) = heic_still() {
        assert_quadrants(&t, "quad.heic");
    }
}

/// The `.livp` holds `IMG_0001.HEIC` (the same still) and `IMG_0001.MOV`; the thumbnail must
/// be the still, not a frame of the movie (which is a 320x180 red/green/blue sequence).
#[test]
fn livp_thumbnail_is_the_heic_still() {
    if heic_still().is_none() {
        return;
    }
    let t = thumbnail("livp/live-photo.livp", "livp", 30).unwrap();
    assert_quadrants(&t, "live-photo.livp");
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
