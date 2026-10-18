//! `batch`: multi-file thumbnail/convert/info jobs with a JSON report and `--retry-from`
//! (FUNCTIONAL_SPEC.md 6.4, 7). Files fan out across all cores; a failed file never stops
//! the others.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};
use tw_core::config::Settings;
use tw_core::decode::route;
use tw_core::{formats, ids};

use crate::ops::{self, Result};

/// The three batch operations.
const OPERATIONS: [&str; 3] = ["thumbnail", "convert", "info"];

/// Everything that shapes one run; persisted in the report so `--retry-from` can repeat it.
#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    pub operation: String,
    pub out: Option<PathBuf>,
    pub size: u32,
    pub to: Option<String>,
    pub quality: Option<u8>,
    pub resize: Option<String>,
}

impl Options {
    fn json(&self) -> Value {
        json!({
            "out": self.out.as_ref().map(|p| p.display().to_string()),
            "quality": self.quality,
            "resize": self.resize,
            "size": self.size,
            "to": self.to,
        })
    }

    fn validate(&self) -> Result<()> {
        if !OPERATIONS.contains(&self.operation.as_str()) {
            return Err(format!(
                "batch operation must be one of {} (got {:?})",
                OPERATIONS.join("|"),
                self.operation
            ));
        }
        if self.operation == "convert" {
            let to = self.to.as_deref().ok_or("batch convert needs --to EXT")?;
            if !ops::can_write(to) {
                return Err(format!("no writer for .{to}"));
            }
        }
        if let Some(r) = &self.resize {
            ops::parse_resize(r)?;
        }
        Ok(())
    }
}

/// One file's outcome.
pub struct Outcome {
    pub input: PathBuf,
    pub result: Result<Value>,
}

/// Files for `inputs`: explicit files as given; folders expand to their supported files,
/// one level deep unless `recurse`.
pub fn expand_inputs(
    inputs: &[PathBuf],
    recurse: bool,
    settings: &Settings,
) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for input in inputs {
        let meta = std::fs::metadata(input).map_err(|e| format!("{}: {e}", input.display()))?;
        if !meta.is_dir() {
            files.push(input.clone());
            continue;
        }
        let mut stack = vec![input.clone()];
        while let Some(dir) = stack.pop() {
            let read = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            let mut children: Vec<PathBuf> =
                read.filter_map(|e| e.ok().map(|e| e.path())).collect();
            children.sort();
            for child in children {
                if child.is_dir() {
                    if recurse {
                        stack.push(child);
                    }
                } else {
                    let ext = ops::ext_of(&child);
                    if !route::chain_for_ext(&ext).is_empty() && settings.type_enabled(&ext) {
                        files.push(child);
                    }
                }
            }
        }
    }
    Ok(files)
}

/// Output location: beside the source, or in `--out` (created on demand).
fn target(src: &Path, out: Option<&Path>) -> Result<PathBuf> {
    match out {
        None => Ok(src.to_path_buf()),
        Some(dir) => {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            Ok(dir.join(src.file_name().ok_or("bad file name")?))
        }
    }
}

fn process(file: &Path, o: &Options, settings: &Settings, worker: &Path) -> Result<Value> {
    let max = settings.max_file_bytes();
    match o.operation.as_str() {
        "thumbnail" => {
            let data = ops::read_input(file, max)?;
            let engine = tw_core::pipeline::Engine::new(worker.to_path_buf());
            let ext = ops::ext_of(file);
            let size = o.size.min(settings.max_thumb_px);
            let t = engine
                .thumbnail(Arc::new(data), Some(&ext), size)
                .map_err(|e| format!("{}: {e}", file.display()))?;
            let png = ops::encode(&ops::thumbnail_to_image(&t).into(), "png", None)?;
            let out = ops::write_new(&target(file, o.out.as_deref())?, " (thumb)", "png", &png)?;
            Ok(json!({"output": out.display().to_string(), "width": t.width, "height": t.height}))
        }
        "convert" => {
            let to = o.to.as_deref().unwrap_or("png");
            let mut img = ops::decode_full(file, max)?;
            if let Some(r) = &o.resize {
                img = ops::apply_resize(&img, ops::parse_resize(r)?);
            }
            let bytes = ops::encode(&img, to, o.quality)?;
            let out = ops::write_new(&target(file, o.out.as_deref())?, "", to, &bytes)?;
            Ok(json!({"output": out.display().to_string()}))
        }
        _ => {
            let img = ops::decode_full(file, max)?;
            let size = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
            let channels = u16::from(img.color().channel_count());
            Ok(json!({
                "width": img.width(),
                "height": img.height(),
                "bit_depth": img.color().bits_per_pixel() / channels,
                "channels": channels,
                "format": formats::lookup(&ops::ext_of(file)).map_or("unknown", |f| f.description),
                "file_size": size,
            }))
        }
    }
}

/// Runs `o` over `files` on all cores; outcomes come back in input order.
pub fn run_files(files: &[PathBuf], o: &Options, settings: &Settings, dir: &Path) -> Vec<Outcome> {
    let worker = dir.join(ids::WORKER_EXE_NAME);
    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(files.len().max(1));
    let next = AtomicUsize::new(0);
    let mut done: Vec<(usize, Outcome)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                s.spawn(|| {
                    let mut mine = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(file) = files.get(i) else { break mine };
                        // A decoder panic fails that file, not the batch.
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            process(file, o, settings, &worker)
                        }))
                        .unwrap_or_else(|_| Err(format!("{}: decoder panicked", file.display())));
                        mine.push((
                            i,
                            Outcome {
                                input: file.clone(),
                                result,
                            },
                        ));
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });
    done.sort_by_key(|(i, _)| *i);
    done.into_iter().map(|(_, o)| o).collect()
}

/// The report document.
pub fn report(o: &Options, outcomes: &[Outcome]) -> Value {
    let failed = outcomes.iter().filter(|x| x.result.is_err()).count();
    json!({
        "operation": o.operation,
        "options": o.json(),
        "total": outcomes.len(),
        "succeeded": outcomes.len() - failed,
        "failed": failed,
        "results": outcomes.iter().map(|x| match &x.result {
            Ok(v) => json!({"input": x.input.display().to_string(), "status": "ok", "detail": v}),
            Err(e) => json!({"input": x.input.display().to_string(), "status": "failed", "error": e}),
        }).collect::<Vec<_>>(),
    })
}

fn text_report(outcomes: &[Outcome]) -> String {
    let mut lines: Vec<String> = outcomes
        .iter()
        .map(|x| match &x.result {
            Ok(v) => {
                let out = v.get("output").and_then(Value::as_str);
                match out {
                    Some(o) => format!("ok      {} -> {o}", x.input.display()),
                    None => format!("ok      {} {v}", x.input.display()),
                }
            }
            Err(e) => format!("FAILED  {e}"),
        })
        .collect();
    let failed = outcomes.iter().filter(|x| x.result.is_err()).count();
    lines.push(format!(
        "{} of {} succeeded, {failed} failed",
        outcomes.len() - failed,
        outcomes.len()
    ));
    lines.join("\n")
}

/// Options and inputs recovered from a report: its operation, its recorded options, and the
/// inputs that failed.
pub fn read_report(text: &str) -> Result<(Options, Vec<PathBuf>)> {
    let v: Value =
        serde_json::from_str(text).map_err(|e| format!("report is not valid JSON: {e}"))?;
    let operation = v
        .get("operation")
        .and_then(Value::as_str)
        .ok_or("report has no operation")?
        .to_owned();
    let opts = v.get("options").cloned().unwrap_or(Value::Null);
    let o = Options {
        operation,
        out: opts.get("out").and_then(Value::as_str).map(PathBuf::from),
        size: opts
            .get("size")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0)
            .unwrap_or(256),
        to: opts.get("to").and_then(Value::as_str).map(str::to_owned),
        quality: opts
            .get("quality")
            .and_then(Value::as_u64)
            .and_then(|n| u8::try_from(n).ok()),
        resize: opts
            .get("resize")
            .and_then(Value::as_str)
            .map(str::to_owned),
    };
    let failed = v
        .get("results")
        .and_then(Value::as_array)
        .ok_or("report has no results")?
        .iter()
        .filter(|r| r.get("status").and_then(Value::as_str) == Some("failed"))
        .filter_map(|r| r.get("input").and_then(Value::as_str))
        .map(PathBuf::from)
        .collect();
    Ok((o, failed))
}

/// What `batch` produced.
pub struct Batch {
    pub text: String,
    pub failed: bool,
}

/// Entry point for the `batch` command; arguments are already parsed.
#[allow(clippy::too_many_arguments)]
pub fn run(
    positional: &[String],
    retry_from: Option<&str>,
    overrides: Options,
    recurse: bool,
    json_out: bool,
    settings: &Settings,
    dir: &Path,
) -> Result<Batch> {
    let (options, files) = if let Some(path) = retry_from {
        if !positional.is_empty() {
            return Err("batch --retry-from takes no input arguments".into());
        }
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        let (mut o, failed) = read_report(&text)?;
        // Anything given on the command line wins over the recorded options.
        if !overrides.operation.is_empty() {
            o.operation = overrides.operation.clone();
        }
        if overrides.out.is_some() {
            o.out = overrides.out.clone();
        }
        if overrides.to.is_some() {
            o.to = overrides.to.clone();
        }
        if overrides.quality.is_some() {
            o.quality = overrides.quality;
        }
        if overrides.resize.is_some() {
            o.resize = overrides.resize.clone();
        }
        if overrides.size != 0 {
            o.size = overrides.size;
        }
        (o, failed)
    } else {
        let mut o = overrides;
        if o.size == 0 {
            o.size = 256;
        }
        if positional.is_empty() {
            return Err("batch needs at least one file or folder".into());
        }
        let inputs: Vec<PathBuf> = positional.iter().map(PathBuf::from).collect();
        (o.clone(), expand_inputs(&inputs, recurse, settings)?)
    };
    options.validate()?;
    let outcomes = run_files(&files, &options, settings, dir);
    let failed = outcomes.iter().any(|o| o.result.is_err());
    let text = if json_out {
        report(&options, &outcomes).to_string()
    } else {
        text_report(&outcomes)
    };
    Ok(Batch { text, failed })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(op: &str) -> Options {
        Options {
            operation: op.into(),
            out: None,
            size: 64,
            to: Some("bmp".into()),
            quality: None,
            resize: None,
        }
    }

    #[test]
    fn retry_report_reruns_only_failures_and_keeps_options() {
        let dir = std::env::temp_dir().join(format!("twctl-batch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        image::RgbImage::new(4, 4)
            .save(dir.join("good.png"))
            .unwrap();
        std::fs::write(dir.join("bad.png"), b"broken").unwrap();
        let settings = Settings::default();
        let exe_dir = dir.clone();
        let files = vec![dir.join("good.png"), dir.join("bad.png")];
        let o = opts("convert");
        let first = run_files(&files, &o, &settings, &exe_dir);
        assert!(first[0].result.is_ok() && first[1].result.is_err());
        let doc = report(&o, &first).to_string();

        // Fix the failure, then retry from the report.
        image::RgbImage::new(2, 2)
            .save(dir.join("bad.png"))
            .unwrap();
        let (ro, retry) = read_report(&doc).unwrap();
        assert_eq!(ro, o);
        assert_eq!(retry, vec![dir.join("bad.png")]);
        let second = run_files(&retry, &ro, &settings, &exe_dir);
        assert_eq!(second.len(), 1);
        assert!(second[0].result.is_ok());
        // The good file was converted once; the retry did not redo it.
        assert!(dir.join("good.bmp").is_file() && !dir.join("good (1).bmp").exists());
        assert!(dir.join("bad.bmp").is_file());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn validation_and_report_parsing_reject_bad_input() {
        assert!(opts("sharpen").validate().is_err());
        let mut o = opts("convert");
        o.to = None;
        assert!(o.validate().unwrap_err().contains("--to"));
        o.to = Some("heic".into());
        assert!(o.validate().unwrap_err().contains("no writer"));
        assert!(read_report("not json").is_err());
        assert!(read_report("{}").is_err());
        assert!(read_report(r#"{"operation":"info"}"#).is_err());
    }
}
