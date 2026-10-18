//! Worker isolation: budgets, kill-on-timeout and hostile-worker handling, driven through
//! the same `Engine` the DLL uses, against the real `tw-worker.exe`.
#![cfg(windows)]

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tw_core::decode::DecodeError;
use tw_core::pipeline::Engine;

fn worker() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tw-worker"))
}

/// A filter-heavy SVG that takes far longer than the budget used below.
fn slow_svg() -> Vec<u8> {
    let mut s = String::from(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="4000" height="4000"><defs>"#,
    );
    for i in 0..400 {
        s.push_str(&format!(
            r#"<filter id="f{i}"><feGaussianBlur stdDeviation="200"/></filter>"#
        ));
    }
    s.push_str("</defs>");
    for i in 0..400 {
        s.push_str(&format!(
            r#"<rect width="4000" height="4000" fill="red" filter="url(#f{i})"/>"#
        ));
    }
    s.push_str("</svg>");
    s.into_bytes()
}

#[test]
fn over_budget_worker_is_killed_and_reported_as_timeout() {
    // A freshly built executable is scanned by antivirus on its first launch; warm it so
    // the measurement covers the watchdog, as for an installed binary.
    Command::new(worker()).arg("--version").status().unwrap();
    let engine = Engine {
        worker_exe: worker(),
        max_tier_time: Duration::from_millis(300),
        video_percent: 30,
        prefer_embedded: true,
    };
    let start = Instant::now();
    let r = engine.thumbnail(Arc::new(slow_svg()), Some("svg"), 1024);
    assert_eq!(r, Err(DecodeError::Timeout));
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn missing_worker_is_a_clean_failure() {
    let engine = Engine::new(worker().join("nonexistent-worker.exe"));
    let r = engine.thumbnail(
        Arc::new(b"<svg xmlns='http://www.w3.org/2000/svg'/>".to_vec()),
        Some("svg"),
        64,
    );
    assert!(matches!(r, Err(DecodeError::Malformed(_))), "{r:?}");
}

#[test]
fn worker_rejects_garbage_requests() {
    let mut child = Command::new(worker())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"not a request")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
}

#[test]
fn worker_round_trip_through_protocol() {
    let engine = Engine::new(worker());
    let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="20"><rect width="10" height="20" fill="#0000ff"/></svg>"##;
    let t = engine
        .thumbnail(Arc::new(svg.to_vec()), Some("svg"), 40)
        .unwrap();
    assert_eq!((t.width, t.height), (20, 40));
    assert_eq!(&t.bgra[..4], &[255, 0, 0, 255]);
}

fn synchsafe(n: u32) -> [u8; 4] {
    [
        ((n >> 21) & 0x7F) as u8,
        ((n >> 14) & 0x7F) as u8,
        ((n >> 7) & 0x7F) as u8,
        (n & 0x7F) as u8,
    ]
}

/// ID3v2.3 tag holding `body`, followed by silent MPEG-1 layer III frames.
fn mp3(body: &[u8]) -> Vec<u8> {
    let mut f = b"ID3\x03\x00\x00".to_vec();
    f.extend_from_slice(&synchsafe(body.len() as u32));
    f.extend_from_slice(body);
    for _ in 0..4 {
        // 128 kbit/s, 44.1 kHz, no padding: 417-byte frames.
        f.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
        f.extend_from_slice(&[0u8; 413]);
    }
    f
}

#[test]
fn deeply_nested_audio_chapters_fail_without_killing_the_host() {
    // Each CHAP frame is 28 bytes of its own (10-byte header, "c\0" element id, 16 bytes of
    // times/offsets) followed by the next, so the tag parser recurses 200,000 levels deep.
    const DEPTH: u32 = 200_000;
    let mut body = Vec::with_capacity(28 * DEPTH as usize);
    for r in (1..=DEPTH).rev() {
        body.extend_from_slice(b"CHAP");
        body.extend_from_slice(&(28 * r - 10).to_be_bytes());
        body.extend_from_slice(&[0, 0]);
        body.extend_from_slice(b"c\0");
        body.extend_from_slice(&[0u8; 16]);
    }
    let engine = Engine::new(worker());
    let r = engine.thumbnail(Arc::new(mp3(&body)), Some("mp3"), 256);
    assert!(r.is_err(), "{r:?}");
}

#[test]
fn audio_cover_art_is_decoded_through_the_worker() {
    let mut png = std::io::Cursor::new(Vec::new());
    image::RgbaImage::from_pixel(6, 3, image::Rgba([0, 0, 255, 255]))
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let mut apic = vec![0u8]; // ISO-8859-1
    apic.extend_from_slice(b"image/png\0");
    apic.push(3); // front cover
    apic.push(0); // empty description
    apic.extend_from_slice(png.get_ref());
    let mut frame = b"APIC".to_vec();
    frame.extend_from_slice(&(apic.len() as u32).to_be_bytes());
    frame.extend_from_slice(&[0, 0]);
    frame.extend_from_slice(&apic);

    let engine = Engine::new(worker());
    let t = engine
        .thumbnail(Arc::new(mp3(&frame)), Some("mp3"), 256)
        .unwrap();
    assert!(t.width > 0 && t.height > 0);
    assert_eq!(&t.bgra[..4], &[255, 0, 0, 255]);
}

fn riff_chunk(out: &mut Vec<u8>, id: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    if body.len() % 2 == 1 {
        out.push(0);
    }
}

fn riff_list(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(b"LIST");
    out.extend_from_slice(&(body.len() as u32 + 4).to_le_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
}

fn le32(vals: &[u32]) -> Vec<u8> {
    vals.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// Uncompressed 24-bit 64x48 AVI at 10 fps: frame `i` is solid RGB `(i * 10, 0, 200)` in the
/// top half and `(0, 200, 0)` in the bottom half.
fn avi(frames: u32) -> Vec<u8> {
    let (w, h) = (64u32, 48u32);
    let size = w * 3 * h;
    let mut avih = le32(&[100_000, 0, 0, 0x10, frames, 0, 1, size, w, h]);
    avih.extend_from_slice(&[0u8; 16]);
    let mut strh = b"vidsDIB ".to_vec();
    strh.extend_from_slice(&le32(&[0, 0, 0, 1, 10, 0, frames, size, u32::MAX, 0]));
    strh.extend_from_slice(&[0u8; 8]);
    let mut strf = le32(&[40, w, h]);
    strf.extend_from_slice(&1u16.to_le_bytes());
    strf.extend_from_slice(&24u16.to_le_bytes());
    strf.extend_from_slice(&le32(&[0, size, 0, 0, 0, 0]));
    let mut strl = Vec::new();
    riff_chunk(&mut strl, b"strh", &strh);
    riff_chunk(&mut strl, b"strf", &strf);
    let mut hdrl = Vec::new();
    riff_chunk(&mut hdrl, b"avih", &avih);
    riff_list(&mut hdrl, b"strl", &strl);
    let (mut movi, mut index) = (Vec::new(), Vec::new());
    for i in 0..frames {
        let mut data = Vec::new();
        for y in 0..h {
            // Rows are stored bottom-up.
            let (r, g, b) = if y >= h / 2 {
                (i * 10, 0, 200)
            } else {
                (0, 200, 0)
            };
            for _ in 0..w {
                data.extend_from_slice(&[b as u8, g as u8, r as u8]);
            }
        }
        index.extend_from_slice(b"00db");
        index.extend_from_slice(&le32(&[0x10, 4 + movi.len() as u32, size]));
        riff_chunk(&mut movi, b"00db", &data);
    }
    let mut body = Vec::new();
    riff_list(&mut body, b"hdrl", &hdrl);
    riff_list(&mut body, b"movi", &movi);
    riff_chunk(&mut body, b"idx1", &index);
    let mut file = b"RIFF".to_vec();
    file.extend_from_slice(&(body.len() as u32 + 4).to_le_bytes());
    file.extend_from_slice(b"AVI ");
    file.extend_from_slice(&body);
    file
}

fn pixel(t: &tw_core::decode::Thumbnail, x: u32, y: u32) -> [u8; 4] {
    let at = ((y * t.width + x) * 4) as usize;
    [t.bgra[at], t.bgra[at + 1], t.bgra[at + 2], t.bgra[at + 3]]
}

#[test]
fn video_frame_is_decoded_inside_the_job_at_the_configured_offset() {
    let file = Arc::new(avi(20));
    let at = |percent: u8| {
        Engine::new(worker())
            .with_video_percent(percent)
            .thumbnail(file.clone(), Some("avi"), 64)
            .unwrap()
    };
    let first = at(0);
    assert_eq!((first.width, first.height), (64, 48));
    // BGRA: top half is the varying colour (blue 200, red = frame * 10), bottom half green.
    assert_eq!(pixel(&first, 5, 5), [200, 0, 0, 255]);
    assert_eq!(pixel(&first, 5, 42), [0, 200, 0, 255]);
    let last = at(100);
    assert!(pixel(&last, 5, 5)[2] >= 150, "{:?}", pixel(&last, 5, 5));
}

#[test]
fn garbage_video_fails_promptly_without_hanging_the_worker() {
    let start = Instant::now();
    let r = Engine::new(worker()).thumbnail(Arc::new(vec![0x5Au8; 100_000]), Some("mp4"), 64);
    assert!(
        matches!(r, Err(DecodeError::Unsupported | DecodeError::Malformed(_))),
        "{r:?}"
    );
    assert!(
        start.elapsed() < Duration::from_secs(20),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn pdf_first_page_is_rendered_through_the_worker() {
    let content = "1 0 0 rg 0 0 100 200 re f";
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R >>".to_string(),
        format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len()
        ),
    ];
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref = pdf.len();
    pdf.extend_from_slice(b"xref\n0 5\n0000000000 65535 f \n");
    for off in offsets {
        pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!("trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );

    let t = Engine::new(worker())
        .thumbnail(Arc::new(pdf.clone()), Some("pdf"), 50)
        .unwrap();
    assert_eq!((t.width, t.height), (50, 50));
    assert_eq!(pixel(&t, 5, 25), [0, 0, 255, 255]);
    assert_eq!(pixel(&t, 45, 25), [255, 255, 255, 255]);
    // The same bytes under the Illustrator extension take the same route.
    assert!(
        Engine::new(worker())
            .thumbnail(Arc::new(pdf[..pdf.len() / 2].to_vec()), Some("ai"), 50)
            .is_err()
    );
}

#[test]
fn metafile_is_played_back_in_the_worker_on_a_white_page() {
    // Minimal EMF: header (88 bytes) and EMR_EOF, no drawing: the page stays white.
    let mut emf = le32(&[1, 88, 0, 0, 99, 49, 0, 0, 2000, 1000]);
    emf.extend_from_slice(&le32(&[0x464D_4520, 0x0001_0000, 108, 2]));
    emf.extend_from_slice(&1u16.to_le_bytes());
    emf.extend_from_slice(&0u16.to_le_bytes());
    emf.extend_from_slice(&le32(&[0, 0, 0, 1000, 1000, 320, 320]));
    assert_eq!(emf.len(), 88);
    emf.extend_from_slice(&le32(&[14, 20, 0, 16, 20]));

    let t = Engine::new(worker())
        .thumbnail(Arc::new(emf), Some("emf"), 64)
        .unwrap();
    assert_eq!((t.width, t.height), (64, 32));
    assert_eq!(pixel(&t, 10, 10), [255, 255, 255, 255]);
}
