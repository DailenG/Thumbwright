//! HEIC/HEIF stills (HEVC) via `open-heic`: a pure-Rust, `forbid(unsafe_code)` reader.
//!
//! The HEVC decoder underneath (`rust_h265`) is memory-safe but young and can panic on
//! corrupt streams; `open-heic` contains those panics, and the stage still runs in the
//! worker ([`super::route::Placement::Worker`]) behind the Job Object's memory and CPU
//! limits and the parent's kill timer.
//!
//! What it cannot do falls through to the next tier (WIC with the Store codecs, where
//! installed): AV1 (AVIF), AVC, JPEG-coded items, overlays, non-4:2:0 chroma. Known limit:
//! the 4:0:0 alpha planes Apple writes are not applied, so such images decode opaque.

use image::{DynamicImage, RgbaImage};
use open_heic::{Codec, Error, Image, Limits};

use super::{DecodeError, Result, check_dimensions};
use crate::limits;

pub const EXTENSIONS: &[&str] = &["heic", "heif", "hif", "heics", "heifs"];

/// `open-heic` bounds from the tw-core budgets. Its memory estimate covers the decoded
/// canvas and the HEVC working copies; the thumbnail conversion that follows may add
/// [`limits::THUMBNAIL_SLACK_BYTES`], and the whole has to fit the worker's committed-memory
/// ceiling, so the slack is held back here.
fn open_heic_limits() -> Limits {
    Limits {
        max_width: limits::MAX_IMAGE_AXIS,
        max_height: limits::MAX_IMAGE_AXIS,
        max_pixels: limits::MAX_PIXEL_AREA,
        max_memory_bytes: limits::DECODE_ALLOC_BYTES - limits::THUMBNAIL_SLACK_BYTES,
    }
}

fn map_error(e: Error) -> DecodeError {
    match e {
        Error::NotHeif | Error::Unsupported(_) => DecodeError::Unsupported,
        Error::LimitExceeded(_) => DecodeError::TooLarge,
        // `Malformed`, `Hevc` and anything a later `open-heic` adds: a plain decode failure.
        other => DecodeError::malformed(format!("HEIF: {other}")),
    }
}

fn into_image(img: Image) -> Result<DynamicImage> {
    RgbaImage::from_raw(img.width, img.height, img.rgba)
        .map(DynamicImage::ImageRgba8)
        .ok_or_else(|| DecodeError::malformed("HEIF: buffer size"))
}

/// Which decode serves a request for `edge` pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Plan {
    /// Embedded thumbnails first (milliseconds, see [`Plan::accepts`]).
    Thumbnail,
    /// The primary image: no thumbnail can be large enough.
    Primary,
}

impl Plan {
    fn for_edge(primary_long_edge: u32, has_thumbnail: bool, edge: u32) -> Self {
        // A thumbnail is smaller than the primary image, so when the primary is no larger
        // than the request none of them qualifies; and without one the thumbnail call
        // would decode the primary anyway.
        if has_thumbnail && primary_long_edge > edge {
            Self::Thumbnail
        } else {
            Self::Primary
        }
    }

    /// Whether `thumb` (what `decode_thumbnail` returned) serves the request: it is at least
    /// `edge` on its long side, or it is the primary image itself (no embedded thumbnail
    /// was usable, so a full decode would repeat the same work).
    fn accepts(thumb: (u32, u32), primary: (u32, u32), edge: u32) -> bool {
        thumb.0.max(thumb.1) >= edge || thumb == primary
    }
}

/// Decodes the primary image, or its embedded thumbnail when that is already at least
/// `edge` on the long side (the shell asks for a few hundred pixels; the thumbnail is a
/// small HEVC picture and decodes in milliseconds where the full image takes much longer).
/// The thumbnail is returned unscaled: the caller resizes. EXIF-style orientation (`irot`,
/// `imir`) is applied.
pub fn decode(data: &[u8], edge: u32) -> Result<DynamicImage> {
    let info = open_heic::info(data).map_err(map_error)?;
    if info.codec != Codec::Hevc {
        return Err(DecodeError::Unsupported);
    }
    check_dimensions(info.width, info.height)?;
    let limits = open_heic_limits();
    let primary = (info.width, info.height);
    let plan = Plan::for_edge(info.width.max(info.height), info.has_thumbnail, edge);
    if plan == Plan::Thumbnail {
        let thumb = open_heic::decode_thumbnail(data, edge, &limits).map_err(map_error)?;
        if Plan::accepts((thumb.width, thumb.height), primary, edge) {
            return into_image(thumb);
        }
        // Dropped before the full decode allocates its canvas.
        drop(thumb);
    }
    into_image(open_heic::decode(data, &limits).map_err(map_error)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::GenericImageView;

    const QUAD: &[u8] = include_bytes!("../../tests/fixtures/heic/quad.heic");

    fn rgb(img: &DynamicImage, x: u32, y: u32) -> [u8; 3] {
        let p = img.get_pixel(x, y).0;
        [p[0], p[1], p[2]]
    }

    #[test]
    fn decodes_the_fixture_at_full_size() {
        let img = decode(QUAD, u32::MAX).unwrap();
        assert_eq!(img.dimensions(), (160, 120));
        assert!(matches!(img, DynamicImage::ImageRgba8(_)));
    }

    /// The fixture has four flat quadrants (tests/fixtures/README.md): all four differ.
    #[test]
    fn fixture_quadrants_are_distinct_and_flat() {
        let img = decode(QUAD, 4096).unwrap();
        let corners = [(10, 10), (150, 10), (10, 110), (150, 110)];
        let colours: Vec<[u8; 3]> = corners.iter().map(|&(x, y)| rgb(&img, x, y)).collect();
        for (i, a) in colours.iter().enumerate() {
            for b in &colours[i + 1..] {
                let spread = (0..3).map(|c| a[c].abs_diff(b[c])).max().unwrap();
                assert!(spread > 40, "quadrants too alike: {colours:?}");
            }
        }
        // Flat: a second pixel of the same quadrant matches within lossy-coding noise.
        let (a, b) = (rgb(&img, 10, 10), rgb(&img, 40, 40));
        assert!((0..3).all(|c| a[c].abs_diff(b[c]) <= 8), "{a:?} vs {b:?}");
    }

    #[test]
    fn small_requests_on_a_file_without_thumbnail_decode_the_primary_once() {
        // The fixture has no embedded thumbnail: any edge yields the 160x120 image.
        let info = open_heic::info(QUAD).unwrap();
        assert!(!info.has_thumbnail);
        for edge in [1, 64, 160, 4096] {
            assert_eq!(
                decode(QUAD, edge).unwrap().dimensions(),
                (160, 120),
                "{edge}"
            );
        }
    }

    #[test]
    fn thumbnail_or_primary_selection() {
        // No thumbnail, or a primary no larger than the request: decode the primary.
        assert_eq!(Plan::for_edge(4000, false, 256), Plan::Primary);
        assert_eq!(Plan::for_edge(4000, true, 4000), Plan::Primary);
        assert_eq!(Plan::for_edge(4000, true, 16384), Plan::Primary);
        // A thumbnail can only help a request smaller than the primary.
        assert_eq!(Plan::for_edge(4000, true, 256), Plan::Thumbnail);
        assert_eq!(Plan::for_edge(4000, true, 3999), Plan::Thumbnail);
    }

    #[test]
    fn a_thumbnail_is_accepted_only_when_large_enough_or_the_primary() {
        let primary = (3024, 4032);
        // Embedded 240x320 serves 256 only if its long edge reaches it.
        assert!(Plan::accepts((240, 320), primary, 256));
        assert!(Plan::accepts((240, 320), primary, 320));
        assert!(!Plan::accepts((240, 320), primary, 321));
        // open-heic fell back to the primary image: it is the best there is.
        assert!(Plan::accepts(primary, primary, 16384));
    }

    #[test]
    fn open_heic_errors_map_to_decode_errors() {
        assert_eq!(map_error(Error::NotHeif), DecodeError::Unsupported);
        assert_eq!(
            map_error(Error::Unsupported("av1")),
            DecodeError::Unsupported
        );
        assert_eq!(
            map_error(Error::LimitExceeded("pixels")),
            DecodeError::TooLarge
        );
        assert!(matches!(
            map_error(Error::Malformed("box")),
            DecodeError::Malformed(_)
        ));
        assert!(matches!(
            map_error(Error::Hevc("bad slice".into())),
            DecodeError::Malformed(_)
        ));
        // Only the plain failures fall through to the next tier (I3).
        assert!(map_error(Error::NotHeif).falls_through());
        assert!(map_error(Error::Malformed("x")).falls_through());
        assert!(!map_error(Error::LimitExceeded("x")).falls_through());
    }

    #[test]
    fn not_heif_input_is_unsupported() {
        let jpeg = [
            0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10, b'J', b'F', b'I', b'F', 0, 1,
        ];
        assert_eq!(decode(&jpeg, 256).unwrap_err(), DecodeError::Unsupported);
        assert_eq!(decode(b"", 256).unwrap_err(), DecodeError::Unsupported);
    }

    #[test]
    fn damaged_files_fail_without_panicking() {
        // Truncations and corruption of every kind land on a typed error, never a panic,
        // and never `TooLarge` for a file that is merely short.
        for len in [0, 8, 24, 64, 200, QUAD.len() / 2, QUAD.len() - 1] {
            match decode(&QUAD[..len], 256) {
                Ok(_) => {}
                Err(e) => assert!(e.falls_through(), "len {len}: {e}"),
            }
        }
        let mut bent = QUAD.to_vec();
        let n = bent.len();
        for b in &mut bent[n / 2..] {
            *b = !*b;
        }
        if let Err(e) = decode(&bent, 256) {
            assert!(e.falls_through(), "{e}");
        }
    }
}
