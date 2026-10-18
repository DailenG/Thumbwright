//! Combine images into a PDF or a CBZ (FUNCTIONAL_SPEC.md 6.4). Hand-written PDF
//! 1.5 writer: one page per image; JPEG sources that a PDF viewer can use as-is are embedded
//! unchanged (DCTDecode), everything else is stored as Flate-compressed RGB (plus a soft
//! mask when it has transparency). The searchable variant adds an invisible OCR text layer.

use std::io::Write;
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::ZlibEncoder;
use image::DynamicImage;

use crate::ocr::{self, Word};
use crate::ops::{self, Result};

/// Pixel-area cap per page; larger images are shrunk first (keeps memory bounded).
const MAX_PAGE_PIXELS: u64 = 100_000_000;
/// PDF user-space units per pixel: images are laid out at 96 dpi.
const PT_PER_PX: f64 = 0.75;
/// PDF viewers reject pages above 14 400 units.
const MAX_PAGE_PT: f64 = 14_400.0;

/// An input that could not be combined.
pub struct Omitted {
    pub input: String,
    pub cause: &'static str,
    pub detail: String,
}

pub struct Combined {
    pub requested: usize,
    pub combined: usize,
    pub omitted: Vec<Omitted>,
}

impl Combined {
    pub fn status(&self) -> &'static str {
        if self.omitted.is_empty() {
            "ok"
        } else {
            "partial"
        }
    }

    /// Spec 6.4 JSON: `{combined, omitted[{input,cause,detail}], output, requested, status}`.
    pub fn json(&self, output: &Path) -> String {
        serde_json::json!({
            "combined": self.combined,
            "omitted": self.omitted.iter().map(|o| serde_json::json!({
                "input": o.input, "cause": o.cause, "detail": o.detail,
            })).collect::<Vec<_>>(),
            "output": output.display().to_string(),
            "requested": self.requested,
            "status": self.status(),
        })
        .to_string()
    }

    pub fn summary(&self, output: &Path) -> String {
        let mut s = format!(
            "{} ({} of {} combined)",
            output.display(),
            self.combined,
            self.requested
        );
        for o in &self.omitted {
            s.push_str(&format!(
                "\nomitted {}: {} ({})",
                o.input, o.cause, o.detail
            ));
        }
        s
    }
}

fn cause_of(detail: &str) -> &'static str {
    if detail.contains("larger than the max file size") {
        "too_large"
    } else if detail.contains("unsupported file type") {
        "unsupported"
    } else if detail.contains("No such file") || detail.contains("cannot find") {
        "missing"
    } else {
        "undecodable"
    }
}

/// Strips the `<path>: ` prefix decode errors carry, leaving the reason.
fn detail_of(input: &Path, err: &str) -> String {
    let prefix = format!("{}: ", input.display());
    err.strip_prefix(&prefix).unwrap_or(err).to_owned()
}

fn pdf_name_obj(n: usize) -> String {
    format!("{n} 0 obj\n")
}

fn zlib(data: &[u8]) -> Result<Vec<u8>> {
    let mut e = ZlibEncoder::new(
        Vec::with_capacity(data.len() / 2 + 64),
        Compression::default(),
    );
    e.write_all(data).map_err(|e| e.to_string())?;
    e.finish().map_err(|e| e.to_string())
}

/// A JPEG a PDF can embed byte-for-byte: baseline or progressive Huffman, 8-bit, one or
/// three components, no Adobe transform marker, no EXIF rotation to honour.
fn jpeg_passthrough(data: &[u8]) -> Option<(u32, u32, u8)> {
    if data.len() < 4 || data[..2] != [0xFF, 0xD8] {
        return None;
    }
    let (mut i, mut dims) = (2usize, None);
    while i + 4 <= data.len() {
        if data[i] != 0xFF {
            return None;
        }
        let marker = data[i + 1];
        if marker == 0xFF {
            i += 1;
            continue;
        }
        if marker == 0xD9 || marker == 0xDA {
            break;
        }
        if (0xD0..=0xD8).contains(&marker) || marker == 0x01 {
            i += 2;
            continue;
        }
        let len = usize::from(u16::from_be_bytes([data[i + 2], data[i + 3]]));
        let seg = data.get(i + 4..i + 2 + len)?;
        match marker {
            0xC0..=0xC2 => {
                if seg.len() < 6 || seg[0] != 8 {
                    return None;
                }
                let h = u32::from(u16::from_be_bytes([seg[1], seg[2]]));
                let w = u32::from(u16::from_be_bytes([seg[3], seg[4]]));
                let comps = seg[5];
                if !(comps == 1 || comps == 3) || w == 0 || h == 0 {
                    return None;
                }
                dims = Some((w, h, comps));
            }
            // Other SOF types (lossless, arithmetic, hierarchical) are not viewer-safe.
            0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => return None,
            // Adobe APP14 can carry an inverted/CMYK transform.
            0xEE if seg.starts_with(b"Adobe") => return None,
            // EXIF with a rotation other than "normal".
            0xE1 if seg.starts_with(b"Exif\0\0") && exif_orientation(seg) > 1 => return None,
            _ => {}
        }
        i += 2 + len;
    }
    dims
}

fn exif_orientation(app1: &[u8]) -> u32 {
    exif::Reader::new()
        .read_raw(app1.get(6..).unwrap_or(&[]).to_vec())
        .ok()
        .and_then(|e| {
            e.get_field(exif::Tag::Orientation, exif::In::PRIMARY)
                .and_then(|f| f.value.get_uint(0))
        })
        .unwrap_or(1)
}

struct Page {
    /// Pixel size of the stored image.
    w: u32,
    h: u32,
    image: PageImage,
    words: Vec<Word>,
}

enum PageImage {
    Jpeg {
        data: Vec<u8>,
        comps: u8,
    },
    Raw {
        rgb: Vec<u8>,
        alpha: Option<Vec<u8>>,
    },
}

/// Built-in Helvetica advance widths (1/1000 em) for 0x20..=0x7E.
const HELVETICA: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556,
    556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584, 556, 1015, 667, 667, 722, 722, 667,
    611, 778, 722, 278, 500, 667, 556, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667,
    667, 611, 278, 278, 278, 469, 556, 333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500,
    222, 833, 556, 556, 556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584,
];

/// WinAnsi bytes for the part of `s` the base-14 font can show; other characters are dropped.
fn win_ansi(s: &str) -> Vec<u8> {
    s.chars()
        .filter_map(|c| match c as u32 {
            0x20..=0x7E | 0xA0..=0xFF => Some(c as u8),
            0x20AC => Some(0x80),
            0x2018 => Some(0x91),
            0x2019 => Some(0x92),
            0x201C => Some(0x93),
            0x201D => Some(0x94),
            0x2013 => Some(0x96),
            0x2014 => Some(0x97),
            _ => None,
        })
        .collect()
}

fn text_width_em(bytes: &[u8]) -> f64 {
    bytes
        .iter()
        .map(|&b| match b {
            0x20..=0x7E => f64::from(HELVETICA[usize::from(b - 0x20)]),
            _ => 556.0,
        })
        .sum::<f64>()
        / 1000.0
}

fn pdf_string(bytes: &[u8]) -> Vec<u8> {
    let mut out = vec![b'('];
    for &b in bytes {
        if matches!(b, b'(' | b')' | b'\\') {
            out.push(b'\\');
        }
        out.push(b);
    }
    out.push(b')');
    out
}

fn load_page(path: &Path, max_bytes: u64, searchable: bool) -> Result<Page> {
    let data = ops::read_input(path, max_bytes)?;
    let ext = ops::ext_of(path);
    let passthrough = matches!(ext.as_str(), "jpg" | "jpeg" | "jpe" | "jfif")
        .then(|| jpeg_passthrough(&data))
        .flatten()
        .filter(|(w, h, _)| u64::from(*w) * u64::from(*h) <= MAX_PAGE_PIXELS);
    // Decode in every case: it validates the input (a truncated JPEG is "undecodable")
    // and feeds OCR.
    let mut img = ops::decode_full(path, max_bytes)?;
    if u64::from(img.width()) * u64::from(img.height()) > MAX_PAGE_PIXELS {
        let f =
            (MAX_PAGE_PIXELS as f64 / (f64::from(img.width()) * f64::from(img.height()))).sqrt();
        img = img.resize_exact(
            ((f64::from(img.width()) * f) as u32).max(1),
            ((f64::from(img.height()) * f) as u32).max(1),
            image::imageops::FilterType::Triangle,
        );
    }
    let words = if searchable {
        ocr::recognize(&img)?.words
    } else {
        Vec::new()
    };
    if let Some((w, h, comps)) = passthrough
        && (w, h) == (img.width(), img.height())
    {
        return Ok(Page {
            w,
            h,
            image: PageImage::Jpeg { data, comps },
            words,
        });
    }
    Ok(Page {
        w: img.width(),
        h: img.height(),
        image: raw_image(&img),
        words,
    })
}

fn raw_image(img: &DynamicImage) -> PageImage {
    let rgba = img.to_rgba8();
    let n = rgba.len() / 4;
    let (mut rgb, mut alpha) = (Vec::with_capacity(n * 3), Vec::with_capacity(n));
    let mut transparent = false;
    for px in rgba.pixels() {
        rgb.extend_from_slice(&px.0[..3]);
        alpha.push(px[3]);
        transparent |= px[3] != 255;
    }
    PageImage::Raw {
        rgb,
        alpha: transparent.then_some(alpha),
    }
}

/// Serialises `pages` as a PDF document. `margin` is in PDF units.
fn write_pdf(pages: &[Page], margin: f64) -> Result<Vec<u8>> {
    let searchable = pages.iter().any(|p| !p.words.is_empty());
    let mut out: Vec<u8> = b"%PDF-1.5\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets: Vec<usize> = Vec::new();
    let mut add = |out: &mut Vec<u8>, body: &[u8]| -> usize {
        offsets.push(out.len());
        let id = offsets.len();
        out.extend_from_slice(pdf_name_obj(id).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
        id
    };
    // Object ids: 1 catalog, 2 pages, 3 font, 4 info; then per page: page, contents, image,
    // optional soft mask.
    let page_count = pages.len();
    let kids_ids: Vec<usize> = {
        // Layout is deterministic, so ids can be precomputed.
        let mut ids = Vec::new();
        let mut next = 5usize;
        for p in pages {
            ids.push(next);
            next += 3 + usize::from(matches!(&p.image, PageImage::Raw { alpha: Some(_), .. }));
        }
        ids
    };
    let kids = kids_ids
        .iter()
        .map(|i| format!("{i} 0 R"))
        .collect::<Vec<_>>()
        .join(" ");
    add(&mut out, b"<< /Type /Catalog /Pages 2 0 R >>");
    add(
        &mut out,
        format!("<< /Type /Pages /Kids [{kids}] /Count {page_count} >>").as_bytes(),
    );
    add(
        &mut out,
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>",
    );
    add(&mut out, b"<< /Producer (Thumbwright) >>");
    for (page, &page_id) in pages.iter().zip(&kids_ids) {
        let has_mask = matches!(&page.image, PageImage::Raw { alpha: Some(_), .. });
        let (cw, ch) = (
            (f64::from(page.w) * PT_PER_PX).min(MAX_PAGE_PT - 2.0 * margin),
            (f64::from(page.h) * PT_PER_PX).min(MAX_PAGE_PT - 2.0 * margin),
        );
        // Shrink to keep the aspect ratio when either edge hit the cap.
        let k = (cw / (f64::from(page.w) * PT_PER_PX)).min(ch / (f64::from(page.h) * PT_PER_PX));
        let (iw, ih) = (
            f64::from(page.w) * PT_PER_PX * k,
            f64::from(page.h) * PT_PER_PX * k,
        );
        let (pw, ph) = (iw + 2.0 * margin, ih + 2.0 * margin);
        let (contents_id, image_id) = (page_id + 1, page_id + 2);
        let font = if searchable {
            "/Font << /F1 3 0 R >> "
        } else {
            ""
        };
        let id = add(
            &mut out,
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {pw:.2} {ph:.2}] \
                 /Resources << {font}/XObject << /Im0 {image_id} 0 R >> >> \
                 /Contents {contents_id} 0 R >>"
            )
            .as_bytes(),
        );
        debug_assert_eq!(id, page_id);

        let mut content = format!("q {iw:.2} 0 0 {ih:.2} {margin:.2} {margin:.2} cm /Im0 Do Q\n");
        if !page.words.is_empty() {
            let (sx, sy) = (iw / f64::from(page.w), ih / f64::from(page.h));
            content.push_str("BT 3 Tr\n");
            let mut bytes = content.into_bytes();
            for w in &page.words {
                let text = win_ansi(&w.text);
                let em = text_width_em(&text);
                if text.is_empty() || em <= 0.0 || w.w <= 0.0 || w.h <= 0.0 {
                    continue;
                }
                let size = (w.h * sy).clamp(1.0, 400.0);
                let tz = (w.w * sx / (em * size) * 100.0).clamp(1.0, 1000.0);
                let (x, y) = (margin + w.x * sx, margin + ih - (w.y + w.h * 0.8) * sy);
                bytes.extend_from_slice(
                    format!("/F1 {size:.2} Tf {tz:.1} Tz 1 0 0 1 {x:.2} {y:.2} Tm ").as_bytes(),
                );
                bytes.extend_from_slice(&pdf_string(&text));
                bytes.extend_from_slice(b" Tj\n");
            }
            bytes.extend_from_slice(b"ET\n");
            content = String::from_utf8(bytes).map_err(|e| e.to_string())?;
        }
        let stream = |dict: &str, body: &[u8]| {
            let mut s = format!("<< {dict} /Length {} >>\nstream\n", body.len()).into_bytes();
            s.extend_from_slice(body);
            s.extend_from_slice(b"\nendstream");
            s
        };
        let z = zlib(content.as_bytes())?;
        add(&mut out, &stream("/Filter /FlateDecode", &z));
        match &page.image {
            PageImage::Jpeg { data, comps } => {
                let cs = if *comps == 1 {
                    "/DeviceGray"
                } else {
                    "/DeviceRGB"
                };
                add(
                    &mut out,
                    &stream(
                        &format!(
                            "/Type /XObject /Subtype /Image /Width {} /Height {} \
                             /ColorSpace {cs} /BitsPerComponent 8 /Filter /DCTDecode",
                            page.w, page.h
                        ),
                        data,
                    ),
                );
            }
            PageImage::Raw { rgb, alpha } => {
                let mask = if has_mask {
                    format!(" /SMask {} 0 R", image_id + 1)
                } else {
                    String::new()
                };
                add(
                    &mut out,
                    &stream(
                        &format!(
                            "/Type /XObject /Subtype /Image /Width {} /Height {} \
                             /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /FlateDecode{mask}",
                            page.w, page.h
                        ),
                        &zlib(rgb)?,
                    ),
                );
                if let Some(a) = alpha {
                    add(
                        &mut out,
                        &stream(
                            &format!(
                                "/Type /XObject /Subtype /Image /Width {} /Height {} \
                                 /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode",
                                page.w, page.h
                            ),
                            &zlib(a)?,
                        ),
                    );
                }
            }
        }
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes(),
    );
    for o in &offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R /Info 4 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            offsets.len() + 1
        )
        .as_bytes(),
    );
    Ok(out)
}

/// Loads every input in order; undecodable ones are collected, or fail the run in strict mode.
fn gather<T>(
    inputs: &[PathBuf],
    strict: bool,
    mut load: impl FnMut(&Path) -> Result<T>,
) -> Result<(Vec<T>, Vec<Omitted>)> {
    let (mut ok, mut omitted) = (Vec::new(), Vec::new());
    for input in inputs {
        match load(input) {
            Ok(v) => ok.push(v),
            Err(e) if strict => return Err(e),
            Err(e) => omitted.push(Omitted {
                input: input.display().to_string(),
                cause: cause_of(&e),
                detail: detail_of(input, &e),
            }),
        }
    }
    if ok.is_empty() {
        let why = omitted.first().map_or_else(
            || "no inputs".into(),
            |o| format!("{}: {}", o.input, o.detail),
        );
        return Err(format!("nothing to combine ({why})"));
    }
    Ok((ok, omitted))
}

/// Builds the PDF document for `inputs` (selection order); the caller writes it.
pub fn build_pdf(
    inputs: &[PathBuf],
    strict: bool,
    searchable: bool,
    margin_pt: f64,
    max_bytes: u64,
) -> Result<(Vec<u8>, Combined)> {
    let (pages, omitted) = gather(inputs, strict, |p| load_page(p, max_bytes, searchable))?;
    let bytes = write_pdf(&pages, margin_pt)?;
    Ok((
        bytes,
        Combined {
            requested: inputs.len(),
            combined: pages.len(),
            omitted,
        },
    ))
}

/// Builds the comic-book archive for `inputs`: images stored as `001.ext`, ...
pub fn build_cbz(inputs: &[PathBuf], strict: bool, max_bytes: u64) -> Result<(Vec<u8>, Combined)> {
    let (entries, omitted) = gather(inputs, strict, |p| {
        let data = ops::read_input(p, max_bytes)?;
        // Validate by decoding, as the PDF path does.
        let img = ops::decode_full(p, max_bytes)?;
        let ext = ops::ext_of(p);
        Ok(
            if crate::web_mime(&ext).is_some() && ext != "svg" && ext != "ico" {
                (ext, data)
            } else {
                ("png".to_owned(), ops::encode(&img, "png", None)?)
            },
        )
    })?;
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let width = entries.len().to_string().len().max(3);
        for (i, (ext, data)) in entries.iter().enumerate() {
            zip.start_file(format!("{:0width$}.{ext}", i + 1), options)
                .map_err(|e| e.to_string())?;
            zip.write_all(data).map_err(|e| e.to_string())?;
        }
        zip.finish().map_err(|e| e.to_string())?;
    }
    Ok((
        buf.into_inner(),
        Combined {
            requested: inputs.len(),
            combined: entries.len(),
            omitted,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg(w: u32, h: u32) -> Vec<u8> {
        ops::encode(&DynamicImage::new_rgb8(w, h), "jpg", None).unwrap()
    }

    #[test]
    fn jpeg_passthrough_accepts_plain_and_rejects_adobe_or_rotated() {
        let plain = jpeg(8, 4);
        assert_eq!(jpeg_passthrough(&plain), Some((8, 4, 3)));
        // Inject an Adobe APP14 marker after SOI.
        let mut adobe = vec![0xFF, 0xD8, 0xFF, 0xEE, 0x00, 0x0E];
        adobe.extend_from_slice(b"Adobe\0\x64\0\0\0\0\0");
        adobe.extend_from_slice(&plain[2..]);
        assert_eq!(jpeg_passthrough(&adobe), None);
        assert_eq!(jpeg_passthrough(&plain[..3]), None);
        assert_eq!(jpeg_passthrough(b"not a jpeg"), None);
    }

    #[test]
    fn pdf_has_one_page_object_per_input_and_valid_xref() {
        let a = Page {
            w: 2,
            h: 2,
            image: raw_image(&DynamicImage::new_rgba8(2, 2)),
            words: Vec::new(),
        };
        let b = Page {
            w: 4,
            h: 3,
            image: PageImage::Jpeg {
                data: jpeg(4, 3),
                comps: 3,
            },
            words: Vec::new(),
        };
        let pdf = write_pdf(&[a, b], 0.0).unwrap();
        let text = String::from_utf8_lossy(&pdf);
        assert!(text.contains("/Count 2"));
        assert!(
            text.contains("/SMask"),
            "transparent page needs a soft mask"
        );
        assert!(text.contains("/DCTDecode"));
        // Every xref offset points at "<n> 0 obj".
        let xref_pos = text.rfind("startxref\n").unwrap();
        let start: usize = text[xref_pos + 10..]
            .lines()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(pdf[start..].starts_with(b"xref\n"));
        let table: Vec<&str> = text[start..]
            .lines()
            .skip(3)
            .take_while(|l| l.ends_with(" n "))
            .collect();
        for (i, line) in table.iter().enumerate() {
            let off: usize = line[..10].parse().unwrap();
            assert!(
                pdf[off..].starts_with(format!("{} 0 obj", i + 1).as_bytes()),
                "obj {}",
                i + 1
            );
        }
    }

    #[test]
    fn combine_omits_bad_inputs_and_strict_fails() {
        let dir = std::env::temp_dir().join(format!("twctl-pdf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        image::RgbImage::new(3, 3).save(dir.join("a.png")).unwrap();
        std::fs::write(dir.join("bad.png"), b"not an image").unwrap();
        let inputs = vec![dir.join("a.png"), dir.join("bad.png")];
        let (bytes, r) = build_cbz(&inputs, false, 1 << 20).unwrap();
        assert_eq!((r.combined, r.requested, r.status()), (1, 2, "partial"));
        assert_eq!(r.omitted[0].cause, "undecodable");
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        assert_eq!(z.len(), 1);
        assert_eq!(z.by_index(0).unwrap().name().unwrap(), "001.png");
        assert!(build_cbz(&inputs, true, 1 << 20).is_err());
        assert!(build_pdf(&inputs, true, false, 0.0, 1 << 20).is_err());
        let (pdf, r) = build_pdf(&inputs, false, false, 0.0, 1 << 20).unwrap();
        assert!(pdf.starts_with(b"%PDF-1.5") && r.combined == 1);
        let json: serde_json::Value = serde_json::from_str(&r.json(Path::new("o.pdf"))).unwrap();
        assert_eq!(json["status"], "partial");
        assert_eq!(json["omitted"][0]["cause"], "undecodable");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
