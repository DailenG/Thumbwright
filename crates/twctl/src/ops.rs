//! Image operations behind the CLI and the context-menu verbs. Everything runs in the
//! `twctl` process, never in Explorer (REQ-ISO-4); audio tag parsing additionally runs in
//! the decoder worker, as it does for thumbnails.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::codecs::webp::WebPEncoder;
use image::{
    DynamicImage, ExtendedColorType, ImageEncoder, ImageFormat, RgbaImage, imageops::FilterType,
};

use tw_core::config::Settings;
use tw_core::decode::route::{self, Decoder};
use tw_core::decode::{DecodeError, Thumbnail};
use tw_core::{ids, worker};

use crate::carry::{self, Metadata};
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

/// Settings that shape conversions (`JpegQuality`, `PngCompression`, `UseEmbeddedThumbs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Defaults {
    jpeg_quality: u8,
    png_compression: u8,
    prefer_embedded: bool,
}

static DEFAULTS: OnceLock<Defaults> = OnceLock::new();

/// Makes the saved `JpegQuality` and `PngCompression` the defaults of every encode that gets
/// no explicit quality, and `UseEmbeddedThumbs` the decoder order for camera RAW input.
/// Called once by `main`; later calls are ignored.
impl From<&Settings> for Defaults {
    fn from(s: &Settings) -> Self {
        Self {
            jpeg_quality: s.jpeg_quality.clamp(1, 100),
            png_compression: s.png_compression.min(9),
            prefer_embedded: s.use_embedded_thumbs,
        }
    }
}

pub fn set_defaults(settings: &Settings) {
    let _ = DEFAULTS.set(Defaults::from(settings));
}

fn defaults() -> Defaults {
    DEFAULTS
        .get()
        .copied()
        .unwrap_or_else(|| Defaults::from(&Settings::default()))
}

/// PNG compression level 0 (stored) to 9 (smallest).
fn png_compression(level: u8) -> image::codecs::png::CompressionType {
    use image::codecs::png::CompressionType;
    match level {
        0 => CompressionType::Uncompressed,
        n => CompressionType::Level(n.min(9)),
    }
}

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
/// box; WIC, video, PDF and metafiles use OS codecs and need COM, which only the worker
/// initialises; DjVu and JPEG 2000 cannot bound their allocation; the HEVC decoder behind
/// HEIC panics on corrupt streams) go through the Job-Object-confined worker, like in the
/// shell. `twctl` never hosts WIC itself (I2).
pub fn decode_full(p: &Path, max_bytes: u64) -> Result<DynamicImage> {
    decode_bytes(p, read_input(p, max_bytes)?)
}

/// [`decode_full`] plus the source's EXIF block, ICC profile and XMP packet when
/// `keep_metadata` is set and the source is a JPEG, PNG, WebP or TIFF carrying them (see
/// [`Metadata::read`]); problems with the source's metadata are printed as `note:` lines.
/// The raster decoders apply the EXIF orientation to the pixels, so [`encode_with_meta`]
/// writes Orientation 1.
pub fn decode_full_with_meta(
    p: &Path,
    max_bytes: u64,
    keep_metadata: bool,
) -> Result<(DynamicImage, Metadata)> {
    let data = read_input(p, max_bytes)?;
    let meta = read_metadata(p, &data, keep_metadata);
    Ok((decode_bytes(p, data)?, meta))
}

/// The metadata of `data` (the contents of `p`), or none when `keep_metadata` is off.
fn read_metadata(p: &Path, data: &[u8], keep_metadata: bool) -> Metadata {
    if !keep_metadata {
        return Metadata::default();
    }
    let meta = Metadata::read(&ext_of(p), data);
    meta.notes
        .iter()
        .for_each(|n| eprintln!("twctl: note: {}: {n}", p.display()));
    meta
}

/// [`decode_full`] for bytes already read from `p`.
pub fn decode_bytes(p: &Path, data: Vec<u8>) -> Result<DynamicImage> {
    let data = Arc::new(data);
    let ext = ext_of(p);
    let chain = route::plan(Some(&ext), &data[..data.len().min(4096)]);
    if chain.is_empty() {
        return Err(format!("{}: unsupported file type", p.display()));
    }
    let chain = route::ordered(chain, defaults().prefer_embedded);
    let inproc_edge = if chain.contains(&Decoder::Svg) {
        VECTOR_EDGE
    } else {
        u32::MAX
    };
    let mut last = DecodeError::Unsupported;
    for &decoder in chain.iter() {
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
        Decoder::J2k | Decoder::Wic | Decoder::Heif | Decoder::Livp => Some(RASTER_EDGE),
        Decoder::Archive => Some(VECTOR_EDGE),
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

/// Encodes `img` for `ext` without metadata. JPEG takes `quality` (1-100, default 90);
/// WebP is lossless.
pub fn encode(img: &DynamicImage, ext: &str, quality: Option<u8>) -> Result<Vec<u8>> {
    encode_noted(img, ext, quality, &Metadata::default()).map(|(bytes, _)| bytes)
}

/// [`encode`] that also writes the metadata of the source (`meta`) where the target holds
/// it, and prints a `note:` line for each part that could not be carried (see
/// [`carry::prepare`]).
pub fn encode_with_meta(
    img: &DynamicImage,
    ext: &str,
    quality: Option<u8>,
    meta: &Metadata,
) -> Result<Vec<u8>> {
    let (bytes, notes) = encode_noted(img, ext, quality, meta)?;
    notes.iter().for_each(|n| eprintln!("twctl: note: {n}"));
    Ok(bytes)
}

/// [`encode_with_meta`] returning the notes instead of printing them. The EXIF block and the
/// XMP packet are normalised for the pixels being written, the ICC profile is written as is
/// when it describes them (see [`carry::prepare`]). JPEG, PNG and WebP take all three, TIFF
/// all three, AVIF only EXIF, JPEG XL EXIF and XMP; other targets ignore the metadata.
pub fn encode_noted(
    img: &DynamicImage,
    ext: &str,
    quality: Option<u8>,
    meta: &Metadata,
) -> Result<(Vec<u8>, Vec<String>)> {
    let carried = carry::Target::of(ext)
        .map(|t| carry::prepare(meta, t, img))
        .unwrap_or_default();
    let mut notes = carried.notes.clone();
    let (exif, icc, xmp) = (
        carried.exif.as_deref(),
        carried.icc.as_deref(),
        carried.xmp.as_deref(),
    );
    if crate::modern::handles(ext) {
        return crate::modern::encode(img, ext, quality, &carried).map(|b| (b, notes));
    }
    let format = format_for_target(ext).ok_or_else(|| format!("no writer for .{ext}"))?;
    let dims = (img.width(), img.height());
    let mut out = Vec::new();
    match format {
        ImageFormat::Jpeg => {
            let rgb = img.to_rgb8();
            let mut enc = JpegEncoder::new_with_quality(
                &mut out,
                quality.unwrap_or(defaults().jpeg_quality).clamp(1, 100),
            );
            if let Some(e) = exif {
                enc.set_exif_metadata(e.to_vec())
                    .map_err(|e| e.to_string())?;
            }
            if let Some(p) = icc {
                enc.set_icc_profile(p.to_vec()).map_err(|e| e.to_string())?;
            }
            enc.encode_image(&rgb).map_err(|e| e.to_string())?;
        }
        ImageFormat::Png => {
            let mut enc = PngEncoder::new_with_quality(
                &mut out,
                png_compression(defaults().png_compression),
                image::codecs::png::FilterType::Adaptive,
            );
            if let Some(e) = exif {
                enc.set_exif_metadata(e.to_vec())
                    .map_err(|e| e.to_string())?;
            }
            if let Some(p) = icc {
                enc.set_icc_profile(p.to_vec()).map_err(|e| e.to_string())?;
            }
            img.write_with_encoder(enc).map_err(|e| e.to_string())?;
        }
        ImageFormat::WebP => {
            let rgba = img.to_rgba8();
            let mut enc = WebPEncoder::new_lossless(&mut out);
            if let Some(e) = exif {
                enc.set_exif_metadata(e.to_vec())
                    .map_err(|e| e.to_string())?;
            }
            if let Some(p) = icc {
                enc.set_icc_profile(p.to_vec()).map_err(|e| e.to_string())?;
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
        ImageFormat::Bmp | ImageFormat::Gif | ImageFormat::Tga | ImageFormat::Qoi => {
            img.to_rgba8()
                .write_to(&mut Cursor::new(&mut out), format)
                .map_err(|e| e.to_string())?;
        }
        _ => {
            img.write_to(&mut Cursor::new(&mut out), format)
                .map_err(|e| e.to_string())?;
            if format == ImageFormat::Tiff && (exif.is_some() || icc.is_some() || xmp.is_some()) {
                out = exif::embed_in_tiff(out, exif, icc, xmp, dims);
            }
        }
    }
    if let Some(x) = xmp {
        let add = match format {
            ImageFormat::Jpeg => Some(carry::jpeg_add_xmp as fn(&[u8], &[u8]) -> Option<Vec<u8>>),
            ImageFormat::Png => Some(carry::png_add_xmp as fn(&[u8], &[u8]) -> Option<Vec<u8>>),
            ImageFormat::WebP => Some(carry::webp_add_xmp as fn(&[u8], &[u8]) -> Option<Vec<u8>>),
            _ => None,
        };
        if let Some(add) = add {
            match add(&out, x) {
                Some(with_xmp) => out = with_xmp,
                None => notes.push("XMP packet could not be written; not carried over".into()),
            }
        }
    }
    Ok((out, notes))
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

/// The modification time of `src` when `preserve` (setting `PreserveFileDate`) is on.
fn source_stamp(src: &Path, preserve: bool) -> Result<Option<SystemTime>> {
    if !preserve {
        return Ok(None);
    }
    std::fs::metadata(src)
        .and_then(|m| m.modified())
        .map(Some)
        .map_err(|e| format!("{}: {e}", src.display()))
}

/// Gives `dest` the modification time `stamp`, if any.
fn apply_stamp(dest: &Path, stamp: Option<SystemTime>) -> Result<()> {
    let Some(t) = stamp else { return Ok(()) };
    std::fs::OpenOptions::new()
        .write(true)
        .open(dest)
        .and_then(|f| f.set_modified(t))
        .map_err(|e| format!("{}: cannot keep the original date: {e}", dest.display()))
}

/// [`write_atomic`] to `dest`, with `src`'s modification time when `preserve`.
pub fn write_atomic_like(src: &Path, dest: &Path, bytes: &[u8], preserve: bool) -> Result<()> {
    let stamp = source_stamp(src, preserve)?;
    write_atomic(dest, bytes)?;
    apply_stamp(dest, stamp)
}

/// [`write_new`] next to `src`, with `src`'s modification time when `preserve`.
pub fn write_new_like(
    src: &Path,
    suffix: &str,
    ext: &str,
    bytes: &[u8],
    preserve: bool,
) -> Result<PathBuf> {
    write_new_dated(src, src, suffix, ext, bytes, preserve)
}

/// [`write_new`] named after `named` (a path next to which the output goes), with the
/// modification time of `dated_from` when `preserve`. `batch convert --out DIR` names the
/// output in `DIR` but takes the date from the real source.
pub fn write_new_dated(
    named: &Path,
    dated_from: &Path,
    suffix: &str,
    ext: &str,
    bytes: &[u8],
    preserve: bool,
) -> Result<PathBuf> {
    let stamp = source_stamp(dated_from, preserve)?;
    let out = write_new(named, suffix, ext, bytes)?;
    apply_stamp(&out, stamp)?;
    Ok(out)
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
    /// Notices for the user: the pixels were re-encoded although the input was a JPEG, edge
    /// MCUs were trimmed, data stored after the image or metadata could not be carried over.
    pub notes: Vec<String>,
}

/// Rotates or flips `p`. A JPEG is transformed in the DCT coefficient domain (no
/// generational loss) with its EXIF orientation folded in and reset to 1; its ICC profile
/// and XMP packet stay (XMP orientation and dimensions are updated). When that cannot be
/// done losslessly (a partial edge MCU on a mirrored axis without `trim`, progressive or
/// unusual encodings the transform refuses, damaged data) the file is decoded and
/// re-encoded at quality 95 instead, and a note says why. With `trim` a partial edge MCU
/// row or column on a mirrored axis is dropped instead (like `jpegtran -trim`), so the
/// rotation stays lossless; a note gives the new size. Other formats are decoded,
/// transformed and written as `fallback_ext`. EXIF, ICC and XMP travel with the image when
/// `keep_metadata` is set (a losslessly rotated JPEG is stripped of everything but its ICC
/// profile when it is not).
pub fn rotate_file(
    p: &Path,
    by: &str,
    max_bytes: u64,
    keep_metadata: bool,
    fallback_ext: &str,
    trim: bool,
) -> Result<Rotated> {
    let xf = Transform::by_name(by)
        .ok_or_else(|| format!("bad --by {by} (right|left|180|fliph|flipv)"))?;
    let data = read_input(p, max_bytes)?;
    let mut notes = Vec::new();
    if data.starts_with(&[0xFF, 0xD8]) {
        let stored = exif::from_container(&data)
            .and_then(exif::orientation)
            .map_or(Transform::IDENTITY, Transform::from_exif);
        match jpeg_lossless::transform_reporting(&data, stored.then(xf), trim) {
            Ok(done) => {
                let ext = ext_of(p);
                let ext = if format_for_target(&ext) == Some(ImageFormat::Jpeg) {
                    ext
                } else {
                    "jpg".to_string()
                };
                let bytes = if keep_metadata {
                    done.bytes
                } else {
                    strip::strip(&done.bytes)?
                };
                if done.dropped > 0 {
                    notes.push(format!(
                        "{}: {} bytes stored after the image (vendor extensions such as motion-photo video, depth maps or gain maps) were not carried over, because they describe the unrotated picture",
                        p.display(),
                        done.dropped
                    ));
                }
                if let Some(((fw, fh), (tw, th))) = done.trimmed {
                    notes.push(format!(
                        "{}: trimmed the partial edge MCUs, {fw}x{fh} became {tw}x{th} before turning",
                        p.display()
                    ));
                }
                return Ok(Rotated { bytes, ext, notes });
            }
            Err(why) => {
                notes.push(format!(
                    "{}: not rotated losslessly ({why}); re-encoded at quality {ROTATE_FALLBACK_QUALITY}",
                    p.display()
                ));
            }
        }
    }
    let meta = read_metadata(p, &data, keep_metadata);
    let img = rotate(&decode_bytes(p, data)?, by)?;
    let (bytes, meta_notes) =
        encode_noted(&img, fallback_ext, Some(ROTATE_FALLBACK_QUALITY), &meta)?;
    notes.extend(
        meta_notes
            .into_iter()
            .map(|n| format!("{}: {n}", p.display())),
    );
    Ok(Rotated {
        bytes,
        ext: fallback_ext.to_string(),
        notes,
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

/// JPEG quality-and-scale search for the largest result not exceeding `max_bytes`; the
/// metadata of the source (`meta`) is part of every candidate. When even the smallest
/// candidate with metadata is too big, the search repeats without it and says so. Returns
/// the file and the notes of the metadata that could not be carried.
pub fn compress_to(
    img: &DynamicImage,
    max_bytes: u64,
    meta: &Metadata,
) -> Result<(Vec<u8>, Vec<String>)> {
    if let Some(found) = compress_search(img, max_bytes, meta)? {
        return Ok(found);
    }
    if !meta.is_empty()
        && let Some((bytes, mut notes)) = compress_search(img, max_bytes, &Metadata::default())?
    {
        notes.push(format!(
            "metadata dropped: {max_bytes} bytes is too small to hold the image and its metadata"
        ));
        return Ok((bytes, notes));
    }
    Err(format!("cannot compress below {max_bytes} bytes"))
}

fn compress_search(
    img: &DynamicImage,
    max_bytes: u64,
    meta: &Metadata,
) -> Result<Option<(Vec<u8>, Vec<String>)>> {
    for scale in [100u32, 85, 70, 55, 40, 30, 20] {
        let scaled = if scale == 100 {
            img.clone()
        } else {
            apply_resize(img, Resize::Percent(scale))
        };
        let (mut lo, mut hi, mut best) = (30u8, 95u8, None);
        while lo <= hi {
            let q = lo + (hi - lo) / 2;
            let (bytes, notes) = encode_noted(&scaled, "jpg", Some(q), meta)?;
            if bytes.len() as u64 <= max_bytes {
                best = Some((bytes, notes));
                lo = q + 1;
            } else {
                hi = q - 1;
            }
        }
        if best.is_some() {
            return Ok(best);
        }
    }
    Ok(None)
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
        let out = compress_to(&img, 60 * 1024, &Metadata::default())
            .unwrap()
            .0;
        assert!(out.len() <= 60 * 1024);
        assert!(image::load_from_memory(&out).is_ok());
        assert!(compress_to(&img, 200, &Metadata::default()).is_err());
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
        let r = rotate_file(&src, "right", u64::MAX, true, "jpg", false).unwrap();
        assert!(r.notes.is_empty() && r.ext == "jpg");
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
        let bare = rotate_file(&src, "right", u64::MAX, false, "jpg", false).unwrap();
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
            let r = rotate_file(&path, "right", u64::MAX, true, "jpg", false).unwrap();
            assert!(r.notes.is_empty(), "turn {n}");
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
        let r = rotate_file(&src, "right", u64::MAX, true, "jpg", false).unwrap();
        let note = r.notes.first().expect("a note explains the re-encode");
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
        assert!(rotate_file(&src, "sideways", u64::MAX, true, "jpg", false).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rotate_file_says_when_vendor_data_after_the_image_is_dropped() {
        let dir = temp_dir_for("rot-trailer");
        let src = dir.join("motion.jpg");
        let mut data = camera_jpeg(64, 48, 1);
        data.extend_from_slice(&[0xAA; 5000]);
        std::fs::write(&src, data).unwrap();
        let r = rotate_file(&src, "left", u64::MAX, true, "jpg", false).unwrap();
        let note = r.notes.first().expect("a note names the dropped data");
        assert!(note.contains("5000 bytes stored after the image"), "{note}");
        assert!(r.bytes.ends_with(&[0xFF, 0xD9]));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rotate_file_handles_non_jpeg_input() {
        let dir = temp_dir_for("rot-png");
        let src = dir.join("a.png");
        std::fs::write(&src, encode(&noisy(7, 3), "png", None).unwrap()).unwrap();
        let r = rotate_file(&src, "left", u64::MAX, true, "png", false).unwrap();
        assert!(r.notes.is_empty() && r.ext == "png");
        let img = decode_as("png", &r.bytes);
        assert_eq!((img.width(), img.height()), (3, 7));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn conversion_carries_exif_with_orientation_normalised() {
        let dir = temp_dir_for("exif-carry");
        let src = dir.join("cam.jpg");
        std::fs::write(&src, camera_jpeg(64, 48, 6)).unwrap();
        let (img, meta) = decode_full_with_meta(&src, u64::MAX, true).unwrap();
        assert_eq!(
            (img.width(), img.height()),
            (48, 64),
            "the decoder applied Orientation 6"
        );
        assert!(meta.exif.is_some());
        for ext in ["jpg", "png", "webp", "tiff"] {
            let bytes = encode_with_meta(&img, ext, None, &meta).unwrap();
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
        let plain = encode(&img, "png", None).unwrap();
        assert!(exif::from_container(&plain).is_none());
        for ext in ["bmp", "gif", "qoi", "tga", "ico"] {
            assert!(encode_with_meta(&img, ext, None, &meta).is_ok());
        }
        assert!(
            decode_full_with_meta(&src, u64::MAX, false)
                .unwrap()
                .1
                .is_empty()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A camera JPEG that also carries a 150 kB RGB ICC profile (three `APP2` chunks) and an
    /// XMP packet saying orientation 6 and 640x480.
    fn tagged_jpeg(w: u32, h: u32, orientation: u16, icc: &[u8]) -> Vec<u8> {
        carry::testutil::inject_icc_xmp_into_jpeg(
            &camera_jpeg(w, h, orientation),
            Some(icc),
            Some(&carry::testutil::xmp_packet()),
        )
    }

    fn xmp_text(m: &Metadata) -> String {
        String::from_utf8(m.xmp.clone().expect("XMP carried")).unwrap()
    }

    #[test]
    fn conversion_carries_icc_and_xmp_into_every_container_and_places_them() {
        let dir = temp_dir_for("icc-xmp");
        let src = dir.join("tagged.jpg");
        let icc = carry::testutil::icc_profile(b"RGB ", 150_000);
        std::fs::write(&src, tagged_jpeg(64, 48, 6, &icc)).unwrap();
        let (img, meta) = decode_full_with_meta(&src, u64::MAX, true).unwrap();
        assert_eq!((img.width(), img.height()), (48, 64));
        assert_eq!(meta.icc.as_deref(), Some(&icc[..]));
        for ext in ["jpg", "png", "webp", "tiff"] {
            let (bytes, notes) = encode_noted(&img, ext, None, &meta).unwrap();
            assert!(notes.is_empty(), "{ext}: {notes:?}");
            let back = Metadata::read(ext, &bytes);
            assert_eq!(back.icc.as_deref(), Some(&icc[..]), "{ext}: ICC bytes");
            let x = xmp_text(&back);
            assert!(x.contains(r#"tiff:Orientation="1""#), "{ext}: {x}");
            assert!(x.contains(r#"exif:PixelXDimension="48""#), "{ext}: {x}");
            assert!(
                x.contains("<exif:PixelYDimension>64</exif:PixelYDimension>"),
                "{ext}: {x}"
            );
            assert!(x.contains("<dc:title>Thumbwright test</dc:title>"), "{ext}");
            let block = back.exif.as_deref().expect(ext);
            assert_eq!(exif::summarize(block).make.as_deref(), Some("Thumbcam"));
            let shown = decode_as(ext, &bytes);
            assert_eq!((shown.width(), shown.height()), (48, 64), "{ext}");
        }
        // JPEG placement: JFIF, Exif, three ICC chunks numbered 1..3 of 3, XMP, then the frame header.
        let jpeg = encode_noted(&img, "jpg", None, &meta).unwrap().0;
        let segs = exif::jpeg_segments(&jpeg);
        let kinds: Vec<(u8, &[u8])> = segs
            .iter()
            .map(|(m, p)| (*m, &p[..p.len().min(4)]))
            .collect();
        assert_eq!(kinds[0].0, 0xE0);
        assert_eq!(kinds[1], (0xE1, &b"Exif"[..]));
        for (i, (m, p)) in segs[2..5].iter().enumerate() {
            assert_eq!(
                (*m, &p[..12], p[12], p[13]),
                (0xE2, &b"ICC_PROFILE\0"[..], i as u8 + 1, 3)
            );
        }
        assert_eq!(segs[5].0, 0xE1);
        assert!(segs[5].1.starts_with(b"http://ns.adobe.com/xap/1.0/\0"));
        assert_eq!(segs[6].0, 0xC0, "the frame header follows");
        // PNG: both before the pixel data.
        let png = encode_noted(&img, "png", None, &meta).unwrap().0;
        let types: Vec<[u8; 4]> = exif::png_chunks(&png).map(|(t, _)| t).collect();
        let pos = |t: &[u8; 4]| types.iter().position(|x| x == t).unwrap();
        assert_eq!(pos(b"IHDR"), 0);
        assert!(pos(b"iCCP") < pos(b"IDAT") && pos(b"iTXt") < pos(b"IDAT"));
        // WebP: VP8X (ICC, EXIF, XMP flags), ICCP, image, EXIF, XMP; RIFF size is exact.
        let webp = encode_noted(&img, "webp", None, &meta).unwrap().0;
        let chunks: Vec<([u8; 4], &[u8])> = exif::webp_chunks(&webp).collect();
        let order: Vec<&[u8; 4]> = chunks.iter().map(|(t, _)| t).collect();
        assert_eq!(order, [b"VP8X", b"ICCP", b"VP8L", b"EXIF", b"XMP "]);
        assert_eq!(chunks[0].1[0] & 0x2C, 0x2C, "ICC, EXIF and XMP flags");
        assert_eq!(
            u32::from_le_bytes(webp[4..8].try_into().unwrap()) as usize,
            webp.len() - 8
        );
        // TIFF: tags 34675 and 700 in IFD0.
        let tiff = encode_noted(&img, "tiff", None, &meta).unwrap().0;
        assert_eq!(
            exif::tiff_bytes(&tiff, exif::TAG_ICC_PROFILE),
            Some(&icc[..])
        );
        assert!(exif::tiff_bytes(&tiff, exif::TAG_XMP).is_some());
        // Metadata off: nothing is read, so nothing is written.
        let (_, none) = decode_full_with_meta(&src, u64::MAX, false).unwrap();
        assert!(none.is_empty());
        let bare = encode_noted(&img, "png", None, &none).unwrap().0;
        assert!(Metadata::read("png", &bare).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn email_and_compress_carry_exif_icc_and_xmp() {
        let dir = temp_dir_for("email-compress");
        let src = dir.join("tagged.jpg");
        let icc = carry::testutil::icc_profile(b"RGB ", 3000);
        std::fs::write(&src, tagged_jpeg(96, 64, 6, &icc)).unwrap();
        let (img, meta) = decode_full_with_meta(&src, u64::MAX, true).unwrap();
        // `email`: shrink, JPEG at quality 85.
        let small = apply_resize(&img, Resize::Fit(32, 32));
        let bytes = encode_with_meta(&small, "jpg", Some(85), &meta).unwrap();
        let back = Metadata::read("jpg", &bytes);
        assert_eq!(back.icc.as_deref(), Some(&icc[..]));
        assert_eq!(
            exif::summarize(back.exif.as_deref().unwrap())
                .make
                .as_deref(),
            Some("Thumbcam")
        );
        assert_eq!(exif::orientation(back.exif.as_deref().unwrap()), Some(1));
        let x = xmp_text(&back);
        let (w, h) = (small.width(), small.height());
        assert!(x.contains(&format!(r#"exif:PixelXDimension="{w}""#)), "{x}");
        assert!(x.contains(&format!(">{h}</exif:PixelYDimension>")), "{x}");
        // `compress`: metadata is in the file when it fits.
        let big = noisy(600, 600);
        let (bytes, notes) = compress_to(&big, 60 * 1024, &meta).unwrap();
        assert!(bytes.len() <= 60 * 1024 && notes.is_empty(), "{notes:?}");
        assert_eq!(Metadata::read("jpg", &bytes).icc.as_deref(), Some(&icc[..]));
        // When the profile alone cannot fit the limit, the image wins and the note says so.
        let huge_icc = carry::testutil::icc_profile(b"RGB ", 70_000);
        let heavy = Metadata {
            icc: Some(huge_icc),
            ..meta.clone()
        };
        let (bytes, notes) = compress_to(&big, 60 * 1024, &heavy).unwrap();
        assert!(bytes.len() <= 60 * 1024);
        assert!(
            notes.iter().any(|n| n.contains("metadata dropped")),
            "{notes:?}"
        );
        assert!(Metadata::read("jpg", &bytes).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_profile_that_does_not_describe_the_written_pixels_is_dropped_with_a_note() {
        let dir = temp_dir_for("cmyk-icc");
        let src = dir.join("cmyk.jpg");
        let icc = carry::testutil::icc_profile(b"CMYK", 4000);
        std::fs::write(&src, tagged_jpeg(32, 32, 1, &icc)).unwrap();
        let (img, meta) = decode_full_with_meta(&src, u64::MAX, true).unwrap();
        assert_eq!(meta.icc.as_deref(), Some(&icc[..]), "read as found");
        for ext in ["jpg", "png", "webp", "tiff", "avif", "jxl"] {
            let (bytes, notes) = encode_noted(&img, ext, None, &meta).unwrap();
            assert!(
                notes.iter().any(|n| n.contains("ICC profile")),
                "{ext}: {notes:?}"
            );
            if !matches!(ext, "avif" | "jxl") {
                assert!(Metadata::read(ext, &bytes).icc.is_none(), "{ext}");
            }
        }
        // A grey profile on grey PNG pixels is kept; the same profile for JPEG (written as
        // RGB) is not.
        let grey = DynamicImage::ImageLuma8(image::GrayImage::new(9, 9));
        let gicc = carry::testutil::icc_profile(b"GRAY", 2000);
        let gm = Metadata {
            icc: Some(gicc.clone()),
            ..Metadata::default()
        };
        let png = encode_noted(&grey, "png", None, &gm).unwrap();
        assert!(png.1.is_empty());
        assert_eq!(
            Metadata::read("png", &png.0).icc.as_deref(),
            Some(&gicc[..])
        );
        let jpg = encode_noted(&grey, "jpg", None, &gm).unwrap();
        assert_eq!(jpg.1.len(), 1);
        assert!(Metadata::read("jpg", &jpg.0).icc.is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tiff_is_an_exif_icc_and_xmp_source() {
        let dir = temp_dir_for("tiff-source");
        let src = dir.join("cam.jpg");
        let icc = carry::testutil::icc_profile(b"RGB ", 5000);
        std::fs::write(&src, tagged_jpeg(64, 48, 1, &icc)).unwrap();
        let (img, meta) = decode_full_with_meta(&src, u64::MAX, true).unwrap();
        let tiff_path = dir.join("cam.tif");
        std::fs::write(
            &tiff_path,
            encode_with_meta(&img, "tiff", None, &meta).unwrap(),
        )
        .unwrap();
        let (img2, from_tiff) = decode_full_with_meta(&tiff_path, u64::MAX, true).unwrap();
        assert_eq!((img2.width(), img2.height()), (64, 48));
        let block = from_tiff
            .exif
            .as_deref()
            .expect("EXIF from IFD0, ExifIFD and GPS");
        let s = exif::summarize(block);
        assert_eq!(s.make.as_deref(), Some("Thumbcam"));
        assert_eq!(s.datetime.as_deref(), Some("2023:07:04 12:34:56"));
        assert!(s.gps.is_some());
        assert_eq!(s.dpi, Some((300.0, 300.0)));
        assert!(block.len() < 2000, "a compact block, not the whole TIFF");
        assert_eq!(from_tiff.icc.as_deref(), Some(&icc[..]));
        assert!(xmp_text(&from_tiff).contains(r#"tiff:Orientation="1""#));
        // Into every writer that holds EXIF.
        for ext in ["jpg", "png", "webp", "tiff"] {
            let bytes = encode_with_meta(&img2, ext, None, &from_tiff).unwrap();
            let s = exif::summarize_file(&bytes);
            assert_eq!(s.make.as_deref(), Some("Thumbcam"), "{ext}");
            assert!(s.gps.is_some(), "{ext}");
            assert_eq!(
                Metadata::read(ext, &bytes).icc.as_deref(),
                Some(&icc[..]),
                "{ext}"
            );
        }
        // Only a .tif/.tiff file is a TIFF source: a TIFF-based RAW container is not.
        let raw_like = std::fs::read(&tiff_path).unwrap();
        assert!(Metadata::read("dng", &raw_like).is_empty());
        assert!(Metadata::read("nef", &raw_like).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rotate_trim_keeps_a_partial_mcu_image_lossless_and_carries_metadata() {
        let dir = temp_dir_for("rotate-trim");
        let src = dir.join("odd.jpg");
        let icc = carry::testutil::icc_profile(b"RGB ", 70_000);
        // Stored 50x30 with Orientation 6, turned right: net 180 degrees, both axes mirrored.
        std::fs::write(&src, tagged_jpeg(50, 30, 6, &icc)).unwrap();
        let plain = rotate_file(&src, "right", u64::MAX, true, "jpg", false).unwrap();
        assert!(
            plain.notes[0].contains("not rotated losslessly"),
            "{:?}",
            plain.notes
        );
        // The fallback re-encode also carries the profile and the packet.
        let m = Metadata::read("jpg", &plain.bytes);
        assert_eq!(m.icc.as_deref(), Some(&icc[..]));
        assert!(xmp_text(&m).contains(r#"tiff:Orientation="1""#));

        let r = rotate_file(&src, "right", u64::MAX, true, "jpg", true).unwrap();
        assert_eq!(r.notes.len(), 1, "{:?}", r.notes);
        assert!(
            r.notes[0].contains("trimmed the partial edge MCUs, 50x30 became 48x24"),
            "{:?}",
            r.notes
        );
        let img = decode_as("jpg", &r.bytes);
        assert_eq!((img.width(), img.height()), (48, 24));
        let m = Metadata::read("jpg", &r.bytes);
        assert_eq!(
            m.icc.as_deref(),
            Some(&icc[..]),
            "ICC passes through the lossless path"
        );
        let x = xmp_text(&m);
        assert!(
            x.contains(r#"exif:PixelXDimension="48""#) && x.contains(">24</exif:PixelYDimension>")
        );
        let block = m.exif.as_deref().unwrap();
        assert_eq!(exif::orientation(block), Some(1));
        assert_eq!(exif::summarize(block).make.as_deref(), Some("Thumbcam"));
        // The kept pixels are the source's: the 180 degree turn of its top-left 48x24
        // (stored pixels, before the Orientation 6 the decoder would apply).
        let stored =
            image::load_from_memory_with_format(&std::fs::read(&src).unwrap(), ImageFormat::Jpeg)
                .unwrap()
                .to_rgb8();
        let want = image::imageops::rotate180(
            &image::imageops::crop_imm(&stored, 0, 0, 48, 24).to_image(),
        );
        let got = img.to_rgb8();
        let worst = want
            .pixels()
            .zip(got.pixels())
            .flat_map(|(a, b)| {
                a.0.iter()
                    .zip(b.0)
                    .map(|(x, y)| (i32::from(*x) - i32::from(y)).abs())
            })
            .max()
            .unwrap();
        assert!(worst <= 3, "decoder rounding only, got {worst}");
        // Keep-metadata off with trim: stripped of EXIF/XMP, ICC stays (like `strip`).
        let bare = rotate_file(&src, "right", u64::MAX, false, "jpg", true).unwrap();
        let m = Metadata::read("jpg", &bare.bytes);
        assert!(m.exif.is_none() && m.xmp.is_none());
        assert_eq!(m.icc.as_deref(), Some(&icc[..]));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `tw-worker.exe` next to the test executable, copied from cargo's output directory
    /// (the integration build of the workspace puts it there).
    fn ensure_worker() {
        let dest = exe_dir().join(ids::WORKER_EXE_NAME);
        if dest.is_file() {
            return;
        }
        let built = exe_dir()
            .parent()
            .map(|p| p.join(ids::WORKER_EXE_NAME))
            .filter(|p| p.is_file())
            .expect("tw-worker.exe is not built; run `cargo build --workspace` first");
        std::fs::copy(built, dest).unwrap();
    }

    #[test]
    fn damaged_png_is_a_decode_error_not_a_com_error() {
        ensure_worker();
        let dir = temp_dir_for("damaged-png");
        let mut png = encode(&noisy(32, 32), "png", None).unwrap();
        // Keep the signature and IHDR, wreck the compressed data.
        for b in &mut png[40..] {
            *b = 0x5A;
        }
        let path = dir.join("broken.png");
        std::fs::write(&path, &png).unwrap();
        let err = decode_full(&path, u64::MAX).unwrap_err();
        assert!(
            !err.to_ascii_lowercase().contains("coinitialize") && !err.contains("0x800401F0"),
            "WIC must not run in twctl: {err}"
        );
        assert!(err.contains("broken.png"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn every_worker_placed_decoder_but_svg_is_isolated_in_conversions() {
        use tw_core::decode::route::Placement;
        for d in Decoder::ALL {
            let worker = d.placement() == Placement::Worker && d != Decoder::Svg;
            assert_eq!(isolated_edge(d).is_some(), worker, "{d:?}");
        }
    }

    #[test]
    fn conversion_defaults_follow_the_settings() {
        let s = Settings {
            jpeg_quality: 55,
            png_compression: 0,
            use_embedded_thumbs: false,
            ..Settings::default()
        };
        assert_eq!(
            Defaults::from(&s),
            Defaults {
                jpeg_quality: 55,
                png_compression: 0,
                prefer_embedded: false
            }
        );
        let d = Defaults::from(&Settings::default());
        assert_eq!(
            (d.jpeg_quality, d.png_compression, d.prefer_embedded),
            (90, 9, true)
        );
        use image::codecs::png::CompressionType;
        assert!(matches!(png_compression(0), CompressionType::Uncompressed));
        assert!(matches!(png_compression(6), CompressionType::Level(6)));
        assert!(matches!(png_compression(200), CompressionType::Level(9)));
    }

    #[test]
    fn png_level_changes_the_size_and_round_trips() {
        let img = DynamicImage::ImageRgb8(RgbImage::from_fn(128, 128, |x, y| {
            image::Rgb([(x / 8) as u8 * 16, (y / 8) as u8 * 16, 40])
        }));
        let png = |level| {
            let mut out = Vec::new();
            let enc = PngEncoder::new_with_quality(
                &mut out,
                png_compression(level),
                image::codecs::png::FilterType::Adaptive,
            );
            img.write_with_encoder(enc).unwrap();
            out
        };
        let (stored, best) = (png(0), png(9));
        assert!(
            best.len() < stored.len(),
            "{} vs {}",
            best.len(),
            stored.len()
        );
        assert_eq!(decode_as("png", &stored).to_rgb8(), img.to_rgb8());
        assert_eq!(decode_as("png", &best).to_rgb8(), img.to_rgb8());
    }

    #[test]
    fn preserve_file_date_copies_the_source_mtime_only_when_asked() {
        let dir = temp_dir_for("keep-date");
        let src = dir.join("a.png");
        std::fs::write(&src, b"x").unwrap();
        let old = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&src)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let mtime = |p: &Path| std::fs::metadata(p).unwrap().modified().unwrap();
        let kept = write_new_like(&src, " (a)", "png", b"1", true).unwrap();
        assert_eq!(mtime(&kept), old);
        let fresh = write_new_like(&src, " (b)", "png", b"2", false).unwrap();
        assert_ne!(mtime(&fresh), old);
        let dest = dir.join("b.png");
        write_atomic_like(&src, &dest, b"3", true).unwrap();
        assert_eq!(mtime(&dest), old);
        assert_eq!(std::fs::read(&dest).unwrap(), b"3");
        assert!(write_new_like(&dir.join("missing.png"), "", "png", b"1", true).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
