//! `twctl`: per-user registration, diagnostics-free command-line tooling, and the
//! back end for every context-menu verb (the shell DLL never decodes in Explorer).
//!
//! Exit status 0 on success, 1 on any failure (including usage errors). Errors go to
//! stderr as `twctl: <message>`; with `--notify` (menu invocations) they are also shown
//! in a message box.

mod carry;
mod cmds;
mod doctor;
mod exif;
mod eyedropper;
mod jobs;
mod jpeg_lossless;
mod mcp;
mod meta;
mod modern;
mod ocr;
mod ops;
mod organize;
mod pdf;
mod popup;
mod prebuild;
mod strip;
mod ui;
mod verbs;
mod win;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use tw_core::config::{self, Settings};
use tw_core::decode::route;
use tw_core::registration::{self, Layout, Plan, Scope};
use tw_core::{formats, ids, log};

use ops::Result;

const USAGE: &str = "usage: twctl <command> [args]
  thumbnail <in> <out.png> [--size N]
  convert <in> <out> [--quality N] [--resize WxH|N%]
  convert-to <ext> <file...> [--quality N]
  resize --to WxH|N% <file...>
  rotate --by right|left|180|fliph|flipv [--trim] <file...>
  email --edge N <file...>
  compress --max-size 1MB|500KB|N <file...>
  strip <file...>
  clip <in>
  data-uri <in>
  to-folder <file...>
  info <in> [--json] [--gui]
  wallpaper <in> --style stretch|tile|center|fill|fit|span
  batch <thumbnail|convert|info> <in|dir...> [--recurse] [--out DIR] [--size N] [--to EXT] [--quality N] [--resize WxH|N%] [--json]
  batch --retry-from <report.json> [--out DIR] [--size N] [--to EXT] [--quality N] [--resize WxH|N%] [--json]
  prebuild <dir|file...> [--recurse] [--size N,N] [--rebuild-all] [--jobs N]
  pdf <out.pdf> <in...> [--strict] [--json] [--searchable] [--margin PT]
  pdf --auto <in...> [--strict] [--json] [--searchable] [--margin PT]
  cbz <out.cbz> <in...> [--strict] [--json]
  cbz --auto <in...> [--strict] [--json]
  ocr <in> [--copy] [--window]
  rename --by date|camera-date|artist-title|track-title|pattern [--pattern P] [--ask] <file...>
  sort --by size|date|tag [--tag artist|album|genre|year] [--ask] <file...>
  pick-color
  lock-screen <in>
  folder-icon <in>
  settings
  menu [--at X,Y] <file...>
  doctor [file] [--bundle out.zip]
  --mcp
  formats [--json]
  register [--off|--status|--reset-journal]
  --help | <command> --help
global options: --notify (show errors in a message box), --files-from <list.txt>;
  -- ends options (later arguments are file names even if they start with --)";

struct Cli {
    command: String,
    positional: Vec<String>,
    opts: HashMap<String, String>,
    flags: Vec<String>,
    /// `--help`/`-h` given after the command: print its synopsis instead of running it.
    help: bool,
}

const VALUE_OPTS: &[&str] = &[
    "--size",
    "--quality",
    "--resize",
    "--to",
    "--by",
    "--edge",
    "--max-size",
    "--style",
    "--files-from",
    "--at",
    "--out",
    "--retry-from",
    "--bundle",
    "--pattern",
    "--tag",
    "--jobs",
    "--margin",
];
const FLAG_OPTS: &[&str] = &[
    "--json",
    "--gui",
    "--off",
    "--status",
    "--reset-journal",
    "--notify",
    "--recurse",
    "--strict",
    "--searchable",
    "--copy",
    "--confirm",
    "--rebuild-all",
    "--open",
    "--auto",
    "--window",
    "--ask",
    "--trim",
];

fn parse(args: Vec<String>) -> Result<Cli> {
    let mut it = args.into_iter();
    let command = it.next().ok_or_else(|| USAGE.to_string())?;
    let mut cli = Cli {
        command,
        positional: Vec::new(),
        opts: HashMap::new(),
        flags: Vec::new(),
        help: false,
    };
    let mut options_ended = false;
    while let Some(a) = it.next() {
        if options_ended {
            cli.positional.push(a);
        } else if a == "--" {
            options_ended = true;
        } else if a == "--help" || a == "-h" {
            cli.help = true;
        } else if VALUE_OPTS.contains(&a.as_str()) {
            let v = it.next().ok_or_else(|| format!("{a} needs a value"))?;
            // A value that is itself an option token is a mistake (`doctor --bundle
            // --json`), not an output path literally named `--json`.
            if v.starts_with("--") {
                return Err(format!("{a} needs a value, got option {v}"));
            }
            cli.opts.insert(a, v);
        } else if FLAG_OPTS.contains(&a.as_str()) {
            cli.flags.push(a);
        } else if a.starts_with("--") {
            return Err(format!("unknown option {a}"));
        } else {
            cli.positional.push(a);
        }
    }
    if let Some(list) = cli.opts.remove("--files-from") {
        let text = std::fs::read_to_string(&list).map_err(|e| format!("{list}: {e}"))?;
        cli.positional.extend(
            text.lines()
                .filter(|l| !l.trim().is_empty())
                .map(str::to_owned),
        );
        // Lists the shell extension writes for long selections are consumed; any other
        // file is left alone.
        if is_shell_list(Path::new(&list)) {
            let _ = std::fs::remove_file(&list);
        }
    }
    Ok(cli)
}

/// `%TEMP%\tw-list-*.txt`, the shape of lists written by the shell extension.
fn is_shell_list(p: &Path) -> bool {
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let lower = name.to_ascii_lowercase();
    let shaped = lower.starts_with("tw-list-") && lower.ends_with(".txt") && lower.len() >= 12;
    shaped && p.parent() == Some(std::env::temp_dir().as_path())
}

impl Cli {
    fn flag(&self, f: &str) -> bool {
        self.flags.iter().any(|x| x == f)
    }

    fn opt(&self, o: &str) -> Option<&str> {
        self.opts.get(o).map(String::as_str)
    }

    fn require(&self, o: &str) -> Result<&str> {
        self.opt(o)
            .ok_or_else(|| format!("{} needs {o}", self.command))
    }

    /// Rejects options the command does not declare.
    fn allow(&self, opts: &[&str], flags: &[&str]) -> Result<()> {
        if let Some(o) = self.opts.keys().find(|o| !opts.contains(&o.as_str())) {
            return Err(format!("{} does not take {o}", self.command));
        }
        if let Some(f) = self
            .flags
            .iter()
            .find(|f| *f != "--notify" && !flags.contains(&f.as_str()))
        {
            return Err(format!("{} does not take {f}", self.command));
        }
        Ok(())
    }

    fn exactly(&self, n: usize) -> Result<&[String]> {
        if self.positional.len() != n {
            return Err(format!(
                "{} takes exactly {n} file argument(s)",
                self.command
            ));
        }
        Ok(&self.positional)
    }

    fn files(&self) -> Result<Vec<PathBuf>> {
        if self.positional.is_empty() {
            return Err(format!("{} needs at least one file", self.command));
        }
        Ok(self.positional.iter().map(PathBuf::from).collect())
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// Runs `f` for every file across all cores; collects per-file errors. A panic in `f`
/// (hostile input reaching an in-process decoder) becomes that file's error, like
/// `jobs::run_isolated`; a panicked worker thread can no longer discard its results and
/// let the command exit 0.
fn batch(files: &[PathBuf], f: impl Fn(&Path) -> Result<PathBuf> + Sync) -> Result<String> {
    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(files.len().max(1));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut results: Vec<(usize, Result<PathBuf>)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                s.spawn(|| {
                    let mut mine = Vec::new();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(file) = files.get(i) else { break mine };
                        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(file)))
                            .unwrap_or_else(|_| Err("decoder panicked".into()));
                        mine.push((i, r));
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });
    results.sort_by_key(|(i, _)| *i);
    let (mut ok, mut errs) = (Vec::new(), Vec::new());
    for (_, r) in results {
        match r {
            Ok(p) => ok.push(p.display().to_string()),
            Err(e) => errs.push(e),
        }
    }
    if errs.is_empty() {
        Ok(ok.join("\n"))
    } else {
        Err(format!(
            "{} of {} failed:\n{}",
            errs.len(),
            files.len(),
            errs.join("\n")
        ))
    }
}

/// Synopsis of one command from `USAGE` (every line that starts with it), or the whole
/// text for an unknown command.
fn command_usage(command: &str) -> String {
    let lines: Vec<&str> = USAGE
        .lines()
        .map(str::trim_start)
        .filter(|l| l.split_whitespace().next() == Some(command))
        .collect();
    if lines.is_empty() {
        return USAGE.to_string();
    }
    lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            if i == 0 {
                format!("usage: twctl {l}")
            } else {
                format!("       twctl {l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn run(cli: &Cli) -> Result<String> {
    if matches!(cli.command.as_str(), "--help" | "-h" | "help") {
        return Ok(USAGE.to_string());
    }
    if cli.help {
        return Ok(command_usage(&cli.command));
    }
    let dir = ops::exe_dir();
    let settings = Settings::load(&dir);
    ops::set_defaults(&settings);
    let max = settings.max_file_bytes();
    match cli.command.as_str() {
        "--version" | "-V" => Ok(format!("twctl {}", ids::VERSION)),
        "thumbnail" => {
            cli.allow(&["--size"], &[])?;
            let [input, out] = cli.exactly(2)? else {
                unreachable!()
            };
            let (input, out) = (Path::new(input), Path::new(out));
            if same_file(input, out) {
                return Err("output must differ from input".into());
            }
            let size = match cli.opt("--size") {
                Some(s) => s
                    .parse::<u32>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or("bad --size")?,
                None => 256,
            }
            .min(settings.max_thumb_px);
            let data = ops::read_input(input, max)?;
            let engine = tw_core::pipeline::Engine::new(dir.join(ids::WORKER_EXE_NAME))
                .with_video_percent(settings.video_frame_percent)
                .with_prefer_embedded(settings.use_embedded_thumbs);
            let ext = ops::ext_of(input);
            let t = engine
                .thumbnail(Arc::new(data), Some(&ext), size)
                .map_err(|e| format!("{}: {e}", input.display()))?;
            let img = ops::thumbnail_to_image(&t);
            ops::write_atomic(out, &ops::encode(&img.into(), "png", None)?)?;
            Ok(format!("{} ({}x{})", out.display(), t.width, t.height))
        }
        "convert" => {
            cli.allow(&["--quality", "--resize"], &[])?;
            let [input, out] = cli.exactly(2)? else {
                unreachable!()
            };
            let (input, out) = (Path::new(input), Path::new(out));
            if same_file(input, out) {
                return Err("output must differ from input".into());
            }
            let quality = parse_quality(cli)?;
            let (mut img, meta) = ops::decode_full_with_meta(input, max, settings.keep_metadata)?;
            if let Some(r) = cli.opt("--resize") {
                img = ops::apply_resize(&img, ops::parse_resize(r)?);
            }
            let bytes = ops::encode_with_meta(&img, &ops::ext_of(out), quality, &meta)?;
            ops::write_atomic_like(input, out, &bytes, settings.preserve_file_date)?;
            Ok(out.display().to_string())
        }
        "convert-to" => {
            cli.allow(&["--quality"], &[])?;
            let (target, files) = cli
                .positional
                .split_first()
                .ok_or("convert-to needs <ext> <file...>")?;
            let target = target.trim_start_matches('.').to_ascii_lowercase();
            if !ops::can_write(&target) {
                return Err(format!("no writer for .{target}"));
            }
            let quality = parse_quality(cli)?;
            if files.is_empty() {
                return Err("convert-to needs at least one file".into());
            }
            let files: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
            batch(&files, |f| {
                let (img, meta) = ops::decode_full_with_meta(f, max, settings.keep_metadata)?;
                let bytes = ops::encode_with_meta(&img, &target, quality, &meta)?;
                ops::write_new_like(f, "", &target, &bytes, settings.preserve_file_date)
            })
        }
        "resize" => {
            cli.allow(&["--to"], &[])?;
            let spec = ops::parse_resize(cli.require("--to")?)?;
            batch(&cli.files()?, |f| {
                let ext = writable_ext(f);
                let (img, meta) = ops::decode_full_with_meta(f, max, settings.keep_metadata)?;
                let img = ops::apply_resize(&img, spec);
                let bytes = ops::encode_with_meta(&img, &ext, None, &meta)?;
                ops::write_new_like(f, " (resized)", &ext, &bytes, settings.preserve_file_date)
            })
        }
        "rotate" => {
            cli.allow(&["--by"], &["--trim"])?;
            let by = cli.require("--by")?.to_owned();
            ops::rotate(&image::DynamicImage::new_rgb8(1, 1), &by)?;
            let notes = std::sync::Mutex::new(Vec::<String>::new());
            let result = batch(&cli.files()?, |f| {
                let r = ops::rotate_file(
                    f,
                    &by,
                    max,
                    settings.keep_metadata,
                    &writable_ext(f),
                    cli.flag("--trim"),
                )?;
                notes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend(r.notes);
                ops::write_new_like(
                    f,
                    " (edited)",
                    &r.ext,
                    &r.bytes,
                    settings.preserve_file_date,
                )
            });
            let mut notes = notes.into_inner().unwrap_or_else(|e| e.into_inner());
            notes.sort();
            let with_notes = |s: String| notes.iter().fold(s, |acc, n| format!("{acc}\nnote: {n}"));
            result.map(with_notes).map_err(with_notes)
        }
        "email" => {
            cli.allow(&["--edge"], &[])?;
            let edge: u32 = cli
                .require("--edge")?
                .parse()
                .ok()
                .filter(|n| *n > 0)
                .ok_or("bad --edge")?;
            batch(&cli.files()?, |f| {
                let (img, meta) = ops::decode_full_with_meta(f, max, settings.keep_metadata)?;
                let img = ops::apply_resize(&img, ops::Resize::Fit(edge, edge));
                let bytes = ops::encode_with_meta(&img, "jpg", Some(85), &meta)?;
                ops::write_new(f, " (email)", "jpg", &bytes)
            })
        }
        "compress" => {
            cli.allow(&["--max-size"], &[])?;
            let limit = ops::parse_size(cli.require("--max-size")?)?;
            batch(&cli.files()?, |f| {
                let (img, meta) = ops::decode_full_with_meta(f, max, settings.keep_metadata)?;
                let (bytes, notes) = ops::compress_to(&img, limit, &meta)
                    .map_err(|e| format!("{}: {e}", f.display()))?;
                notes
                    .iter()
                    .for_each(|n| eprintln!("twctl: note: {}: {n}", f.display()));
                ops::write_new(f, " (compressed)", "jpg", &bytes)
            })
        }
        "strip" => {
            cli.allow(&[], &[])?;
            batch(&cli.files()?, |f| {
                let data = ops::read_input(f, max)?;
                let out = strip::strip(&data).map_err(|e| format!("{}: {e}", f.display()))?;
                ops::write_atomic(f, &out)?;
                Ok(f.to_path_buf())
            })
        }
        "clip" => {
            cli.allow(&[], &[])?;
            let [input] = cli.exactly(1)? else {
                unreachable!()
            };
            win::copy_image(&ops::decode_full(Path::new(input), max)?)?;
            Ok("image copied to the clipboard".into())
        }
        "data-uri" => {
            cli.allow(&[], &[])?;
            let [input] = cli.exactly(1)? else {
                unreachable!()
            };
            let p = Path::new(input);
            let (mime, bytes) = match web_mime(&ops::ext_of(p)) {
                Some(m) => (m, ops::read_input(p, max)?),
                None => (
                    "image/png",
                    ops::encode(&ops::decode_full(p, max)?, "png", None)?,
                ),
            };
            win::copy_text(&format!("data:{mime};base64,{}", base64(&bytes)))?;
            Ok("data URI copied to the clipboard".into())
        }
        "to-folder" => {
            cli.allow(&[], &[])?;
            let files = cli.files()?;
            let parent = files[0]
                .parent()
                .ok_or("file has no parent folder")?
                .to_path_buf();
            if files.iter().any(|f| f.parent() != Some(parent.as_path())) {
                return Err("all files must be in the same folder".into());
            }
            let folder = std::iter::once(parent.join("New folder"))
                .chain((2..).map(|n| parent.join(format!("New folder ({n})"))))
                .find(|p| !p.exists())
                .expect("unbounded");
            std::fs::create_dir(&folder).map_err(|e| format!("{}: {e}", folder.display()))?;
            for f in &files {
                let dest = folder.join(f.file_name().ok_or("bad file name")?);
                std::fs::rename(f, &dest).map_err(|e| format!("{}: {e}", f.display()))?;
            }
            Ok(folder.display().to_string())
        }
        "info" => {
            cli.allow(&[], &["--json", "--gui"])?;
            let [input] = cli.exactly(1)? else {
                unreachable!()
            };
            let p = Path::new(input);
            let data = ops::read_input(p, max)?;
            let meta = exif::summarize_file(&data);
            let img = ops::decode_bytes(p, data)?;
            let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            let bits = img.color().bits_per_pixel() / u16::from(img.color().channel_count());
            let fmt = formats::lookup(&ops::ext_of(p)).map_or("unknown", |f| f.description);
            let report = InfoReport {
                path: p,
                format: fmt,
                width: img.width(),
                height: img.height(),
                channels: img.color().channel_count(),
                bit_depth: bits,
                file_size: size,
                meta: &meta,
            };
            let text = if cli.flag("--json") {
                report.json()
            } else {
                report.text()
            };
            if cli.flag("--gui") {
                win::message(&text, false);
            }
            Ok(text)
        }
        "wallpaper" => {
            cli.allow(&["--style"], &[])?;
            let pos = win::wallpaper_position(cli.require("--style")?)?;
            let [input] = cli.exactly(1)? else {
                unreachable!()
            };
            let img = ops::decode_full(Path::new(input), max)?;
            let store = std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .ok_or("LOCALAPPDATA not set")?
                .join(ids::PRODUCT_NAME);
            std::fs::create_dir_all(&store).map_err(|e| e.to_string())?;
            let out = store.join("wallpaper.png");
            ops::write_atomic(&out, &ops::encode(&img, "png", None)?)?;
            win::set_wallpaper(&out, pos)?;
            Ok(format!("wallpaper set from {input}"))
        }
        "formats" => {
            cli.allow(&[], &["--json"])?;
            cli.exactly(0)?;
            Ok(formats_listing(cli.flag("--json")))
        }
        "menu" => {
            cli.allow(&["--at"], &[])?;
            let files = cli.files()?;
            let at = cli
                .opt("--at")
                .map_or_else(|| Ok(popup::cursor()), popup::parse_at)?;
            let Some(verb) = popup::choose(&settings, &files, at)? else {
                return Ok(String::new());
            };
            // The choice runs here, through the same dispatch as a verb started from the
            // classic menu (`twctl <verb> --notify <files>`).
            let args = verb
                .argv(&files)
                .into_iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            run(&parse(args)?)
        }
        "register" => {
            cli.allow(&[], &["--off", "--status"])?;
            cli.exactly(0)?;
            register(cli, &dir, &settings)
        }
        other => cmds::run(cli, &dir, &settings, max)
            .unwrap_or_else(|| Err(format!("unknown command {other}\n{USAGE}"))),
    }
}

/// What `info` prints: spec keys `bit_depth, datetime, dpi_x, dpi_y, gps, height, make,
/// model, width` (null when the file does not record them) plus `channels`, `file_size`
/// and `format`.
struct InfoReport<'a> {
    path: &'a Path,
    format: &'a str,
    width: u32,
    height: u32,
    channels: u8,
    bit_depth: u16,
    file_size: u64,
    meta: &'a exif::Summary,
}

/// Whole numbers without a fraction, otherwise two decimals trimmed.
fn fmt_num(v: f64) -> String {
    ((v * 100.0).round() / 100.0).to_string()
}

impl InfoReport<'_> {
    fn gps_text(&self) -> Option<String> {
        self.meta.gps.map(|(lat, lon)| format!("{lat:.6},{lon:.6}"))
    }

    fn json(&self) -> String {
        let m = self.meta;
        let s = |v: &Option<String>| v.as_deref().map_or("null".to_string(), json_str);
        let n = |v: Option<f64>| v.map_or("null".to_string(), fmt_num);
        format!(
            "{{\"bit_depth\":{},\"channels\":{},\"datetime\":{},\"dpi_x\":{},\"dpi_y\":{},\"file_size\":{},\"format\":{},\"gps\":{},\"height\":{},\"make\":{},\"model\":{},\"width\":{}}}",
            self.bit_depth,
            self.channels,
            s(&m.datetime),
            n(m.dpi.map(|d| d.0)),
            n(m.dpi.map(|d| d.1)),
            self.file_size,
            json_str(self.format),
            s(&self.gps_text()),
            self.height,
            s(&m.make),
            s(&m.model),
            self.width,
        )
    }

    fn text(&self) -> String {
        let m = self.meta;
        let mut out = format!(
            "{}\n{}\n{} x {} pixels, {} channel(s), {}-bit\n{} bytes",
            self.path.display(),
            self.format,
            self.width,
            self.height,
            self.channels,
            self.bit_depth,
            self.file_size
        );
        let camera: Vec<&str> = [&m.make, &m.model]
            .into_iter()
            .filter_map(|v| v.as_deref())
            .collect();
        if !camera.is_empty() {
            out.push_str(&format!("\nCamera: {}", camera.join(" ")));
        }
        if let Some(d) = &m.datetime {
            out.push_str(&format!("\nTaken: {d}"));
        }
        if let Some((x, y)) = m.dpi {
            out.push_str(&format!(
                "\nResolution: {} x {} dpi",
                fmt_num(x),
                fmt_num(y)
            ));
        }
        if let Some(g) = self.gps_text() {
            out.push_str(&format!("\nGPS: {g}"));
        }
        out
    }
}

fn parse_quality(cli: &Cli) -> Result<Option<u8>> {
    cli.opt("--quality")
        .map(|q| {
            q.parse::<u8>()
                .ok()
                .filter(|n| (1..=100).contains(n))
                .ok_or_else(|| "bad --quality (1-100)".to_string())
        })
        .transpose()
}

/// Keeps the source format when it is writable, otherwise writes PNG.
fn writable_ext(p: &Path) -> String {
    let e = ops::ext_of(p);
    if ops::format_for_target(&e).is_some() {
        e
    } else {
        "png".into()
    }
}

fn web_mime(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "png" => "image/png",
        "jpg" | "jpeg" | "jpe" | "jfif" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        _ => return None,
    })
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn json_str(s: &str) -> String {
    let mut o = String::from('"');
    for ch in s.chars() {
        match ch {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn formats_listing(json: bool) -> String {
    let rows = formats::all().iter().map(|f| {
        let thumb = !route::chain_for_ext(f.ext).is_empty();
        (f, thumb)
    });
    if json {
        let items: Vec<String> = rows
            .map(|(f, thumb)| {
                format!(
                    "{{\"category\":{},\"convertible\":{},\"description\":{},\"ext\":{},\"os_codec\":{},\"source\":{},\"thumbnail\":{thumb}}}",
                    json_str(&format!("{:?}", f.category)),
                    f.convertible,
                    json_str(f.description),
                    json_str(&format!(".{}", f.ext)),
                    json_str(&format!("{:?}", f.os_codec)),
                    json_str(&format!("{:?}", f.source)),
                )
            })
            .collect();
        format!("[{}]", items.join(","))
    } else {
        rows.map(|(f, thumb)| {
            format!(
                ".{:<12} {:<10} {:<16} {} {}",
                f.ext,
                format!("{:?}", f.category),
                format!("{:?}", f.source),
                if thumb { "thumbnail" } else { "-        " },
                f.description
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
    }
}

fn register(cli: &Cli, dir: &Path, settings: &Settings) -> Result<String> {
    let layout = Layout::for_scope(Scope::User);
    // A corrupt journal (tampering, partial write) otherwise strands registration: register
    // unregisters first, and unregister refuses to replay a journal it cannot parse. This
    // escape hatch removes the product's own registration keys, so the next register starts
    // clean.
    if cli.flag("--reset-journal") {
        registration::reset(&layout).map_err(|e| e.to_string())?;
        return Ok("registration journal reset; product keys removed".into());
    }
    let layout = Layout::for_scope(Scope::User);
    if cli.flag("--status") {
        return Ok(match registration::status(&layout) {
            Some(s) => format!(
                "Explorer thumbnails: ON for this user ({} file types, context menu {})\nDLL: {}",
                s.thumbnail_extensions,
                if s.context_menu { "on" } else { "off" },
                s.dll_path
            ),
            None => "Explorer thumbnails: OFF for this user".into(),
        });
    }
    if cli.flag("--off") {
        registration::unregister(&layout).map_err(|e| e.to_string())?;
        return Ok("Explorer thumbnails turned OFF for this user".into());
    }
    // REQ-REG-6: per-user registration is for portable copies only; from an installed
    // copy it would survive uninstall and block thumbnails.
    if !config::is_portable(dir) {
        return Err(format!(
            "per-user registration is only for portable copies ({} next to twctl.exe); \
             an installed copy registers machine-wide, and a per-user registration would \
             survive uninstall and block thumbnails. Use `twctl register --off` to remove one.",
            config::PORTABLE_INI
        ));
    }
    let dll = dir.join(ids::SHELL_DLL_NAME);
    if !dll.is_file() {
        return Err(format!("{} not found", dll.display()));
    }
    let extensions: Vec<&str> = formats::all()
        .iter()
        .map(|f| f.ext)
        .filter(|e| !route::chain_for_ext(e).is_empty() && settings.type_enabled(e))
        .collect();
    let n = extensions.len();
    registration::register(
        &layout,
        &Plan {
            dll_path: &dll,
            extensions,
            context_menu: settings.menu_enabled,
            cloud: true,
            installer: None,
        },
    )
    .map_err(|e| e.to_string())?;
    Ok(format!(
        "Explorer thumbnails turned ON for this user ({n} file types)"
    ))
}

/// Set when a command finished but some items failed and its output (a report) still has
/// to reach stdout: `main` then exits 1 after printing it.
static PARTIAL_FAILURE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn mark_partial_failure() {
    PARTIAL_FAILURE.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Reads and clears the partial-failure mark.
fn take_partial_failure() -> bool {
    PARTIAL_FAILURE.swap(false, std::sync::atomic::Ordering::Relaxed)
}

fn main() -> ExitCode {
    log::init("twctl");
    let args: Vec<String> = std::env::args().skip(1).collect();
    let notify = args.iter().any(|a| a == "--notify");
    let result = parse(args).and_then(|cli| run(&cli));
    match result {
        Ok(msg) => {
            if !msg.is_empty() {
                println!("{msg}");
            }
            if take_partial_failure() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("twctl: {e}");
            log::error(&format!("twctl: {e}"));
            if notify {
                win::message(&e, true);
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (i, o) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(i.as_bytes()), o);
        }
    }

    #[test]
    fn help_succeeds_without_running_the_command() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            run(&parse(args(&["--help"])).unwrap()),
            Ok(USAGE.to_string())
        );
        let synopsis = run(&parse(args(&["strip", "--help"])).unwrap()).unwrap();
        assert_eq!(synopsis, "usage: twctl strip <file...>");
        // Help wins over missing required options and files.
        let resize = run(&parse(args(&["resize", "-h"])).unwrap()).unwrap();
        assert!(resize.starts_with("usage: twctl resize --to"), "{resize}");
    }

    #[test]
    fn double_dash_ends_options() {
        let args = vec![
            "strip".into(),
            "--".into(),
            "--x.png".into(),
            "--help".into(),
        ];
        let cli = parse(args).unwrap();
        assert_eq!(cli.positional, ["--x.png", "--help"]);
        assert!(!cli.help);
    }

    #[test]
    fn arg_parsing_rejects_unknown_and_extra() {
        let cli = parse(vec!["clip".into(), "a.png".into(), "b.png".into()]).unwrap();
        assert!(run(&cli).unwrap_err().contains("exactly 1"));
        assert!(parse(vec!["clip".into(), "--bogus".into()]).is_err());
        let cli = parse(vec!["formats".into(), "--quality".into(), "5".into()]).unwrap();
        assert!(run(&cli).is_err());
        assert!(parse(vec!["resize".into(), "--to".into()]).is_err());
    }

    #[test]
    fn thumbnail_refuses_overwriting_input() {
        let p = std::env::temp_dir().join(format!("twctl-same-{}.png", std::process::id()));
        std::fs::write(&p, b"x").unwrap();
        let s = p.to_string_lossy().into_owned();
        let cli = parse(vec!["thumbnail".into(), s.clone(), s]).unwrap();
        assert_eq!(run(&cli), Err("output must differ from input".into()));
        std::fs::remove_file(p).unwrap();
    }

    #[test]
    fn convert_to_same_stem_keeps_both_outputs() {
        let dir = std::env::temp_dir().join(format!("twctl-stem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let red = image::RgbImage::from_pixel(4, 4, image::Rgb([255, 0, 0]));
        let blue = image::RgbImage::from_pixel(4, 4, image::Rgb([0, 0, 255]));
        red.save(dir.join("a.png")).unwrap();
        blue.save(dir.join("a.bmp")).unwrap();
        let arg = |n: &str| dir.join(n).to_string_lossy().into_owned();
        let cli = parse(vec![
            "convert-to".into(),
            "tga".into(),
            arg("a.png"),
            arg("a.bmp"),
        ])
        .unwrap();
        run(&cli).unwrap();
        let mut pixels: Vec<[u8; 3]> = ["a.tga", "a (1).tga"]
            .iter()
            .map(|n| {
                image::open(dir.join(n))
                    .unwrap()
                    .to_rgb8()
                    .get_pixel(0, 0)
                    .0
            })
            .collect();
        pixels.sort();
        assert_eq!(pixels, vec![[0, 0, 255], [255, 0, 0]]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn files_from_user_list_is_kept() {
        let dir = std::env::temp_dir().join(format!("twctl-list-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let list = dir.join("tw-list-mine.txt");
        std::fs::write(&list, "a.png\nb.png\n").unwrap();
        let cli = parse(vec![
            "clip".into(),
            "--files-from".into(),
            list.to_string_lossy().into_owned(),
        ])
        .unwrap();
        assert!(run(&cli).unwrap_err().contains("exactly 1"));
        assert!(list.exists(), "a list outside %TEMP% is never deleted");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn files_from_shell_list_is_consumed() {
        let list = std::env::temp_dir().join(format!("tw-list-test-{}.txt", std::process::id()));
        std::fs::write(&list, "a.png\n\nb.png\n").unwrap();
        let cli = parse(vec![
            "clip".into(),
            "--files-from".into(),
            list.to_string_lossy().into_owned(),
        ])
        .unwrap();
        assert_eq!(cli.positional, vec!["a.png", "b.png"]);
        assert!(!list.exists());
    }

    fn jpeg_bytes(w: u32, h: u32, exif: Option<&[u8]>) -> Vec<u8> {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::new(w, h));
        let plain = ops::encode(&img, "jpg", None).unwrap();
        match exif {
            Some(e) => exif::testutil::inject_into_jpeg(&plain, e),
            None => plain,
        }
    }

    fn run_args(args: &[&str]) -> Result<String> {
        run(&parse(args.iter().map(|s| s.to_string()).collect()).unwrap())
    }

    #[test]
    fn info_reports_the_spec_keys_for_a_camera_jpeg() {
        let dir = std::env::temp_dir().join(format!("twctl-info-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cam = dir.join("cam.jpg");
        let block = exif::testutil::camera_exif(6, (64, 48));
        std::fs::write(&cam, jpeg_bytes(64, 48, Some(&block))).unwrap();
        let cam = cam.to_string_lossy().into_owned();
        let json = run_args(&["info", &cam, "--json"]).unwrap();
        for expected in [
            "\"bit_depth\":8",
            "\"datetime\":\"2023:07:04 12:34:56\"",
            "\"dpi_x\":300",
            "\"dpi_y\":300",
            "\"gps\":\"47.606200,-122.332100\"",
            "\"make\":\"Thumbcam\"",
            "\"model\":\"TC-100 Mark II\"",
            // Orientation 6 is applied by the decoder, so the displayed size is swapped.
            "\"width\":48",
            "\"height\":64",
        ] {
            assert!(json.contains(expected), "{expected} missing from {json}");
        }
        let text = run_args(&["info", &cam]).unwrap();
        for expected in [
            "Camera: Thumbcam TC-100 Mark II",
            "Taken: 2023:07:04 12:34:56",
            "Resolution: 300 x 300 dpi",
            "GPS: 47.606200,-122.332100",
        ] {
            assert!(text.contains(expected), "{expected} missing from {text}");
        }
        // A file with no metadata still lists every key, as null.
        let bare = dir.join("bare.jpg");
        std::fs::write(&bare, jpeg_bytes(16, 8, None)).unwrap();
        let json = run_args(&["info", &bare.to_string_lossy(), "--json"]).unwrap();
        for key in [
            "bit_depth",
            "datetime",
            "dpi_x",
            "dpi_y",
            "gps",
            "height",
            "make",
            "model",
            "width",
        ] {
            assert!(
                json.contains(&format!("\"{key}\":")),
                "{key} missing: {json}"
            );
        }
        for key in ["datetime", "dpi_x", "dpi_y", "gps", "make", "model"] {
            assert!(json.contains(&format!("\"{key}\":null")), "{key}: {json}");
        }
        assert!(
            !run_args(&["info", &bare.to_string_lossy()])
                .unwrap()
                .contains("Camera")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn convert_keeps_make_model_and_datetime() {
        let dir = std::env::temp_dir().join(format!("twctl-convexif-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("cam.jpg");
        let block = exif::testutil::camera_exif(1, (32, 24));
        std::fs::write(&src, jpeg_bytes(32, 24, Some(&block))).unwrap();
        for ext in ["png", "jpg"] {
            let out = dir.join(format!("out.{ext}"));
            run_args(&["convert", &src.to_string_lossy(), &out.to_string_lossy()]).unwrap();
            let meta = exif::summarize_file(&std::fs::read(&out).unwrap());
            assert_eq!(meta.make.as_deref(), Some("Thumbcam"), "{ext}");
            assert_eq!(meta.model.as_deref(), Some("TC-100 Mark II"), "{ext}");
            assert_eq!(
                meta.datetime.as_deref(),
                Some("2023:07:04 12:34:56"),
                "{ext}"
            );
        }
        let json = run_args(&["info", &dir.join("out.png").to_string_lossy(), "--json"]).unwrap();
        assert!(json.contains("\"make\":\"Thumbcam\""), "{json}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rotate_trim_flag_trims_instead_of_re_encoding() {
        let dir = std::env::temp_dir().join(format!("twctl-rottrim-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("odd.jpg");
        std::fs::write(&src, jpeg_bytes(50, 30, None)).unwrap();
        let path = src.to_string_lossy().into_owned();
        // 50x30 is not a whole number of 8x8 MCUs, and a half turn mirrors both axes.
        let plain = run_args(&["rotate", "--by", "180", &path]).unwrap();
        assert!(plain.contains("not rotated losslessly"), "{plain}");
        let trimmed = run_args(&["rotate", "--by", "180", "--trim", &path]).unwrap();
        assert!(
            trimmed.contains("trimmed the partial edge MCUs, 50x30 became 48x24"),
            "{trimmed}"
        );
        assert!(!trimmed.contains("not rotated losslessly"), "{trimmed}");
        let out = dir.join("odd (edited) (1).jpg");
        let shown = image::open(&out).unwrap();
        assert_eq!((shown.width(), shown.height()), (48, 24));
        // The flag belongs to rotate only.
        let err = run_args(&[
            "convert",
            "--trim",
            &path,
            &dir.join("x.png").to_string_lossy(),
        ])
        .unwrap_err();
        assert!(err.contains("does not take --trim"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod s_fixes_tests {
    use super::*;

    #[test]
    fn value_option_rejects_an_option_shaped_value() {
        let err = match parse(vec!["doctor".into(), "--bundle".into(), "--json".into()]) {
            Err(e) => e,
            Ok(_) => panic!("option-shaped value must be a usage error"),
        };
        assert!(err.contains("needs a value"), "{err}");
    }

    #[test]
    fn batch_reports_a_panicking_file_instead_of_silent_success() {
        let dir = std::env::temp_dir().join(format!("twctl-panic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.png");
        std::fs::write(&a, b"x").unwrap();
        let b = dir.join("b.png");
        std::fs::write(&b, b"x").unwrap();
        let files = vec![a.clone(), b.clone()];
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let f = |p: &Path| -> Result<PathBuf> {
            if p == a {
                panic!("decoder panicked");
            }
            let target = out.join(p.file_name().unwrap());
            std::fs::write(&target, b"ok").map_err(|e| e.to_string())?;
            Ok(target)
        };
        let res = batch(&files, f).unwrap_err();
        assert!(res.contains("decoder panicked"), "{res}");
        assert!(res.contains("1 of 2 failed"), "{res}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
