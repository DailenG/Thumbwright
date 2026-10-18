//! Measurement harness for issue #13; results and decisions are in `docs/performance.md`.
//! Not shipped. Build and run with a release profile:
//!
//! ```text
//! cargo build --release --workspace --examples
//! perf settings [iters]                  Settings::load cost, INI / HKCU / miss
//! perf worker <tw-worker.exe> [iters]    worker spawn + round trip vs in-proc decode
//! perf gen <dir>                         write the ~200-file mixed corpus
//! perf corpus <dir> <tw-worker.exe> [threads] [runs]
//! perf resize [photo.jpg]                24 MP -> 256 px resize candidates
//! perf memory <file-mib>                 peak memory of reading a stream into memory
//! ```

use std::hint::black_box;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};
use tw_core::config::{self, Settings};
use tw_core::decode::route::{self, Decoder, Placement};
use tw_core::decode::to_thumbnail;
use tw_core::pipeline::Engine;
use tw_core::worker;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).map(String::as_str);
    match arg(0) {
        Some("settings") => settings(num(arg(1), 10_000)),
        Some("worker") => worker_bench(Path::new(arg(1).expect("worker exe")), num(arg(2), 60)),
        Some("gen") => gen_corpus(Path::new(arg(1).expect("dir"))),
        Some("corpus") => corpus(
            Path::new(arg(1).expect("dir")),
            Path::new(arg(2).expect("worker exe")),
            num(arg(3), 1),
            num(arg(4), 3),
        ),
        Some("resize") => resize_bench(arg(1).map(Path::new)),
        Some("memory") => memory(num(arg(1), 256)),
        Some("calls") => calls(num(arg(1), 300)),
        Some("anatomy") => anatomy(Path::new(arg(1).expect("worker exe")), num(arg(2), 40)),
        Some("memory-one") => {
            memory_one(arg(1).expect("variant"), Path::new(arg(2).expect("file")))
        }
        _ => eprintln!("usage: see the header of crates/tw-core/examples/perf.rs"),
    }
}

fn num(s: Option<&str>, default: usize) -> usize {
    s.and_then(|s| s.parse().ok()).unwrap_or(default)
}

// ---------------------------------------------------------------- statistics

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn us(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

struct Stats {
    median: Duration,
    p95: Duration,
    mean: Duration,
    min: Duration,
}

fn stats(mut v: Vec<Duration>) -> Stats {
    v.sort();
    let n = v.len();
    Stats {
        median: v[n / 2],
        p95: v[(n * 95 / 100).min(n - 1)],
        mean: v.iter().sum::<Duration>() / n as u32,
        min: v[0],
    }
}

fn time_each(iters: usize, mut f: impl FnMut()) -> Vec<Duration> {
    for _ in 0..(iters / 10).max(1) {
        f();
    }
    (0..iters)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed()
        })
        .collect()
}

// ------------------------------------------------------------------ settings

const INI_BODY: &str = "; Thumbwright settings\r\n[Settings]\r\nThumbnailsEnabled=1\r\nMaxFileMB=2048\r\n\
MaxThumbPx=512\r\nMenuEnabled=1\r\nMenuAllFileTypes=0\r\nMenuPreview=1\r\n\
DisabledTypes=bmp,gif,ico,tga,pcx,sgi,qoi,hdr,exr,dds,psd,kra,ora,epub,cbz\r\n\
HiddenMenuItems=tw.wallpaper,tw.strip,tw.rotate.left,tw.rotate.right\r\nVerboseLog=0\r\n";

fn settings(iters: usize) {
    println!("Settings::load, {iters} calls each (microseconds)");
    let dir = std::env::temp_dir().join(format!("tw-perf-ini-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(config::portable_ini(&dir), INI_BODY).unwrap();
    let ini = stats(time_each(iters, || {
        black_box(Settings::load(black_box(&dir)));
    }));
    row_us("INI (portable, 9 keys)", &ini);

    let from_text = stats(time_each(iters, || {
        black_box(Settings::from_ini(black_box(INI_BODY)));
    }));
    row_us("  of which from_ini parse only", &from_text);
    let ini_path = config::portable_ini(&dir);
    let stat = stats(time_each(iters, || {
        black_box(
            std::fs::metadata(black_box(&ini_path))
                .unwrap()
                .modified()
                .unwrap(),
        );
    }));
    row_us("  INI staleness check: fs::metadata", &stat);

    let sub = format!("Software\\Thumbwright-perf-{}", std::process::id());
    let key = windows_registry::CURRENT_USER.create(&sub).unwrap();
    for line in INI_BODY.lines().skip(2) {
        let (k, v) = line.split_once('=').unwrap();
        match v.parse::<u32>() {
            Ok(n) => key.set_u32(k, n).unwrap(),
            Err(_) => key.set_string(k, v).unwrap(),
        }
    }
    let reg = stats(time_each(iters, || {
        black_box(Settings::load_registry(black_box(&sub)));
    }));
    row_us("HKCU (installed, 9 values)", &reg);
    let (open_query, cached_query) = registry_stamp_cost(&sub, iters);
    row_us("  HKCU stamp: open+query+close", &open_query);
    row_us("  HKCU stamp: query on held handle", &cached_query);
    windows_registry::CURRENT_USER.remove_tree(&sub).unwrap();

    let empty = dir.join("none");
    std::fs::create_dir_all(&empty).unwrap();
    let miss = stats(time_each(iters, || {
        black_box(Settings::load(black_box(&empty)));
    }));
    row_us("no INI, key absent (defaults)", &miss);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Cost of detecting a registry change via the key's last-write time: reopening the key
/// each time vs querying a handle that is kept open.
fn registry_stamp_cost(subkey: &str, iters: usize) -> (Stats, Stats) {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, RegCloseKey, RegOpenKeyExW, RegQueryInfoKeyW,
    };
    use windows::core::HSTRING;
    let name = HSTRING::from(subkey);
    let query = |key: HKEY| {
        let mut ft = FILETIME::default();
        // SAFETY: valid key handle and out-parameter; every other pointer is optional.
        unsafe {
            RegQueryInfoKeyW(
                key,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(&mut ft),
            )
            .ok()
            .unwrap();
        }
        black_box(ft.dwLowDateTime);
    };
    let reopen = stats(time_each(iters, || {
        let mut key = HKEY::default();
        // SAFETY: handle is closed below.
        unsafe {
            RegOpenKeyExW(HKEY_CURRENT_USER, &name, None, KEY_QUERY_VALUE, &mut key)
                .ok()
                .unwrap();
        }
        query(key);
        // SAFETY: closing the handle opened above.
        unsafe { RegCloseKey(key).ok().unwrap() };
    }));
    let mut held = HKEY::default();
    // SAFETY: handle is closed below.
    unsafe {
        RegOpenKeyExW(HKEY_CURRENT_USER, &name, None, KEY_QUERY_VALUE, &mut held)
            .ok()
            .unwrap();
    }
    let cached = stats(time_each(iters, || query(held)));
    // SAFETY: closing the handle opened above.
    unsafe { RegCloseKey(held).ok().unwrap() };
    (reopen, cached)
}

fn row_us(label: &str, s: &Stats) {
    println!(
        "  {label:<34} median {:>8.1}  p95 {:>8.1}  mean {:>8.1}  min {:>8.1}",
        us(s.median),
        us(s.p95),
        us(s.mean),
        us(s.min)
    );
}

fn row_ms(label: &str, s: &Stats) {
    println!(
        "  {label:<34} median {:>8.2}  p95 {:>8.2}  mean {:>8.2}  min {:>8.2}",
        ms(s.median),
        ms(s.p95),
        ms(s.mean),
        ms(s.min)
    );
}

// ------------------------------------------------------------------- inputs

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

/// Photo-like test image: smooth colour field, textured regions, hard edges and noise.
fn photo(w: u32, h: u32, seed: u64) -> RgbImage {
    let cols: Vec<f32> = (0..w)
        .map(|x| (x as f32 / w as f32 * 12.57).sin())
        .collect();
    let rows: Vec<f32> = (0..h).map(|y| (y as f32 / h as f32 * 9.42).cos()).collect();
    RgbImage::from_fn(w, h, |x, y| {
        let (c, r) = (cols[x as usize], rows[y as usize]);
        let mut hsh = (u64::from(x) * 0x9E37_79B9) ^ (u64::from(y) * 0x85EB_CA6B) ^ seed;
        hsh ^= hsh >> 13;
        hsh = hsh.wrapping_mul(0xC2B2_AE35);
        let noise = (hsh >> 8 & 15) as f32 - 8.0;
        let mut v = [
            128.0 + 90.0 * c + noise,
            128.0 + 90.0 * r + noise,
            128.0 + 60.0 * c * r + noise,
        ];
        if (x / 96 + y / 96) % 5 == 0 && (x + y) % 3 == 0 {
            v = [235.0 - v[0] * 0.5, 20.0 + v[1] * 0.3, 240.0 - v[2] * 0.4];
        }
        Rgb(v.map(|c| c.clamp(0.0, 255.0) as u8))
    })
}

fn jpeg(img: &RgbImage, quality: u8) -> Vec<u8> {
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, quality)
        .encode_image(img)
        .unwrap();
    out
}

fn encode(img: &DynamicImage, fmt: ImageFormat) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, fmt).unwrap();
    out.into_inner()
}

fn svg(shapes: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed | 1);
    let mut s = String::from(
        "<svg xmlns='http://www.w3.org/2000/svg' width='800' height='600' viewBox='0 0 800 600'>\
<defs><linearGradient id='g' x1='0' y1='0' x2='1' y2='1'><stop offset='0' stop-color='#fc0'/>\
<stop offset='1' stop-color='#06c'/></linearGradient></defs><rect width='800' height='600' fill='url(#g)'/>",
    );
    for i in 0..shapes {
        let (x, y, r) = (rng.next() % 800, rng.next() % 600, 5 + rng.next() % 60);
        let col = rng.next() & 0xFF_FFFF;
        if i % 2 == 0 {
            s += &format!(
                "<circle cx='{x}' cy='{y}' r='{r}' fill='#{col:06x}' fill-opacity='0.6'/>"
            );
        } else {
            s += &format!(
                "<path d='M{x} {y} q{r} -{r} {} {r} t-{r} {r}z' fill='none' stroke='#{col:06x}' \
stroke-width='3'/>",
                r * 2
            );
        }
    }
    s += "</svg>";
    s.into_bytes()
}

fn synchsafe(n: u32) -> [u8; 4] {
    [
        ((n >> 21) & 0x7F) as u8,
        ((n >> 14) & 0x7F) as u8,
        ((n >> 7) & 0x7F) as u8,
        (n & 0x7F) as u8,
    ]
}

/// ID3v2.3 tag with a front-cover APIC frame, then silent MPEG-1 layer III frames.
fn mp3(cover_png: &[u8]) -> Vec<u8> {
    let mut body = vec![0u8];
    body.extend_from_slice(b"image/png\0");
    body.extend_from_slice(&[3, 0]);
    body.extend_from_slice(cover_png);
    let mut frame = b"APIC".to_vec();
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&[0, 0]);
    frame.extend_from_slice(&body);
    let mut f = b"ID3\x03\x00\x00".to_vec();
    f.extend_from_slice(&synchsafe(frame.len() as u32));
    f.extend_from_slice(&frame);
    for _ in 0..40 {
        f.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
        f.extend_from_slice(&[0u8; 413]);
    }
    f
}

fn kra(preview_png: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
    z.start_file("mergedimage.png", zip::write::SimpleFileOptions::default())
        .unwrap();
    z.write_all(preview_png).unwrap();
    z.finish().unwrap().into_inner()
}

// -------------------------------------------------------------------- worker

fn worker_bench(exe: &Path, iters: usize) {
    // SAFETY: single-threaded COM init for the in-proc WIC comparison.
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        );
    }
    let edge = 256;
    let photo_12mp = jpeg(&photo(4000, 3000, 1), 85);
    let photo_xga = jpeg(&photo(1024, 768, 2), 85);
    let cover = encode(
        &DynamicImage::ImageRgb8(photo(300, 300, 3)),
        ImageFormat::Png,
    );
    let cases: Vec<(&str, Decoder, &str, Vec<u8>)> = vec![
        ("svg 1 KB", Decoder::Svg, "svg", svg(3, 1)),
        ("svg 60 KB", Decoder::Svg, "svg", svg(600, 2)),
        (
            "wic (JPEG via WIC) 1024x768",
            Decoder::Wic,
            "heic",
            photo_xga,
        ),
        ("wic (JPEG via WIC) 12 MP", Decoder::Wic, "heic", photo_12mp),
        (
            "audio mp3 + 300x300 cover",
            Decoder::Audio,
            "mp3",
            mp3(&cover),
        ),
        (
            "empty request (parse fails)",
            Decoder::Svg,
            "svg",
            b"x".to_vec(),
        ),
    ];

    println!("worker spawn + round trip vs in-proc decode, {iters} warm calls each (milliseconds)");
    let floor = stats(time_each(iters, || {
        let st = std::process::Command::new(exe)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(st.success());
    }));
    row_ms("tw-worker --version (spawn floor)", &floor);

    for (name, decoder, ext, data) in cases {
        println!("{name} ({} bytes)", data.len());
        let data = Arc::new(data);
        let t = Instant::now();
        let first = worker::run(
            exe,
            decoder,
            ext,
            data.clone(),
            edge,
            tw_core::limits::DEFAULT_VIDEO_PERCENT,
            Duration::from_secs(60),
        );
        let cold = t.elapsed();
        println!(
            "  worker first call {:>8.2} ms ({})",
            ms(cold),
            first.map_or_else(|e| e.to_string(), |t| format!("{}x{}", t.width, t.height))
        );
        let w = stats(time_each(iters, || {
            let _ = black_box(worker::run(
                exe,
                decoder,
                ext,
                data.clone(),
                edge,
                tw_core::limits::DEFAULT_VIDEO_PERCENT,
                Duration::from_secs(60),
            ));
        }));
        row_ms("worker::run, one-shot (warm)", &w);
        worker::set_prespawn(true);
        let _ = worker::run(
            exe,
            decoder,
            ext,
            data.clone(),
            edge,
            tw_core::limits::DEFAULT_VIDEO_PERCENT,
            Duration::from_secs(60),
        );
        let back_to_back = stats(time_each(iters, || {
            let _ = black_box(worker::run(
                exe,
                decoder,
                ext,
                data.clone(),
                edge,
                tw_core::limits::DEFAULT_VIDEO_PERCENT,
                Duration::from_secs(60),
            ));
        }));
        row_ms("worker::run, pre-spawned, back to back", &back_to_back);
        let spaced = stats(
            (0..iters.min(40))
                .map(|_| {
                    std::thread::sleep(Duration::from_millis(300));
                    let t = Instant::now();
                    let _ = black_box(worker::run(
                        exe,
                        decoder,
                        ext,
                        data.clone(),
                        edge,
                        tw_core::limits::DEFAULT_VIDEO_PERCENT,
                        Duration::from_secs(60),
                    ));
                    t.elapsed()
                })
                .collect(),
        );
        row_ms("worker::run, pre-spawned, 300 ms apart", &spaced);
        worker::set_prespawn(false);
        let p = stats(time_each(iters, || {
            let _ = black_box(
                decoder
                    .run(&data, ext, edge)
                    .map(|img| to_thumbnail(&img, edge)),
            );
        }));
        row_ms("same decoder in this process", &p);
        println!(
            "  process overhead (median)          {:>8.2} ms",
            ms(w.median.saturating_sub(p.median))
        );
    }
}

/// Where the per-request time goes: `Command::spawn` alone, the whole cold request, and the
/// request latency of a worker that was spawned 400 ms earlier (the best a pre-spawned
/// single-use worker could do). Variants are interleaved so background load hits them alike.
fn anatomy(exe: &Path, iters: usize) {
    use std::io::BufReader;
    use std::process::{Command, Stdio};
    use tw_core::protocol;
    let edge = 256;
    let cover = encode(
        &DynamicImage::ImageRgb8(photo(300, 300, 3)),
        ImageFormat::Png,
    );
    let cases: Vec<(&str, Decoder, &str, Vec<u8>)> = vec![
        ("svg 1 KB", Decoder::Svg, "svg", svg(3, 1)),
        ("audio mp3 + cover", Decoder::Audio, "mp3", mp3(&cover)),
        (
            "wic 1024x768 JPEG",
            Decoder::Wic,
            "heic",
            jpeg(&photo(1024, 768, 2), 85),
        ),
    ];
    let spawn = || {
        Command::new(exe)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    };
    let round_trip = |child: &mut std::process::Child, d: Decoder, ext: &str, data: &[u8]| {
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let writer = std::thread::scope(|s| {
            s.spawn(|| {
                protocol::write_request(
                    &mut stdin,
                    d,
                    edge,
                    tw_core::limits::DEFAULT_VIDEO_PERCENT,
                    ext,
                    data,
                )
            });
            protocol::read_response(&mut stdout, edge)
        });
        writer.unwrap().unwrap();
    };
    println!("worker request anatomy, {iters} interleaved rounds (milliseconds)");
    for (name, d, ext, data) in &cases {
        let (mut spawn_only, mut cold, mut warm) = (vec![], vec![], vec![]);
        for _ in 0..iters {
            let t = Instant::now();
            let mut c = spawn();
            spawn_only.push(t.elapsed());
            round_trip(&mut c, *d, ext, data);
            cold.push(t.elapsed());
            c.wait().unwrap();

            let mut c = spawn();
            std::thread::sleep(Duration::from_millis(400));
            let t = Instant::now();
            round_trip(&mut c, *d, ext, data);
            warm.push(t.elapsed());
            c.wait().unwrap();
        }
        println!("{name}");
        row_ms("Command::spawn returns", &stats(spawn_only));
        row_ms("cold: spawn + request + response", &stats(cold));
        row_ms("warm: request + response only", &stats(warm));
    }
}

// -------------------------------------------------------------------- corpus

fn gen_corpus(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let mut n = 0usize;
    let mut put = |name: String, data: Vec<u8>| {
        std::fs::write(dir.join(name), data).unwrap();
        n += 1;
    };
    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    let sizes = [
        (640, 480),
        (1024, 768),
        (1600, 1200),
        (3000, 2000),
        (4000, 3000),
    ];
    for i in 0..56 {
        let (w, h) = sizes[i % sizes.len()];
        put(
            format!("jpg_{i:03}.jpg"),
            jpeg(&photo(w, h, i as u64), 80 + (i % 15) as u8),
        );
    }
    for i in 0..3 {
        put(
            format!("jpg_24mp_{i}.jpg"),
            jpeg(&photo(6000, 4000, 100 + i), 85),
        );
    }
    for i in 0..40 {
        let (w, h) = sizes[i % 3];
        let mut img = DynamicImage::ImageRgb8(photo(w, h, 200 + i as u64)).to_rgba8();
        for (x, y, p) in img.enumerate_pixels_mut() {
            if (x / 64 + y / 64) % 4 == 0 {
                *p = Rgba([p[0], p[1], p[2], (x * 255 / w) as u8]);
            }
        }
        put(
            format!("png_{i:03}.png"),
            encode(&DynamicImage::ImageRgba8(img), ImageFormat::Png),
        );
    }
    for (ext, fmt, count) in [
        ("bmp", ImageFormat::Bmp, 12),
        ("gif", ImageFormat::Gif, 10),
        ("webp", ImageFormat::WebP, 8),
        ("tga", ImageFormat::Tga, 6),
        ("qoi", ImageFormat::Qoi, 5),
        ("tif", ImageFormat::Tiff, 5),
    ] {
        for i in 0..count {
            let img = DynamicImage::ImageRgba8(
                DynamicImage::ImageRgb8(photo(800 + 40 * i, 600, 300 + u64::from(i))).to_rgba8(),
            );
            let img = if fmt == ImageFormat::Gif || fmt == ImageFormat::Bmp {
                DynamicImage::ImageRgb8(img.to_rgb8())
            } else {
                img
            };
            put(format!("{ext}_{i:03}.{ext}"), encode(&img, fmt));
        }
    }
    for i in 0..15 {
        put(format!("svg_{i:03}.svg"), svg(5 + i * 25, 400 + i as u64));
    }
    let cover = encode(
        &DynamicImage::ImageRgb8(photo(300, 300, 7)),
        ImageFormat::Png,
    );
    for i in 0..15 {
        put(format!("mp3_{i:03}.mp3"), mp3(&cover));
    }
    for i in 0..8 {
        put(
            format!("kra_{i:03}.kra"),
            kra(&encode(
                &DynamicImage::ImageRgb8(photo(1200, 900, 500 + i)),
                ImageFormat::Png,
            )),
        );
    }
    // JPEG content under a HEIC name: routed straight to the WIC worker.
    for i in 0..6 {
        put(
            format!("heic_{i:03}.heic"),
            jpeg(&photo(2000, 1500, 600 + i), 85),
        );
    }
    // Corrupt rasters: the in-proc decoder fails and the chain falls through to WIC.
    for i in 0..5 {
        let mut junk: Vec<u8> = (0..4096).map(|_| rng.next() as u8).collect();
        junk[..3].copy_from_slice(&[0xFF, 0xD8, 0xFF]);
        put(format!("bad_{i:03}.jpg"), junk);
    }
    println!("wrote {n} files to {}", dir.display());
}

struct Item {
    path: PathBuf,
    ext: String,
    kind: String,
    uses_worker: bool,
}

fn corpus(dir: &Path, worker_exe: &Path, threads: usize, runs: usize) {
    let mut items: Vec<Item> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_file())
        .map(|path| {
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            let data = std::fs::read(&path).unwrap();
            let chain = route::plan(Some(&ext), &data[..data.len().min(4096)]);
            let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
            Item {
                kind: stem[..stem.len().min(3)].to_owned(),
                // Raster files list WIC as a fall-through tier only; corrupt ones reach it.
                uses_worker: chain
                    .first()
                    .is_some_and(|d| d.placement() == Placement::Worker)
                    || stem.starts_with("bad_"),
                path,
                ext,
            }
        })
        .collect();
    // A deterministic shuffle: a real folder interleaves formats, which matters when a
    // worker request is followed by an in-process one.
    items.sort_by_key(|i| {
        i.path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .bytes()
            .fold(0xCBF2_9CE4_8422_2325u64, |h, b| {
                (h ^ u64::from(b)).wrapping_mul(0x100_0000_01B3)
            })
    });
    let engine = Arc::new(Engine::new(worker_exe.to_path_buf()));
    println!(
        "{} files, {threads} thread(s); per file = read + Engine::thumbnail (edge 256); \
{runs} interleaved run(s) per mode (the first pair is cold)",
        items.len()
    );
    let items = Arc::new(items);
    let modes = [("one-shot workers", false), ("pre-spawned workers", true)];
    let mut walls: Vec<Vec<Duration>> = vec![Vec::new(); modes.len()];
    let mut last: Vec<Vec<Duration>> = vec![Vec::new(); modes.len()];
    for run in 0..runs {
        for (m, (label, prespawn)) in modes.iter().enumerate() {
            worker::set_prespawn(*prespawn);
            let next = Arc::new(AtomicUsize::new(0));
            let t0 = Instant::now();
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    let (items, engine, next) = (items.clone(), engine.clone(), next.clone());
                    std::thread::spawn(move || {
                        let mut out = Vec::new();
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            let Some(item) = items.get(i) else { break };
                            let t = Instant::now();
                            let data = std::fs::read(&item.path).unwrap();
                            let r = engine.thumbnail(Arc::new(data), Some(&item.ext), 256);
                            out.push((i, t.elapsed(), r.is_ok()));
                        }
                        out
                    })
                })
                .collect();
            let mut times = vec![Duration::ZERO; items.len()];
            let mut failed = 0;
            for h in handles {
                for (i, d, ok) in h.join().unwrap() {
                    times[i] = d;
                    failed += usize::from(!ok);
                }
            }
            let wall = t0.elapsed();
            println!(
                "run {} {:<20} wall {:>7.0} ms, failures {failed} (the 5 bad_* are expected)",
                run + 1,
                label,
                ms(wall)
            );
            walls[m].push(wall);
            last[m] = times;
        }
    }
    worker::set_prespawn(false);
    for (m, (label, _)) in modes.iter().enumerate() {
        let warm = &walls[m][walls[m].len().min(1)..];
        let warm = if warm.is_empty() { &walls[m][..] } else { warm };
        let s = stats(warm.to_vec());
        println!(
            "{label:<20} warm wall: min {:.0} ms, median {:.0} ms",
            ms(s.min),
            ms(s.median)
        );
    }
    let mut kinds: Vec<&str> = items.iter().map(|i| i.kind.as_str()).collect();
    kinds.sort();
    kinds.dedup();
    println!(
        "per kind, last run (ms): kind count worker-first | one-shot sum median | pre-spawned sum median"
    );
    let (mut worker_sum, mut inproc_sum) = ([Duration::ZERO; 2], [Duration::ZERO; 2]);
    for kind in kinds {
        let worker_kind = items.iter().any(|i| i.kind == kind && i.uses_worker);
        let mut cols = Vec::new();
        let mut count = 0;
        for (m, times) in last.iter().enumerate() {
            let sel: Vec<Duration> = items
                .iter()
                .zip(times)
                .filter(|(i, _)| i.kind == kind)
                .map(|(_, t)| *t)
                .collect();
            count = sel.len();
            let sum: Duration = sel.iter().sum();
            if worker_kind {
                worker_sum[m] += sum;
            } else {
                inproc_sum[m] += sum;
            }
            cols.push(format!("{:>8.1} {:>7.2}", ms(sum), ms(stats(sel).median)));
        }
        println!(
            "  {kind:<4} {count:>3} {:<4} | {} | {}",
            if worker_kind { "yes" } else { "no" },
            cols[0],
            cols[1]
        );
    }
    println!(
        "  sum over worker-first kinds (ms): one-shot {:.0}, pre-spawned {:.0}; in-proc kinds: {:.0} / {:.0}",
        ms(worker_sum[0]),
        ms(worker_sum[1]),
        ms(inproc_sum[0]),
        ms(inproc_sum[1])
    );
}

// -------------------------------------------------------------------- resize

fn resize_dims(w: u32, h: u32, edge: u32) -> (u32, u32) {
    let ratio = f64::from(edge) / f64::from(w.max(h));
    (
        ((f64::from(w) * ratio).round() as u32).max(1),
        ((f64::from(h) * ratio).round() as u32).max(1),
    )
}

/// Premultiplied BGRA from straight RGBA, as the shell expects.
fn to_bgra_premul(mut px: Vec<u8>) -> Vec<u8> {
    for p in px.as_chunks_mut::<4>().0 {
        let a = u16::from(p[3]);
        let m = |c: u8| ((u16::from(c) * a + 127) / 255) as u8;
        let (b, g, r) = (m(p[2]), m(p[1]), m(p[0]));
        p[..3].copy_from_slice(&[b, g, r]);
    }
    px
}

fn premultiply_in_place(img: &mut RgbaImage) {
    for p in img.pixels_mut() {
        let a = u16::from(p[3]);
        for c in &mut p.0[..3] {
            *c = ((u16::from(*c) * a + 127) / 255) as u8;
        }
    }
}

fn bgra_from_premul_rgba(img: RgbaImage) -> Vec<u8> {
    let mut v = img.into_raw();
    for p in v.as_chunks_mut::<4>().0 {
        p.swap(0, 2);
    }
    v
}

type Candidate = (&'static str, fn(&DynamicImage, u32) -> (u32, u32, Vec<u8>));

/// What `to_thumbnail` did before issue #13: `DynamicImage::thumbnail`, then premultiply.
fn cand_current(img: &DynamicImage, edge: u32) -> (u32, u32, Vec<u8>) {
    let resized;
    let img = if img.width() > edge || img.height() > edge {
        resized = img.thumbnail(edge, edge);
        &resized
    } else {
        img
    };
    let small = img.to_rgba8();
    (
        small.width(),
        small.height(),
        to_bgra_premul(small.into_raw()),
    )
}

/// `to_thumbnail` as it ships now.
fn cand_shipped(img: &DynamicImage, edge: u32) -> (u32, u32, Vec<u8>) {
    let t = to_thumbnail(img, edge);
    (t.width, t.height, t.bgra)
}

fn imageops_premul(
    img: &DynamicImage,
    edge: u32,
    f: image::imageops::FilterType,
) -> (u32, u32, Vec<u8>) {
    let (w, h) = resize_dims(img.width(), img.height(), edge);
    let mut src = img.to_rgba8();
    premultiply_in_place(&mut src);
    let small = image::imageops::resize(&src, w, h, f);
    (w, h, bgra_from_premul_rgba(small))
}

fn cand_catmull_premul(img: &DynamicImage, edge: u32) -> (u32, u32, Vec<u8>) {
    imageops_premul(img, edge, image::imageops::FilterType::CatmullRom)
}

/// The current box filter, but averaging premultiplied colour when the image has alpha.
fn cand_premul_thumbnail(img: &DynamicImage, edge: u32) -> (u32, u32, Vec<u8>) {
    if !img.color().has_alpha() {
        return cand_current(img, edge);
    }
    let mut rgba = img.to_rgba8();
    premultiply_in_place(&mut rgba);
    let small = DynamicImage::ImageRgba8(rgba)
        .thumbnail(edge, edge)
        .to_rgba8();
    (small.width(), small.height(), bgra_from_premul_rgba(small))
}

fn fir(img: &DynamicImage, edge: u32, alg: fast_image_resize::ResizeAlg) -> (u32, u32, Vec<u8>) {
    use fast_image_resize::images::{Image, ImageRef};
    use fast_image_resize::{PixelType, ResizeOptions, Resizer};
    let (w, h) = resize_dims(img.width(), img.height(), edge);
    let opts = ResizeOptions::new().resize_alg(alg);
    let mut resizer = Resizer::new();
    // Borrow the pixels when the decoder already produced 8-bit RGB or RGBA.
    let rgba = match img {
        DynamicImage::ImageRgba8(src) => {
            let src =
                ImageRef::new(src.width(), src.height(), src.as_raw(), PixelType::U8x4).unwrap();
            let mut dst = Image::new(w, h, PixelType::U8x4);
            resizer.resize(&src, &mut dst, &opts).unwrap();
            dst.into_vec()
        }
        DynamicImage::ImageRgb8(src) => {
            let src =
                ImageRef::new(src.width(), src.height(), src.as_raw(), PixelType::U8x3).unwrap();
            let mut dst = Image::new(w, h, PixelType::U8x3);
            resizer.resize(&src, &mut dst, &opts).unwrap();
            dst.buffer()
                .as_chunks::<3>()
                .0
                .iter()
                .flat_map(|p| [p[0], p[1], p[2], 255])
                .collect()
        }
        other => {
            let src = other.to_rgba8();
            let src =
                ImageRef::new(src.width(), src.height(), src.as_raw(), PixelType::U8x4).unwrap();
            let mut dst = Image::new(w, h, PixelType::U8x4);
            resizer.resize(&src, &mut dst, &opts).unwrap();
            dst.into_vec()
        }
    };
    (w, h, to_bgra_premul(rgba))
}

fn cand_fir_box(img: &DynamicImage, edge: u32) -> (u32, u32, Vec<u8>) {
    use fast_image_resize::{FilterType, ResizeAlg};
    fir(img, edge, ResizeAlg::Convolution(FilterType::Box))
}

fn cand_fir_bilinear(img: &DynamicImage, edge: u32) -> (u32, u32, Vec<u8>) {
    use fast_image_resize::{FilterType, ResizeAlg};
    fir(img, edge, ResizeAlg::Convolution(FilterType::Bilinear))
}

fn cand_fir_catmull(img: &DynamicImage, edge: u32) -> (u32, u32, Vec<u8>) {
    use fast_image_resize::{FilterType, ResizeAlg};
    fir(img, edge, ResizeAlg::Convolution(FilterType::CatmullRom))
}

fn cand_fir_lanczos(img: &DynamicImage, edge: u32) -> (u32, u32, Vec<u8>) {
    use fast_image_resize::{FilterType, ResizeAlg};
    fir(img, edge, ResizeAlg::Convolution(FilterType::Lanczos3))
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let se: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| (f64::from(*x) - f64::from(*y)).powi(2))
        .sum();
    if se == 0.0 {
        return f64::INFINITY;
    }
    10.0 * (255.0f64.powi(2) / (se / a.len() as f64)).log10()
}

fn max_err(a: &[u8], b: &[u8]) -> u8 {
    a.iter()
        .zip(b)
        .map(|(x, y)| x.abs_diff(*y))
        .max()
        .unwrap_or(0)
}

/// Tiles `tile` to `w` x `h` (real photographic content at 24 MP).
fn tiled(tile: &RgbImage, w: u32, h: u32) -> RgbImage {
    RgbImage::from_fn(w, h, |x, y| {
        *tile.get_pixel(x % tile.width(), y % tile.height())
    })
}

/// Opaque red/blue field with fully transparent regions whose hidden colour is pure blue:
/// averaging straight alpha pulls the hidden colour into every semi-transparent edge pixel.
fn alpha_edges(w: u32, h: u32) -> RgbaImage {
    RgbaImage::from_fn(w, h, |x, y| {
        if (x / 50 + y / 50) % 2 == 0 {
            Rgba([220, 30, 30, 255])
        } else {
            Rgba([0, 0, 255, 0])
        }
    })
}

/// Concentric rings with a rising spatial frequency: any aliasing shows as moire.
fn zone_plate(w: u32, h: u32) -> RgbImage {
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    RgbImage::from_fn(w, h, |x, y| {
        let (dx, dy) = (x as f32 - cx, y as f32 - cy);
        let r2 = (dx * dx + dy * dy) / (w as f32 * w as f32);
        let v = (127.5 + 127.5 * (r2 * 9000.0).cos()) as u8;
        Rgb([v, v, v])
    })
}

fn resize_bench(photo_path: Option<&Path>) {
    let edge = 256;
    let candidates: &[Candidate] = &[
        ("DynamicImage::thumbnail (before)", cand_current),
        ("to_thumbnail (shipped)", cand_shipped),
        (
            "premultiply + DynamicImage::thumbnail",
            cand_premul_thumbnail,
        ),
        ("imageops CatmullRom, premultiplied", cand_catmull_premul),
        ("fast_image_resize Box", cand_fir_box),
        ("fast_image_resize Bilinear", cand_fir_bilinear),
        ("fast_image_resize CatmullRom", cand_fir_catmull),
        ("fast_image_resize Lanczos3", cand_fir_lanczos),
    ];
    let real = photo_path.map(|p| image::open(p).unwrap().to_rgb8());
    let mut inputs: Vec<(String, DynamicImage)> = vec![
        (
            "synthetic photo 6000x4000 (24 MP, RGB8)".into(),
            DynamicImage::ImageRgb8(photo(6000, 4000, 9)),
        ),
        (
            "zone plate 6000x4000 (aliasing probe, RGB8)".into(),
            DynamicImage::ImageRgb8(zone_plate(6000, 4000)),
        ),
        (
            "alpha edges 6000x4000 (RGBA8)".into(),
            DynamicImage::ImageRgba8(alpha_edges(6000, 4000)),
        ),
    ];
    if let Some(r) = &real {
        inputs.insert(
            0,
            (
                format!(
                    "real photo {}x{} tiled to 6000x4000 (RGB8)",
                    r.width(),
                    r.height()
                ),
                DynamicImage::ImageRgb8(tiled(r, 6000, 4000)),
            ),
        );
    }
    for (name, img) in &inputs {
        println!("\n{name} -> {edge} px longer edge");
        let reference = imageops_premul(img, edge, image::imageops::FilterType::Lanczos3);
        println!(
            "  {:<38} {:>9} {:>10} {:>9} {:>8}",
            "candidate", "min ms", "median ms", "PSNR dB", "max err"
        );
        for (label, f) in candidates {
            let reps = 9;
            let mut out = None;
            let s = stats(time_each(reps, || out = Some(black_box(f(img, edge)))));
            let (w, h, px) = out.unwrap();
            assert_eq!((w, h), (reference.0, reference.1), "{label}");
            println!(
                "  {label:<38} {:>9.1} {:>10.1} {:>9.2} {:>8}",
                ms(s.min),
                ms(s.median),
                psnr(&px, &reference.2),
                max_err(&px, &reference.2)
            );
        }
    }
    // Real-content check at actual wallpaper size, and an image for eyeballing.
    if let (Some(r), Some(p)) = (&real, photo_path) {
        let img = DynamicImage::ImageRgb8(r.clone());
        println!(
            "\n{} as is ({}x{}) -> {edge} px",
            p.display(),
            r.width(),
            r.height()
        );
        let reference = imageops_premul(&img, edge, image::imageops::FilterType::Lanczos3);
        for (label, f) in candidates {
            let (w, h, px) = f(&img, edge);
            println!("  {label:<38} PSNR {:>6.2} dB", psnr(&px, &reference.2));
            if let Ok(out) = std::env::var("TW_PERF_OUT") {
                let rgba: Vec<u8> = px
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .flat_map(|p| [p[2], p[1], p[0], p[3]])
                    .collect();
                let name = label.replace([' ', ',', '(', ')', ':'], "_");
                RgbaImage::from_raw(w, h, rgba)
                    .unwrap()
                    .save(Path::new(&out).join(format!("{name}.png")))
                    .unwrap();
            }
        }
    }
}

// --------------------------------------------------------------- per-call costs

fn open_stream(path: &Path) -> windows::Win32::System::Com::IStream {
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
    use windows::Win32::UI::Shell::SHCreateStreamOnFileEx;
    use windows::core::HSTRING;
    // SAFETY: COM init on the calling thread; the stream is used only by the caller.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        SHCreateStreamOnFileEx(&HSTRING::from(path.as_os_str()), 0, 0x80, false, None).unwrap()
    }
}

/// What one in-process `GetThumbnail` spends, stage by stage, on typical small inputs, so
/// `Settings::load` and the stream read can be judged against the whole call.
fn calls(iters: usize) {
    let dir = std::env::temp_dir().join(format!("tw-perf-calls-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(config::portable_ini(&dir), INI_BODY).unwrap();
    let files = [
        ("jpeg 640x480", "vga.jpg", jpeg(&photo(640, 480, 1), 85)),
        ("jpeg 1024x768", "xga.jpg", jpeg(&photo(1024, 768, 2), 85)),
        ("jpeg 12 MP", "12mp.jpg", jpeg(&photo(4000, 3000, 3), 85)),
        (
            "png 256x256 (icon-sized)",
            "icon.png",
            encode(
                &DynamicImage::ImageRgb8(photo(256, 256, 4)),
                ImageFormat::Png,
            ),
        ),
    ];
    let ini = stats(time_each(iters * 10, || {
        black_box(Settings::load(&dir));
    }));
    println!("per-call costs, {iters} calls each (microseconds)");
    row_us("Settings::load (INI)", &ini);
    let engine = Engine::new(PathBuf::from("unused"));
    for (name, file, data) in files {
        let path = dir.join(file);
        std::fs::write(&path, &data).unwrap();
        let stream = open_stream(&path);
        let size = data.len() as u64;
        println!("{name} ({} bytes)", data.len());
        let cur = stats(time_each(iters, || {
            black_box(read_current(&stream, size));
        }));
        row_us("read_all: 1 MiB bounce chunk (current)", &cur);
        let dir_read = stats(time_each(iters, || {
            black_box(read_direct(&stream, size));
        }));
        row_us("read_all: straight into the Vec", &dir_read);
        let data = Arc::new(data);
        let ext = Path::new(file).extension().unwrap().to_str().unwrap();
        let whole = stats(time_each(iters.min(100), || {
            black_box(engine.thumbnail(data.clone(), Some(ext), 256).unwrap());
        }));
        row_us("Engine::thumbnail, in-proc (decode+resize)", &whole);
        println!(
            "  Settings::load = {:.1}% of the in-proc thumbnail call; read_all bounce chunk = {:.1}%",
            100.0 * us(ini.median) / us(whole.median),
            100.0 * us(cur.median) / us(whole.median),
        );
        // The same call with each stage's before/after implementation, summed.
        let decoder = Decoder::Raster;
        let img = decoder.run(&data, ext, 256).unwrap();
        let reps = iters.min(100);
        let dec = stats(time_each(reps, || {
            black_box(decoder.run(&data, ext, 256).unwrap());
        }));
        let old = stats(time_each(reps, || {
            black_box(cand_current(&img, 256));
        }));
        let new = stats(time_each(reps, || {
            black_box(to_thumbnail(&img, 256));
        }));
        row_us("  decode only", &dec);
        row_us("  resize before: DynamicImage::thumbnail", &old);
        row_us("  resize after: to_thumbnail", &new);
        let before = ini.median + cur.median + dec.median + old.median;
        let after = ini.median + dir_read.median + dec.median + new.median;
        println!(
            "  call total (settings + read + decode + resize, medians): before {:.0} us, after {:.0} us ({:+.1}%)",
            us(before),
            us(after),
            100.0 * (us(after) - us(before)) / us(before)
        );
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

// -------------------------------------------------------------------- memory

fn peak_bytes() -> (u64, u64) {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: valid pseudo-handle and a correctly sized out-struct.
    unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb).unwrap() };
    (c.PeakWorkingSetSize as u64, c.PeakPagefileUsage as u64)
}

fn memory(mib: usize) {
    let dir = std::env::temp_dir().join(format!("tw-perf-mem-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let big = dir.join("big.bin");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&big).unwrap();
        let chunk: Vec<u8> = (0..1 << 20).map(|i| (i * 31 % 251) as u8).collect();
        for _ in 0..mib {
            f.write_all(&chunk).unwrap();
        }
    }
    let jpg = dir.join("photo.jpg");
    std::fs::write(&jpg, jpeg(&photo(6000, 4000, 5), 85)).unwrap();
    let exe = std::env::current_exe().unwrap();
    println!("peak memory (MiB) of one process reading a {mib} MiB stream");
    for (variant, file) in [
        ("baseline", &big),
        ("current-read", &big),
        ("direct-read", &big),
        ("current-read+decode", &jpg),
    ] {
        let out = std::process::Command::new(&exe)
            .args(["memory-one", variant])
            .arg(file)
            .output()
            .unwrap();
        println!(
            "  {variant:<22} {}",
            String::from_utf8_lossy(&out.stdout).trim()
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
}

fn memory_one(variant: &str, file: &Path) {
    use windows::Win32::System::Com::{
        COINIT_MULTITHREADED, CoInitializeEx, IStream, STREAM_SEEK_SET,
    };
    use windows::Win32::UI::Shell::SHCreateStreamOnFileEx;
    use windows::core::HSTRING;
    // SAFETY: COM init on the main thread, stream used only here.
    let stream: IStream = unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        SHCreateStreamOnFileEx(&HSTRING::from(file.as_os_str()), 0, 0x80, false, None).unwrap()
    };
    let size = std::fs::metadata(file).unwrap().len();
    let (base_ws, base_pf) = peak_bytes();
    let data = match variant {
        "baseline" => Vec::new(),
        "direct-read" => read_direct(&stream, size),
        _ => read_current(&stream, size),
    };
    black_box(&data);
    let _ = STREAM_SEEK_SET;
    if variant.ends_with("decode") {
        let engine = Engine::new(PathBuf::from("unused"));
        let t = engine.thumbnail(Arc::new(data), Some("jpg"), 256).unwrap();
        black_box(t);
    }
    let (ws, pf) = peak_bytes();
    let mib = |b: u64| b as f64 / 1_048_576.0;
    println!(
        "peak working set {:>8.1}, peak commit {:>8.1} (process start: {:.1} / {:.1})",
        mib(ws),
        mib(pf),
        mib(base_ws),
        mib(base_pf)
    );
}

/// Copy of `tw-shell` `thumbnail.rs::read_all` (private there): 1 MiB bounce chunk.
fn read_current(stream: &windows::Win32::System::Com::IStream, size: u64) -> Vec<u8> {
    use windows::Win32::System::Com::STREAM_SEEK_SET;
    // SAFETY: seeking and reading a stream we hold; buffers outlive the calls.
    unsafe { stream.Seek(0, STREAM_SEEK_SET, None).unwrap() };
    let mut buf = Vec::with_capacity(size as usize);
    let mut chunk = vec![0u8; 1 << 20];
    loop {
        let mut read = 0u32;
        let hr = unsafe {
            stream.Read(
                chunk.as_mut_ptr().cast(),
                chunk.len() as u32,
                Some(&mut read),
            )
        };
        hr.ok().unwrap();
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read as usize]);
    }
    buf
}

/// Reads straight into the destination vector (no bounce chunk).
fn read_direct(stream: &windows::Win32::System::Com::IStream, size: u64) -> Vec<u8> {
    use windows::Win32::System::Com::STREAM_SEEK_SET;
    // SAFETY: as above; the pointer is into the vector's spare capacity.
    unsafe { stream.Seek(0, STREAM_SEEK_SET, None).unwrap() };
    let mut buf: Vec<u8> = Vec::with_capacity(size as usize + 1);
    loop {
        let want = (buf.capacity() - buf.len()).min(1 << 20);
        let mut read = 0u32;
        let hr = unsafe {
            stream.Read(
                buf.as_mut_ptr().add(buf.len()).cast(),
                want as u32,
                Some(&mut read),
            )
        };
        hr.ok().unwrap();
        if read == 0 {
            break;
        }
        // SAFETY: the stream wrote `read` bytes into spare capacity.
        unsafe { buf.set_len(buf.len() + read as usize) };
    }
    buf
}
