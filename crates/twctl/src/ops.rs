//! Image operations behind the CLI and the context-menu verbs. Everything runs in the
//! `twctl` process, never in Explorer (REQ-ISO-4); audio tag parsing additionally runs in
//! the decoder worker, as it does for thumbnails.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::codecs::webp::WebPEncoder;
use image::{
    DynamicImage, ExtendedColorType, ImageEncoder, ImageFormat, RgbaImage, imageops::FilterType,
};

use tw_core::decode::route::{self, Decoder};
use tw_core::decode::{DecodeError, Thumbnail};
use tw_core::{ids, worker};

use crate::exif;
use crate::jpeg_lossless::{self, Transform};
use crate::strip;

pub type Result<T, E = String> = std::result::Result<T, E>;

/// Edge used when rasterising vector input for conversion.
const VECTOR_EDGE: u32 = 2048;
/// Longest edge of audio cover art decoded for conversion (larger art is shrunk).
const COVER_ART_EDGE: u32 = 4096;
/// Longest edge decoded for conversion from worker-isolated raster formats (JPEG 2000).
const RASTER_EDGE: u32 = 16384;

/// Directory holding `twctl.exe`, `tw-worker.exe` and the portable INI.
pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Premultiplied BGRA thumbnail pixels back to straight RGBA.
pub fn thumbnail_to_image(t: &Thumbnail) -> RgbaImage {
    RgbaImage::from_fn(t.width, t.height, |x, y| {
        let i = ((y * t.width + x) * 4) as usize;
        let (b, g, r, a) = (t.bgra[i], t.bgra[i + 1], t.bgra[i + 2], t.bgra[i + 3]);
        let un = |c: u8| {
            if a == 0 {
                0
            } else {
                ((u16::from(c) * 255 + u16::from(a) / 2) / u16::from(a)).min(255) as u8
            }
        };
        image::Rgba([un(r), un(g), un(b), a])
    })
}

pub fn ext_of(p: &Path) -> String {
    p.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

pub fn read_input(p: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    let len = std::fs::metadata(p)
        .map_err(|e| format!("{}: {e}", p.display()))?
        .len();
    if len > max_bytes {
        return Err(format!("{}: larger than the max file size", p.display()));
    }
    std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))
}

/// Full-resolution decode through the format router. Memory-safe tiers run in this
/// process; decoders that parse hostile tags or call OS codecs or fixed-function libraries
/// (audio tags can overflow the stack, which would end `twctl` without the `--notify` message
/// box; video, PDF and metafiles use OS codecs; DjVu and JPEG 2000 cannot bound their
/// allocation) go through the Job-Object-confined worker, like in the shell.
pub fn decode_full(p: &Path, max_bytes: u64) -> Result<DynamicImage> {
    decode_bytes(p, read_input(p, max_bytes)?)
}

/// [`decode_full`] plus the source's EXIF block (TIFF structure, no `Exif\0\0` prefix) when
/// `keep_exif` is set and the source is a JPEG, PNG or WebP carrying one. The raster decoders
/// apply the EXIF orientation to the pixels, so [`encode_with_exif`] writes Orientation 1.
pub fn decode_full_with_exif(
    p: &Path,
    max_bytes: u64,
    keep_exif: bool,
) -> Result<(DynamicImage, Option<Vec<u8>>)> {
    let data = read_input(p, max_bytes)?;
    let exif = keep_exif
        .then(|| exif::from_container(&data).map(<[u8]>::to_vec))
        .flatten();
    Ok((decode_bytes(p, data)?, exif))
}

/// [`decode_full`] for bytes already read from `p`.
pub fn decode_bytes(p: &Path, data: Vec<u8>) -> Result<DynamicImage> {
    let data = Arc::new(data);
    let ext = ext_of(p);
    let chain = route::plan(Some(&ext), &data[..data.len().min(4096)]);
    if chain.is_empty() {
        return Err(format!("{}: unsupported file type", p.display()));
    }
    let inproc_edge = if chain.contains(&Decoder::Svg) {
        VECTOR_EDGE
    } else {
        u32::MAX
    };
    let mut last = DecodeError::Unsupported;
    for &decoder in chain {
        let result = match isolated_edge(decoder) {
            Some(edge) => worker::run(
                &exe_dir().join(ids::WORKER_EXE_NAME),
                decoder,
                &ext,
                data.clone(),
                edge,
                tw_core::limits::DEFAULT_VIDEO_PERCENT,
                worker::budget(decoder),
            )
            .map(|t| DynamicImage::from(thumbnail_to_image(&t))),
            None => decoder.run(&data, &ext, inproc_edge),
        };
        match result {
            Ok(img) => return Ok(img),
            Err(e) if e.falls_through() => last = e,
            Err(e) => return Err(format!("{}: {e}", p.display())),
        }
    }
    Err(format!("{}: {last}", p.display()))
}

/// Longest edge requested from a decoder that must run in the worker, or `None` for
/// decoders that run in this process.
fn isolated_edge(decoder: Decoder) -> Option<u32> {
    match decoder {
        Decoder::Pdf | Decoder::Metafile | Decoder::OleMetafile | Decoder::Djvu => {
            Some(VECTOR_EDGE)
        }
        Decoder::J2k => Some(RASTER_EDGE),
        Decoder::Audio | Decoder::Video => Some(COVER_ART_EDGE),
        _ => None,
    }
}

/// Writable target formats (native pure-Rust encoders).
pub fn format_for_target(ext: &str) -> Option<ImageFormat> {
    Some(match ext {
        "png" => ImageFormat::Png,
        "jpg" | "jpeg" | "jpe" | "jfif" => ImageFormat::Jpeg,
        "webp" => ImageFormat::WebP,
        "bmp" => ImageFormat::Bmp,
        "gif" => ImageFormat::Gif,
        "tif" | "tiff" => ImageFormat::Tiff,
        "ico" => ImageFormat::Ico,
        "qoi" => ImageFormat::Qoi,
        "tga" => ImageFormat::Tga,
        _ => return None,
    })
}

/// Whether `ext` has a writer: a native `image` encoder, or AVIF / JPEG XL.
pub fn can_write(ext: &str) -> bool {
    format_for_target(ext).is_some() || crate::modern::handles(ext)
}

/// Encodes `img` for `ext`. JPEG takes `quality` (1-100, default 90); WebP is lossless.
pub fn encode(img: &DynamicImage, ext: &str, quality: Option<u8>) -> Result<Vec<u8>> {
    encode_with_exif(img, ext, quality, None)
}

/// Largest EXIF block an APP1 segment can hold (64 KiB less length and `Exif\0\0`).
const MAX_JPEG_EXIF: usize = 65535 - 2 - 6;

/// [`encode`] that also writes `exif` (a source EXIF block) where the target supports it:
/// JPEG, PNG (`eXIf`), WebP and TIFF. The block is normalised for the pixels being written
/// (see `exif::normalise`); formats without EXIF support ignore it.
pub fn encode_with_exif(
    img: &DynamicImage,
    ext: &str,
    quality: Option<u8>,
    exif: Option<&[u8]>,
) -> Result<Vec<u8>> {
    if crate::modern::handles(ext) {
        return crate::modern::encode(img, ext, quality);
    }
    let format = format_for_target(ext).ok_or_else(|| format!("no writer for .{ext}"))?;
    let dims = (img.width(), img.height());
    let normalised = exif.and_then(|e| exif::normalise(e, dims));
    let mut out = Vec::new();
    match format {
        ImageFormat::Jpeg => {
            let rgb = img.to_rgb8();
            let mut enc =
                JpegEncoder::new_with_quality(&mut out, quality.unwrap_or(90).clamp(1, 100));
            match normalised {
                Some(e) if e.len() <= MAX_JPEG_EXIF => {
                    enc.set_exif_metadata(e).map_err(|e| e.to_string())?;
                }
                Some(e) => eprintln!(
                    "twctl: note: EXIF block of {} bytes does not fit a JPEG segment; not carried over",
                    e.len()
                ),
                None => {}
            }
            enc.encode_image(&rgb).map_err(|e| e.to_string())?;
        }
        ImageFormat::Png if normalised.is_some() => {
            let mut enc = PngEncoder::new(&mut out);
            if let Some(e) = normalised {
                enc.set_exif_metadata(e).map_err(|e| e.to_string())?;
            }
            img.write_with_encoder(enc).map_err(|e| e.to_string())?;
        }
        ImageFormat::WebP if normalised.is_some() => {
            let rgba = img.to_rgba8();
            let mut enc = WebPEncoder::new_lossless(&mut out);
            if let Some(e) = normalised {
                enc.set_exif_metadata(e).map_err(|e| e.to_string())?;
            }
            enc.write_image(&rgba, rgba.width(), rgba.height(), ExtendedColorType::Rgba8)
                .map_err(|e| e.to_string())?;
        }
        ImageFormat::Ico => {
            let icon = if img.width() > 256 || img.height() > 256 {
                img.resize(256, 256, FilterType::Lanczos3)
            } else {
                img.clone()
            };
            icon.to_rgba8()
                .write_to(&mut Cursor::new(&mut out), format)
                .map_err(|e| e.to_string())?;
        }
        ImageFormat::Bmp
        | ImageFormat::Gif
        | ImageFormat::Tga
        | ImageFormat::Qoi
        | ImageFormat::WebP => {
            img.to_rgba8()
                .write_to(&mut Cursor::new(&mut out), format)
                .map_err(|e| e.to_string())?;
        }
        _ => {
            img.write_to(&mut Cursor::new(&mut out), format)
                .map_err(|e| e.to_string())?;
            if format == ImageFormat::Tiff
                && let Some(e) = exif
            {
                out = exif::embed_in_tiff(out, e, dims);
            }
        }
    }
    Ok(out)
}

static SEQ: AtomicU32 = AtomicU32::new(0);

/// Attempts at a fresh staging name before giving up.
const STAGE_ATTEMPTS: u32 = 1000;

/// Writes `bytes` to a freshly created sibling `<dest>.<pid>-<seq>.twtmp`. The name is
/// opened with `create_new`, so a pre-existing file or link there is never written
/// through; the next sequence number is tried instead. The temp is removed on failure.
fn stage(dest: &Path, bytes: &[u8]) -> std::io::Result<PathBuf> {
    use std::io::Write;
    for _ in 0..STAGE_ATTEMPTS {
        let mut tmp = dest.as_os_str().to_owned();
        tmp.push(format!(
            ".{}-{}.twtmp",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let tmp = PathBuf::from(tmp);
        let mut file = match std::fs::File::create_new(&tmp) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        return match file.write_all(bytes) {
            Ok(()) => Ok(tmp),
            Err(e) => {
                drop(file);
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        };
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "no free staging name",
    ))
}

/// Atomic write (REQ-DEC-3): staged temp sibling, then a replacing rename. A failure
/// leaves no partial destination.
pub fn write_atomic(dest: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = stage(dest, bytes).map_err(|e| format!("{}: {e}", dest.display()))?;
    std::fs::rename(&tmp, dest).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", dest.display())
    })
}

/// Writes `bytes` to a new file `<dir>\<stem><suffix>.<ext>`, adding ` (1)`, ` (2)`, ...
/// when the name is taken. The final name is claimed by a non-replacing move, so a
/// concurrent writer or an existing file is never overwritten. Returns the path written.
pub fn write_new(src: &Path, suffix: &str, ext: &str, bytes: &[u8]) -> Result<PathBuf> {
    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS};
    use windows::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
    use windows::core::HSTRING;

    let dir = src.parent().unwrap_or(Path::new("."));
    let stem = src
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let first = dir.join(format!("{stem}{suffix}.{ext}"));
    let tmp = stage(&first, bytes).map_err(|e| format!("{}: {e}", first.display()))?;
    let tmp_w = HSTRING::from(tmp.as_os_str());
    let mut n = 0u32;
    loop {
        let candidate = if n == 0 {
            first.clone()
        } else {
            dir.join(format!("{stem}{suffix} ({n}).{ext}"))
        };
        let candidate_w = HSTRING::from(candidate.as_os_str());
        // SAFETY: both arguments are valid NUL-terminated wide strings that outlive the call.
        let moved = unsafe { MoveFileExW(&tmp_w, &candidate_w, MOVEFILE_WRITE_THROUGH) };
        match moved {
            Ok(()) => return Ok(candidate),
            Err(e)
                if n < u32::MAX
                    && (e.code() == ERROR_ALREADY_EXISTS.to_hresult()
                        || e.code() == ERROR_FILE_EXISTS.to_hresult()) =>
            {
                n += 1;
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!("{}: {e}", candidate.display()));
            }
        }
    }
}

/// `WxH` (fit inside, never enlarge) or `N%`.
pub fn parse_resize(spec: &str) -> Result<Resize> {
    if let Some(p) = spec.strip_suffix('%') {
        let pct: u32 = p
            .trim()
            .parse()
            .map_err(|_| format!("bad percentage {spec}"))?;
        if pct == 0 || pct > 1000 {
            return Err(format!("bad percentage {spec}"));
        }
        return Ok(Resize::Percent(pct));
    }
    let (w, h) = spec
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("bad size {spec} (expected WxH or N%)"))?;
    let (w, h) = (w.trim().parse::<u32>(), h.trim().parse::<u32>());
    match (w, h) {
        (Ok(w), Ok(h)) if w > 0 && h > 0 => Ok(Resize::Fit(w, h)),
        _ => Err(format!("bad size {spec}")),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resize {
    Fit(u32, u32),
    Percent(u32),
}

pub fn apply_resize(img: &DynamicImage, r: Resize) -> DynamicImage {
    match r {
        Resize::Fit(w, h) if img.width() > w || img.height() > h => {
            img.resize(w, h, FilterType::Lanczos3)
        }
        Resize::Fit(..) => img.clone(),
        Resize::Percent(p) => {
            let w = (img.width() as u64 * p as u64 / 100).max(1) as u32;
            let h = (img.height() as u64 * p as u64 / 100).max(1) as u32;
            img.resize_exact(w, h, FilterType::Lanczos3)
        }
    }
}

pub fn rotate(img: &DynamicImage, by: &str) -> Result<DynamicImage> {
    Ok(match by {
        "right" => img.rotate90(),
        "left" => img.rotate270(),
        "180" => img.rotate180(),
        "fliph" => img.fliph(),
        "flipv" => img.flipv(),
        _ => return Err(format!("bad --by {by} (right|left|180|fliph|flipv)")),
    })
}

/// JPEG quality of the re-encode fallback of [`rotate_file`].
const ROTATE_FALLBACK_QUALITY: u8 = 95;

/// A rotated or flipped file ready to be written next to its source.
pub struct Rotated {
    pub bytes: Vec<u8>,
    /// Extension of the output.
    pub ext: String,
    /// A notice for the user: the pixels were re-encoded although the input was a JPEG, or
    /// data stored after the image could not be carried over.
    pub note: Option<String>,
}

/// Rotates or flips `p`. A JPEG is transformed in the DCT coefficient domain (no
/// generational loss) with its EXIF orientation folded in and reset to 1. When that cannot
/// be done losslessly (a partial edge MCU on a mirrored axis, progressive or unusual
/// encodings the transform refuses, damaged data) the file is decoded and re-encoded at
/// quality 95 instead, and `note` says why. Other formats are decoded, transformed and
/// written as `fallback_ext`. EXIF travels with the image when `keep_exif` is set.
pub fn rotate_file(
    p: &Path,
    by: &str,
    max_bytes: u64,
    keep_exif: bool,
    fallback_ext: &str,
) -> Result<Rotated> {
    let xf = Transform::by_name(by)
        .ok_or_else(|| format!("bad --by {by} (right|left|180|fliph|flipv)"))?;
    let data = read_input(p, max_bytes)?;
    let mut note = None;
    if data.starts_with(&[0xFF, 0xD8]) {
        let stored = exif::from_container(&data)
            .and_then(exif::orientation)
            .map_or(Transform::IDENTITY, Transform::from_exif);
        match jpeg_lossless::transform_reporting(&data, stored.then(xf)) {
            Ok((out, dropped)) => {
                let ext = ext_of(p);
                let ext = if format_for_target(&ext) == Some(ImageFormat::Jpeg) {
                    ext
                } else {
                    "jpg".to_string()
                };
                let bytes = if keep_exif { out } else { strip::strip(&out)? };
                let note = (dropped > 0).then(|| {
                    format!(
                        "{}: {dropped} bytes stored after the image (vendor extensions such as motion-photo video, depth maps or gain maps) were not carried over, because they describe the unrotated picture",
                        p.display()
                    )
                });
                return Ok(Rotated { bytes, ext, note });
            }
            Err(why) => {
                note = Some(format!(
                    "{}: not rotated losslessly ({why}); re-encoded at quality {ROTATE_FALLBACK_QUALITY}",
                    p.display()
                ));
            }
        }
    }
    let exif = keep_exif
        .then(|| exif::from_container(&data).map(<[u8]>::to_vec))
        .flatten();
    let img = rotate(&decode_bytes(p, data)?, by)?;
    Ok(Rotated {
        bytes: encode_with_exif(
            &img,
            fallback_ext,
            Some(ROTATE_FALLBACK_QUALITY),
            exif.as_deref(),
        )?,
        ext: fallback_ext.to_string(),
        note,
    })
}

/// `1MB`, `500KB`, or a byte count.
pub fn parse_size(spec: &str) -> Result<u64> {
    let s = spec.trim().to_ascii_uppercase();
    let (num, mul) = if let Some(n) = s.strip_suffix("MB") {
        (n, 1024 * 1024)
    } else if let Some(n) = s.strip_suffix("KB") {
        (n, 1024)
    } else {
        (s.as_str(), 1)
    };
    num.trim()
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .and_then(|n| n.checked_mul(mul))
        .ok_or_else(|| format!("bad size {spec}"))
}

/// JPEG quality-and-scale search for the largest result not exceeding `max_bytes`.
pub fn compress_to(img: &DynamicImage, max_bytes: u64) -> Result<Vec<u8>> {
    for scale in [100u32, 85, 70, 55, 40, 30, 20] {
        let scaled = if scale == 100 {
            img.clone()
        } else {
            apply_resize(img, Resize::Percent(scale))
        };
        let (mut lo, mut hi, mut best) = (30u8, 95u8, None);
        while lo <= hi {
            let q = lo + (hi - lo) / 2;
            let bytes = encode(&scaled, "jpg", Some(q))?;
            if bytes.len() as u64 <= max_bytes {
                best = Some(bytes);
                lo = q + 1;
            } else {
                hi = q - 1;
            }
        }
        if let Some(b) = best {
            return Ok(b);
        }
    }
    Err(format!("cannot compress below {max_bytes} bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::RgbImage;

    fn noisy(w: u32, h: u32) -> DynamicImage {
        let mut s = 0x1234_5678u32;
        DynamicImage::ImageRgb8(RgbImage::from_fn(w, h, |_, _| {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            image::Rgb([s as u8, (s >> 8) as u8, (s >> 16) as u8])
        }))
    }

    #[test]
    fn resize_specs() {
        assert_eq!(parse_resize("1920x1080"), Ok(Resize::Fit(1920, 1080)));
        assert_eq!(parse_resize("50%"), Ok(Resize::Percent(50)));
        assert!(
            parse_resize("0x5").is_err()
                && parse_resize("abc").is_err()
                && parse_resize("0%").is_err()
        );
        let img = noisy(400, 200);
        let fit = apply_resize(&img, Resize::Fit(100, 100));
        assert_eq!((fit.width(), fit.height()), (100, 50));
        let same = apply_resize(&img, Resize::Fit(1000, 1000));
        assert_eq!(
            (same.width(), same.height()),
            (400, 200),
            "fit never enlarges"
        );
    }

    #[test]
    fn size_specs() {
        assert_eq!(parse_size("1MB"), Ok(1024 * 1024));
        assert_eq!(parse_size("500kb"), Ok(500 * 1024));
        assert_eq!(parse_size("1234"), Ok(1234));
        assert!(parse_size("0").is_err() && parse_size("MB").is_err());
    }

    #[test]
    fn worker_pixels_are_unpremultiplied_to_rgba() {
        let t = Thumbnail {
            width: 2,
            height: 1,
            // Half-transparent orange (premultiplied BGRA), then fully transparent.
            bgra: vec![0, 64, 128, 128, 9, 9, 9, 0],
        };
        let img = thumbnail_to_image(&t);
        assert_eq!(img.get_pixel(0, 0).0, [255, 128, 0, 128]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 0, 0, 0]);
    }

    #[test]
    fn size_suffix_overflow_is_rejected() {
        let max_mb = u64::MAX / (1024 * 1024);
        assert_eq!(parse_size(&format!("{max_mb}MB")), Ok(max_mb * 1024 * 1024));
        assert!(parse_size(&format!("{}MB", max_mb + 1)).is_err());
        assert!(parse_size(&format!("{}KB", u64::MAX)).is_err());
        assert_eq!(parse_size(&u64::MAX.to_string()), Ok(u64::MAX));
    }

    #[test]
    fn compress_meets_target_or_fails() {
        let img = noisy(600, 600);
        let out = compress_to(&img, 60 * 1024).unwrap();
        assert!(out.len() <= 60 * 1024);
        assert!(image::load_from_memory(&out).is_ok());
        assert!(compress_to(&img, 200).is_err());
    }

    #[test]
    fn rotation_swaps_dimensions() {
        let img = noisy(30, 10);
        let r = rotate(&img, "right").unwrap();
        assert_eq!((r.width(), r.height()), (10, 30));
        assert!(rotate(&img, "sideways").is_err());
    }

    #[test]
    fn encode_round_trips_and_rejects_unknown_targets() {
        let img = noisy(20, 10);
        for ext in [
            "png", "jpg", "webp", "bmp", "gif", "tiff", "ico", "qoi", "tga",
        ] {
            let bytes = encode(&img, ext, None).unwrap();
            let back = image::load_from_memory_with_format(&bytes, format_for_target(ext).unwrap())
                .unwrap();
            assert_eq!((back.width(), back.height()), (20, 10), "{ext}");
        }
        assert_eq!(
            encode(&img, "heic", None),
            Err("no writer for .heic".into())
        );
    }

    fn temp_dir_for(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("twctl-ops-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn leftovers(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .to_string_lossy()
                    .ends_with(".twtmp")
            })
            .count()
    }

    #[test]
    fn atomic_write_and_new_names() {
        let dir = temp_dir_for("names");
        let src = dir.join("a.png");
        std::fs::write(&src, b"x").unwrap();
        let first = write_new(&src, " (edited)", "png", b"1").unwrap();
        assert_eq!(first, dir.join("a (edited).png"));
        let second = write_new(&src, " (edited)", "png", b"2").unwrap();
        assert_eq!(second, dir.join("a (edited) (1).png"));
        assert_eq!(std::fs::read(&first).unwrap(), b"1");
        assert_eq!(std::fs::read(&second).unwrap(), b"2");
        write_atomic(&first, b"3").unwrap();
        assert_eq!(std::fs::read(&first).unwrap(), b"3");
        assert_eq!(leftovers(&dir), 0);
        assert!(write_atomic(&dir.join("missing-dir").join("x.png"), b"1").is_err());
        assert!(write_new(&dir.join("missing-dir").join("x.png"), "", "png", b"1").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn staging_never_writes_through_existing_link() {
        let dir = temp_dir_for("link");
        let sentinel = dir.join("sentinel.txt");
        std::fs::write(&sentinel, b"keep").unwrap();
        let dest = dir.join("out.png");
        // Pre-link the next staging names for this process to the sentinel.
        let start = SEQ.load(Ordering::Relaxed);
        for seq in start..start + 64 {
            let mut tmp = dest.as_os_str().to_owned();
            tmp.push(format!(".{}-{seq}.twtmp", std::process::id()));
            std::fs::hard_link(&sentinel, PathBuf::from(tmp)).unwrap();
        }
        write_atomic(&dest, b"new").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn concurrent_write_new_yields_distinct_files() {
        const N: usize = 8;
        let dir = temp_dir_for("race");
        let src = dir.join("same.png");
        let barrier = std::sync::Barrier::new(N);
        let outs: Vec<PathBuf> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..N)
                .map(|i| {
                    let (src, barrier) = (&src, &barrier);
                    s.spawn(move || {
                        barrier.wait();
                        write_new(src, "", "jpg", format!("{i}").as_bytes()).unwrap()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let mut contents: Vec<String> = outs
            .iter()
            .map(|p| std::fs::read_to_string(p).unwrap())
            .collect();
        contents.sort();
        contents.dedup();
        assert_eq!(contents.len(), N, "every writer kept its own file");
        assert_eq!(leftovers(&dir), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A JPEG of `w` x `h` noise carrying the camera-like EXIF block of `exif::testutil`.
    fn camera_jpeg(w: u32, h: u32, orientation: u16) -> Vec<u8> {
        use crate::exif::testutil::{camera_exif, inject_into_jpeg};
        let plain = encode(&noisy(w, h), "jpg", Some(90)).unwrap();
        inject_into_jpeg(&plain, &camera_exif(orientation, (w, h)))
    }

    fn decode_as(ext: &str, bytes: &[u8]) -> DynamicImage {
        decode_bytes(Path::new(&format!("x.{ext}")), bytes.to_vec()).unwrap()
    }

    #[test]
    fn rotate_file_is_lossless_for_jpeg_and_folds_in_exif_orientation() {
        let dir = temp_dir_for("rot-lossless");
        // Stored 64x48 with Orientation 6: displayed 48x64. Rotating right turns the
        // displayed image upside-down relative to the stored pixels, so the stored pixels
        // get a 180 degree turn and Orientation becomes 1.
        let src = dir.join("cam.jpg");
        std::fs::write(&src, camera_jpeg(64, 48, 6)).unwrap();
        let r = rotate_file(&src, "right", u64::MAX, true, "jpg").unwrap();
        assert!(r.note.is_none() && r.ext == "jpg");
        let shown_before = decode_full(&src, u64::MAX).unwrap();
        assert_eq!((shown_before.width(), shown_before.height()), (48, 64));
        let want = rotate(&shown_before, "right").unwrap().to_rgb8();
        let got = decode_as("jpg", &r.bytes).to_rgb8();
        assert_eq!(got.dimensions(), want.dimensions());
        let worst = got
            .as_raw()
            .iter()
            .zip(want.as_raw())
            .map(|(a, b)| (i16::from(*a) - i16::from(*b)).abs())
            .max()
            .unwrap();
        assert!(worst <= 3, "decoder rounding only, got {worst}");
        let block = exif::from_container(&r.bytes).expect("EXIF kept");
        assert_eq!(exif::orientation(block), Some(1));
        let s = exif::summarize(block);
        assert_eq!(
            (s.make.as_deref(), s.model.as_deref()),
            (Some("Thumbcam"), Some("TC-100 Mark II"))
        );
        // Keep EXIF off: the lossless output carries no metadata at all.
        let bare = rotate_file(&src, "right", u64::MAX, false, "jpg").unwrap();
        assert!(exif::from_container(&bare.bytes).is_none());
        assert_eq!(decode_as("jpg", &bare.bytes).to_rgb8(), got);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn four_right_turns_reproduce_the_original_decode_exactly() {
        let dir = temp_dir_for("rot4");
        let plain = encode(&noisy(80, 48), "jpg", Some(92)).unwrap();
        let mut path = dir.join("a.jpg");
        std::fs::write(&path, &plain).unwrap();
        for n in 0..4 {
            let r = rotate_file(&path, "right", u64::MAX, true, "jpg").unwrap();
            assert!(r.note.is_none(), "turn {n}");
            path = dir.join(format!("turn{n}.jpg"));
            std::fs::write(&path, r.bytes).unwrap();
        }
        let (before, after) = (
            decode_as("jpg", &plain),
            decode_full(&path, u64::MAX).unwrap(),
        );
        assert_eq!(before.to_rgb8(), after.to_rgb8(), "no generational loss");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rotate_file_reencodes_with_a_note_when_a_partial_mcu_blocks_it() {
        let dir = temp_dir_for("rot-fallback");
        // 50x30 is not a multiple of the 8x8 MCU; Orientation 6 means displayed 30x50.
        let src = dir.join("odd.jpg");
        std::fs::write(&src, camera_jpeg(50, 30, 6)).unwrap();
        let r = rotate_file(&src, "right", u64::MAX, true, "jpg").unwrap();
        let note = r.note.expect("a note explains the re-encode");
        assert!(
            note.contains("not rotated losslessly") && note.contains("quality 95"),
            "{note}"
        );
        // Displayed 30x50, turned right: 50x30, and Orientation was reset so decoders keep it.
        let img = decode_as("jpg", &r.bytes);
        assert_eq!((img.width(), img.height()), (50, 30));
        let block = exif::from_container(&r.bytes).expect("EXIF carried");
        assert_eq!(exif::orientation(block), Some(1));
        assert_eq!(exif::summarize(block).make.as_deref(), Some("Thumbcam"));
        assert!(rotate_file(&src, "sideways", u64::MAX, true, "jpg").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rotate_file_says_when_vendor_data_after_the_image_is_dropped() {
        let dir = temp_dir_for("rot-trailer");
        let src = dir.join("motion.jpg");
        let mut data = camera_jpeg(64, 48, 1);
        data.extend_from_slice(&[0xAA; 5000]);
        std::fs::write(&src, data).unwrap();
        let r = rotate_file(&src, "left", u64::MAX, true, "jpg").unwrap();
        let note = r.note.expect("a note names the dropped data");
        assert!(note.contains("5000 bytes stored after the image"), "{note}");
        assert!(r.bytes.ends_with(&[0xFF, 0xD9]));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rotate_file_handles_non_jpeg_input() {
        let dir = temp_dir_for("rot-png");
        let src = dir.join("a.png");
        std::fs::write(&src, encode(&noisy(7, 3), "png", None).unwrap()).unwrap();
        let r = rotate_file(&src, "left", u64::MAX, true, "png").unwrap();
        assert!(r.note.is_none() && r.ext == "png");
        let img = decode_as("png", &r.bytes);
        assert_eq!((img.width(), img.height()), (3, 7));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn conversion_carries_exif_with_orientation_normalised() {
        let dir = temp_dir_for("exif-carry");
        let src = dir.join("cam.jpg");
        std::fs::write(&src, camera_jpeg(64, 48, 6)).unwrap();
        let (img, exif_block) = decode_full_with_exif(&src, u64::MAX, true).unwrap();
        assert_eq!(
            (img.width(), img.height()),
            (48, 64),
            "the decoder applied Orientation 6"
        );
        assert!(exif_block.is_some());
        for ext in ["jpg", "png", "webp", "tiff"] {
            let bytes = encode_with_exif(&img, ext, None, exif_block.as_deref()).unwrap();
            let s = exif::summarize_file(&bytes);
            assert_eq!(s.make.as_deref(), Some("Thumbcam"), "{ext}");
            assert_eq!(s.model.as_deref(), Some("TC-100 Mark II"), "{ext}");
            assert_eq!(s.datetime.as_deref(), Some("2023:07:04 12:34:56"), "{ext}");
            assert!(s.gps.is_some(), "{ext}");
            // Orientation 6 left in place would make a decoder turn the pixels again.
            let back = decode_as(ext, &bytes);
            assert_eq!((back.width(), back.height()), (48, 64), "{ext}");
            if let Some(block) = exif::from_container(&bytes) {
                assert_eq!(exif::orientation(block), Some(1), "{ext}");
            }
        }
        // Without the block, or for formats that cannot hold EXIF, nothing is written.
        let plain = encode_with_exif(&img, "png", None, None).unwrap();
        assert!(exif::from_container(&plain).is_none());
        for ext in ["bmp", "gif", "qoi", "tga", "ico"] {
            assert!(encode_with_exif(&img, ext, None, exif_block.as_deref()).is_ok());
        }
        assert!(
            decode_full_with_exif(&src, u64::MAX, false)
                .unwrap()
                .1
                .is_none()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
