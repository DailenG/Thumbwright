//! Command arms for the verbs added on top of the core CLI: batch, prebuild, combine,
//! OCR, organise, upload, shell verbs, doctor and the MCP server.

use std::io::Write;
use std::path::{Path, PathBuf};

use tw_core::config::Settings;

use crate::ops::{self, Result};
use crate::{
    Cli, doctor, eyedropper, jobs, mcp, meta, ocr, organize, pdf, prebuild, ui, upload, verbs, win,
};

/// Page margin (PDF units) when the "margin on combined PDFs" setting is on.
const SETTING_MARGIN_PT: f64 = 36.0;

fn parse_size_opt(cli: &Cli) -> Result<u32> {
    match cli.opt("--size") {
        Some(s) => s
            .parse::<u32>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| "bad --size".to_string()),
        None => Ok(0),
    }
}

fn single_path(cli: &Cli) -> Result<&Path> {
    let [input] = cli.exactly(1)? else {
        unreachable!()
    };
    Ok(Path::new(input))
}

/// Where a combined document goes.
enum Target {
    Path(PathBuf),
    /// `<first input stem> (combined).<ext>` next to the first input, never replacing a file.
    Auto(PathBuf),
}

/// Splits `<out> <in...>` (or `--auto <in...>`) and refuses an output equal to an input.
fn combine_args(cli: &Cli, ext: &str) -> Result<(Target, Vec<PathBuf>)> {
    if cli.flag("--auto") {
        let inputs = cli.files()?;
        let first = inputs[0].clone();
        return Ok((Target::Auto(first), inputs));
    }
    let (out, inputs) = cli
        .positional
        .split_first()
        .ok_or_else(|| format!("{} needs <out.{ext}> <in...>", cli.command))?;
    if inputs.is_empty() {
        return Err(format!("{} needs at least one input", cli.command));
    }
    let out = PathBuf::from(out);
    let inputs: Vec<PathBuf> = inputs.iter().map(PathBuf::from).collect();
    if inputs.iter().any(|i| crate::same_file(i, &out)) {
        return Err("output must differ from every input".into());
    }
    Ok((Target::Path(out), inputs))
}

fn write_combined(
    target: Target,
    ext: &str,
    bytes: &[u8],
    c: &pdf::Combined,
    json: bool,
) -> Result<String> {
    let out = match target {
        Target::Path(p) => {
            ops::write_atomic(&p, bytes)?;
            p
        }
        Target::Auto(first) => ops::write_new(&first, " (combined)", ext, bytes)?,
    };
    Ok(if json { c.json(&out) } else { c.summary(&out) })
}

fn confirm_text(files: &[PathBuf], hosts: &[upload::Host]) -> String {
    const SHOWN: usize = 8;
    let mut s = format!(
        "Upload {} file{} to the internet?\n\n",
        files.len(),
        if files.len() == 1 { "" } else { "s" }
    );
    for f in files.iter().take(SHOWN) {
        s.push_str(&format!(
            "  {}\n",
            f.file_name().map_or_else(
                || f.display().to_string(),
                |n| n.to_string_lossy().into_owned()
            )
        ));
    }
    if files.len() > SHOWN {
        s.push_str(&format!("  ... and {} more\n", files.len() - SHOWN));
    }
    let names: Vec<String> = hosts
        .iter()
        .map(|h| {
            h.url
                .trim_start_matches("https://")
                .split('/')
                .next()
                .unwrap_or("")
                .to_owned()
        })
        .collect();
    s.push_str(&format!(
        "\nThe file{} will be sent over HTTPS to the first of these hosts that accepts it, and anyone with the link can open it:\n  {}",
        if files.len() == 1 { "" } else { "s" },
        names.join(", ")
    ));
    s
}

fn upload_cmd(cli: &Cli, dir: &Path) -> Result<String> {
    cli.allow(&[], &["--copy", "--confirm"])?;
    let files = cli.files()?;
    let hosts = upload::load_hosts(dir)?;
    if cli.flag("--confirm") && !ui::confirm_cancel_default(&confirm_text(&files, &hosts)) {
        return Ok("upload cancelled".into());
    }
    let (mut links, mut errors) = (Vec::new(), Vec::new());
    for f in &files {
        match upload::upload_file(&hosts, f) {
            Ok(up) => {
                if let Err(e) = upload::record(f, &up) {
                    tw_core::log::error(&format!("upload history: {e}"));
                }
                links.push((f.clone(), up));
            }
            Err(e) => errors.push(e),
        }
    }
    let text: Vec<String> = links
        .iter()
        .map(|(f, up)| {
            if files.len() == 1 {
                up.link.clone()
            } else {
                format!("{} -> {}", f.display(), up.link)
            }
        })
        .collect();
    if cli.flag("--copy") && !links.is_empty() {
        let joined: Vec<&str> = links.iter().map(|(_, u)| u.link.as_str()).collect();
        win::copy_text(&joined.join("\r\n"))?;
    }
    if errors.is_empty() {
        // Menu invocation: tell the user where the link went.
        if cli.flag("--notify") {
            win::message(
                &format!(
                    "{}\n\n{}",
                    if cli.flag("--copy") {
                        "Link copied to the clipboard:"
                    } else {
                        "Uploaded:"
                    },
                    text.join("\n")
                ),
                false,
            );
        }
        Ok(text.join("\n"))
    } else {
        Err(format!(
            "{} of {} failed:\n{}{}",
            errors.len(),
            files.len(),
            errors.join("\n"),
            if text.is_empty() {
                String::new()
            } else {
                format!("\nuploaded:\n{}", text.join("\n"))
            }
        ))
    }
}

fn batch_cmd(cli: &Cli, settings: &Settings, dir: &Path) -> Result<String> {
    cli.allow(
        &[
            "--out",
            "--size",
            "--to",
            "--quality",
            "--resize",
            "--retry-from",
        ],
        &["--recurse", "--json"],
    )?;
    let retry = cli.opt("--retry-from");
    let (operation, inputs) = if retry.is_some() {
        (String::new(), &cli.positional[..])
    } else {
        let (op, rest) = cli
            .positional
            .split_first()
            .ok_or("batch needs an operation: thumbnail, convert or info")?;
        (op.clone(), rest)
    };
    let overrides = jobs::Options {
        operation,
        out: cli.opt("--out").map(PathBuf::from),
        size: parse_size_opt(cli)?,
        to: cli
            .opt("--to")
            .map(|t| t.trim_start_matches('.').to_ascii_lowercase()),
        quality: crate::parse_quality(cli)?,
        resize: cli.opt("--resize").map(str::to_owned),
    };
    let b = jobs::run(
        inputs,
        retry,
        overrides,
        cli.flag("--recurse"),
        cli.flag("--json"),
        settings,
        dir,
    )?;
    if b.failed {
        crate::mark_partial_failure();
    }
    Ok(b.text)
}

fn prebuild_cmd(cli: &Cli, settings: &Settings) -> Result<String> {
    cli.allow(&["--size", "--jobs"], &["--recurse", "--rebuild-all"])?;
    let inputs = cli.files()?;
    let sizes = match cli.opt("--size") {
        Some(s) => prebuild::parse_sizes(s)?,
        None => prebuild::DEFAULT_SIZES.to_vec(),
    };
    let jobs = match cli.opt("--jobs") {
        Some(j) => j
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=64).contains(n))
            .ok_or("bad --jobs (1-64)")?,
        None => std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .min(16),
    };
    let (files, skipped) = prebuild::collect(&inputs, cli.flag("--recurse"), settings)?;
    let mut summary = prebuild::warm(&files, &sizes, cli.flag("--rebuild-all"), jobs);
    summary.skipped = skipped;
    let text = summary.text(&sizes);
    if cli.flag("--notify") && summary.failed < summary.extracted + summary.already_cached {
        win::message(&text, false);
    }
    if summary.failed > 0 && summary.extracted + summary.already_cached == 0 {
        return Err(format!(
            "{text}\nIs the thumbnail handler registered? Run `twctl doctor`."
        ));
    }
    Ok(text)
}

fn rename_cmd(cli: &Cli, max: u64) -> Result<String> {
    cli.allow(&["--by", "--pattern"], &["--ask"])?;
    let by = cli.require("--by")?;
    let pattern = if by == "pattern" {
        match (cli.opt("--pattern"), cli.flag("--ask")) {
            (Some(p), _) => p.to_owned(),
            (None, true) => {
                let Some(p) = ui::prompt(
                    "Rename with pattern",
                    "Pattern ({name} {date} {time} {camera} {artist} {title} {n:3} ...):",
                    "{date} {time} {name}",
                ) else {
                    return Ok(String::new());
                };
                if p.trim().is_empty() {
                    return Err("empty pattern".into());
                }
                p
            }
            (None, false) => return Err("rename --by pattern needs --pattern P".into()),
        }
    } else {
        organize::preset_pattern(by)
            .ok_or_else(|| {
                format!("bad --by {by} (date|camera-date|artist-title|track-title|pattern)")
            })?
            .to_owned()
    };
    organize::rename(&cli.files()?, &pattern, max)
}

fn sort_cmd(cli: &Cli, max: u64) -> Result<String> {
    cli.allow(&["--by", "--tag"], &["--ask"])?;
    let by = cli.require("--by")?;
    let tag = match (cli.opt("--tag"), by == "tag" && cli.flag("--ask")) {
        (Some(t), _) => t.to_owned(),
        (None, true) => {
            let Some(t) = ui::prompt(
                "Sort by audio tag",
                "Tag to sort by (artist, album, genre, year):",
                "artist",
            ) else {
                return Ok(String::new());
            };
            t.trim().to_ascii_lowercase()
        }
        (None, false) => "artist".to_owned(),
    };
    organize::sort(&cli.files()?, by, &tag, max)
}

fn combine_pdf_cmd(cli: &Cli, settings: &Settings, max: u64) -> Result<String> {
    cli.allow(
        &["--margin"],
        &["--strict", "--json", "--searchable", "--auto"],
    )?;
    let margin = match cli.opt("--margin") {
        Some(m) => m
            .parse::<f64>()
            .ok()
            .filter(|v| (0.0..=720.0).contains(v))
            .ok_or("bad --margin (0-720 points)")?,
        None if settings.pdf_margin => SETTING_MARGIN_PT,
        None => 0.0,
    };
    let (target, inputs) = combine_args(cli, "pdf")?;
    let (bytes, c) = pdf::build_pdf(
        &inputs,
        cli.flag("--strict"),
        cli.flag("--searchable"),
        margin,
        max,
    )?;
    write_combined(target, "pdf", &bytes, &c, cli.flag("--json"))
}

fn combine_cbz_cmd(cli: &Cli, max: u64) -> Result<String> {
    cli.allow(&[], &["--strict", "--json", "--auto"])?;
    let (target, inputs) = combine_args(cli, "cbz")?;
    let (bytes, c) = pdf::build_cbz(&inputs, cli.flag("--strict"), max)?;
    write_combined(target, "cbz", &bytes, &c, cli.flag("--json"))
}

fn ocr_cmd(cli: &Cli, max: u64) -> Result<String> {
    cli.allow(&[], &["--copy", "--window"])?;
    let input = single_path(cli)?;
    let result = ocr::recognize(&ops::decode_full(input, max)?)?;
    let interactive = cli.flag("--copy") || cli.flag("--window");
    if interactive && result.text.trim().is_empty() {
        return Err("no text found in the image".into());
    }
    if cli.flag("--copy") {
        win::copy_text(&result.text)?;
    }
    if cli.flag("--window") {
        ui::text_window("Thumbwright OCR", &result.text);
    }
    Ok(result.text)
}

fn doctor_cmd(cli: &Cli, dir: &Path) -> Result<String> {
    cli.allow(&["--bundle"], &[])?;
    if cli.positional.len() > 1 {
        return Err("doctor takes at most one file".into());
    }
    let file = cli.positional.first().map(Path::new);
    let text = doctor::report(dir, file);
    match cli.opt("--bundle") {
        Some(b) => {
            doctor::bundle(Path::new(b), dir, &text)?;
            Ok(format!("{text}\n\nsupport bundle written to {b}"))
        }
        None => Ok(text),
    }
}

fn pick_color_cmd(cli: &Cli) -> Result<String> {
    cli.allow(&[], &[])?;
    cli.exactly(0)?;
    match eyedropper::pick()? {
        Some(rgb) => {
            let hex = eyedropper::hex(rgb);
            win::copy_text(&hex)?;
            Ok(hex)
        }
        None => Ok(String::new()),
    }
}

fn mcp_cmd(cli: &Cli) -> Result<String> {
    cli.allow(&[], &[])?;
    cli.exactly(0)?;
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    mcp::serve(stdin.lock(), stdout.lock()).map_err(|e| format!("mcp: {e}"))?;
    let _ = std::io::stdout().flush();
    Ok(String::new())
}

/// Runs `cli` when it names one of these commands; `None` otherwise.
pub fn run(cli: &Cli, dir: &Path, settings: &Settings, max: u64) -> Option<Result<String>> {
    Some(match cli.command.as_str() {
        "doctor" => doctor_cmd(cli, dir),
        "batch" => batch_cmd(cli, settings, dir),
        "prebuild" => prebuild_cmd(cli, settings),
        "pdf" => combine_pdf_cmd(cli, settings, max),
        "cbz" => combine_cbz_cmd(cli, max),
        "ocr" => ocr_cmd(cli, max),
        "rename" => rename_cmd(cli, max),
        "sort" => sort_cmd(cli, max),
        "upload" => upload_cmd(cli, dir),
        "upload-hosts" => (|| {
            cli.allow(&[], &["--open"])?;
            cli.exactly(0)?;
            let path = upload::ensure_hosts_file(dir)?;
            if cli.flag("--open") {
                verbs::open_in_editor(&path)?;
            }
            Ok(path.display().to_string())
        })(),
        "upload-history" => (|| {
            cli.allow(&[], &["--json"])?;
            cli.exactly(0)?;
            upload::history(cli.flag("--json"))
        })(),
        "pick-color" => pick_color_cmd(cli),
        "lock-screen" => (|| {
            cli.allow(&[], &[])?;
            verbs::lock_screen(single_path(cli)?, max)
        })(),
        "folder-icon" => (|| {
            cli.allow(&[], &[])?;
            verbs::folder_icon(single_path(cli)?, max)
        })(),
        "settings" => (|| {
            cli.allow(&[], &[])?;
            cli.exactly(0)?;
            verbs::settings(dir)
        })(),
        "--mcp" => mcp_cmd(cli),
        // Hidden: the isolated tag reader used by rename/sort (see `meta`).
        "__tags" => (|| {
            cli.allow(&[], &[])?;
            meta::tags_child(single_path(cli)?, max)
        })(),
        _ => return None,
    })
}
