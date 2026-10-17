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
