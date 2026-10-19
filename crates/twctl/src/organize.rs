//! Rename and sort-into-folders verbs (FUNCTIONAL_SPEC.md 6.4). Every move is a
//! non-replacing `MoveFileExW`, so an existing file is never overwritten; a taken name gets
//! ` (1)`, ` (2)`, ... like every other output.

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS};
use windows::Win32::Storage::FileSystem::{MOVE_FILE_FLAGS, MoveFileExW};
use windows::core::HSTRING;

use crate::meta::{self, Exif, Tags};
use crate::ops::{self, Result};

/// Preset patterns for `rename --by`.
pub fn preset_pattern(by: &str) -> Option<&'static str> {
    Some(match by {
        "date" => "{date} {time}",
        "camera-date" => "{camera} {date} {time}",
        "artist-title" => "{artist} - {title}",
        "track-title" => "{track:2} - {title}",
        _ => return None,
    })
}

/// Characters Windows forbids in a file name, plus control characters, become `_`;
/// trailing dots and spaces are dropped; reserved device names get a leading `_`.
pub fn sanitize(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
                || (c as u32) < 0x20
            {
                '_'
            } else {
                c
            }
        })
        .collect();
    s = s.trim().trim_end_matches(['.', ' ']).to_owned();
    let base = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((base.len() == 4)
            && (base.starts_with("COM") || base.starts_with("LPT"))
            && base.as_bytes()[3].is_ascii_digit()
            && base.as_bytes()[3] != b'0');
    if reserved {
        s.insert(0, '_');
    }
    s
}

/// What a pattern needs from the file.
#[derive(Default)]
struct Needs {
    exif: bool,
    tags: bool,
    dims: bool,
}

fn needs(pattern: &str) -> Result<Needs> {
    let mut n = Needs::default();
    for token in tokens(pattern)? {
        match token.name.as_str() {
            "date" | "time" | "yyyy" | "MM" | "dd" | "HH" | "mm" | "ss" | "make" | "model"
            | "camera" => n.exif = true,
            "artist" | "title" | "album" | "track" | "genre" | "year" => n.tags = true,
            "w" | "h" => n.dims = true,
            "name" | "n" => {}
            other => return Err(format!("unknown pattern token {{{other}}}")),
        }
    }
    Ok(n)
}

struct Token {
    name: String,
}

fn tokens(pattern: &str) -> Result<Vec<Token>> {
    let mut out = Vec::new();
    let mut rest = pattern;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let close = after
            .find('}')
            .ok_or_else(|| "pattern has an unclosed {".to_string())?;
        let body = &after[..close];
        let name = match body.split_once(':') {
            Some((n, w)) => {
                w.parse::<usize>()
                    .ok()
                    .filter(|w| (1..=9).contains(w))
                    .ok_or_else(|| format!("bad width in {{{body}}}"))?;
                n
            }
            None => body,
        };
        out.push(Token {
            name: name.to_owned(),
        });
        rest = &after[close + 1..];
    }
    Ok(out)
}

/// Per-file values a pattern may reference.
struct Facts {
    stem: String,
    exif: Exif,
    tags: Tags,
    dims: Option<(u32, u32)>,
}

fn camera(e: &Exif) -> Option<String> {
    match (&e.make, &e.model) {
        (Some(make), Some(model)) if model.to_lowercase().starts_with(&make.to_lowercase()) => {
            Some(model.clone())
        }
        (Some(make), Some(model)) => Some(format!("{make} {model}")),
        (None, Some(m)) | (Some(m), None) => Some(m.clone()),
        (None, None) => None,
    }
}

fn expand(pattern: &str, f: &Facts, seq: usize, path: &Path) -> Result<String> {
    let missing = |what: &str| format!("{}: no {what}", path.display());
    let mut out = String::new();
    let mut rest = pattern;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let close = after.find('}').expect("validated by tokens()");
        let body = &after[..close];
        let (name, width) = body
            .split_once(':')
            .map_or((body, None), |(n, w)| (n, w.parse::<usize>().ok()));
        let w = width.unwrap_or(0);
        let dt = || f.exif.taken.ok_or_else(|| missing("EXIF date"));
        let text = |v: &Option<String>, what: &str| v.clone().ok_or_else(|| missing(what));
        let num =
            |v: Option<u32>, what: &str| v.map(|n| format!("{n:0w$}")).ok_or_else(|| missing(what));
        let piece = match name {
            "name" => f.stem.clone(),
            "n" => format!("{seq:0w$}"),
            "date" => dt()?.date(),
            "time" => dt()?.time(),
            "yyyy" => format!("{:04}", dt()?.year),
            "MM" => format!("{:02}", dt()?.month),
            "dd" => format!("{:02}", dt()?.day),
            "HH" => format!("{:02}", dt()?.hour),
            "mm" => format!("{:02}", dt()?.minute),
            "ss" => format!("{:02}", dt()?.second),
            "make" => text(&f.exif.make, "camera make")?,
            "model" => text(&f.exif.model, "camera model")?,
            "camera" => camera(&f.exif).ok_or_else(|| missing("camera make/model"))?,
            "artist" => text(&f.tags.artist, "artist tag")?,
            "title" => text(&f.tags.title, "title tag")?,
            "album" => text(&f.tags.album, "album tag")?,
            "genre" => text(&f.tags.genre, "genre tag")?,
            "track" => num(f.tags.track, "track tag")?,
            "year" => num(f.tags.year, "year tag")?,
            "w" => f.dims.ok_or_else(|| missing("image size"))?.0.to_string(),
            "h" => f.dims.ok_or_else(|| missing("image size"))?.1.to_string(),
            other => return Err(format!("unknown pattern token {{{other}}}")),
        };
        out.push_str(&piece);
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Moves `src` to `<dir>\<stem>.<ext>` (or ` (n)` variants); never replaces a file.
fn move_unique(src: &Path, dir: &Path, stem: &str, ext: &str) -> Result<PathBuf> {
    let src_w = HSTRING::from(src.as_os_str());
    let mut n = 0u32;
    loop {
        let file = match (n, ext.is_empty()) {
            (0, true) => stem.to_owned(),
            (0, false) => format!("{stem}.{ext}"),
            (n, true) => format!("{stem} ({n})"),
            (n, false) => format!("{stem} ({n}).{ext}"),
        };
        let dest = dir.join(file);
        // SAFETY: both arguments are valid NUL-terminated wide strings; flags 0 means the
        // move fails instead of replacing an existing destination.
        let moved =
            unsafe { MoveFileExW(&src_w, &HSTRING::from(dest.as_os_str()), MOVE_FILE_FLAGS(0)) };
        match moved {
            Ok(()) => return Ok(dest),
            Err(e)
                if n < u32::MAX
                    && (e.code() == ERROR_ALREADY_EXISTS.to_hresult()
                        || e.code() == ERROR_FILE_EXISTS.to_hresult()) =>
            {
                n += 1;
            }
            Err(e) => return Err(format!("{}: {e}", src.display())),
        }
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    a.as_os_str().eq_ignore_ascii_case(b.as_os_str())
}

fn image_dims(p: &Path, max: u64) -> Result<(u32, u32)> {
    if let Ok(r) = image::ImageReader::open(p)
        && let Ok(r) = r.with_guessed_format()
        && let Ok(d) = r.into_dimensions()
    {
        return Ok(d);
    }
    let img = ops::decode_full(p, max)?;
    Ok((img.width(), img.height()))
}

fn facts(p: &Path, need: &Needs, max: u64) -> Result<Facts> {
    Ok(Facts {
        stem: p
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        exif: if need.exif {
            meta::read_exif(p)
        } else {
            Exif::default()
        },
        tags: if need.tags {
            meta::read_tags(p)?
        } else {
            Tags::default()
        },
        dims: if need.dims {
            Some(image_dims(p, max)?)
        } else {
            None
        },
    })
}

/// Reports per-file failures after processing everything that could be processed.
fn finish(lines: Vec<String>, errors: Vec<String>, total: usize) -> Result<String> {
    if errors.is_empty() {
        Ok(lines.join("\n"))
    } else {
        Err(format!(
            "{} of {total} failed:\n{}{}",
            errors.len(),
            errors.join("\n"),
            if lines.is_empty() {
                String::new()
            } else {
                format!("\ndone:\n{}", lines.join("\n"))
            }
        ))
    }
}

/// `rename`: gives every file the name `pattern` expands to, keeping its extension.
pub fn rename(files: &[PathBuf], pattern: &str, max: u64) -> Result<String> {
    let need = needs(pattern)?;
    let (mut lines, mut errors) = (Vec::new(), Vec::new());
    for (i, f) in files.iter().enumerate() {
        let result = (|| {
            let facts = facts(f, &need, max)?;
            let stem = sanitize(&expand(pattern, &facts, i + 1, f)?);
            if stem.is_empty() {
                return Err(format!(
                    "{}: the pattern produced an empty name",
                    f.display()
                ));
            }
            let dir = f.parent().unwrap_or(Path::new("."));
            let ext = f
                .extension()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_default();
            let target = dir.join(if ext.is_empty() {
                stem.clone()
            } else {
                format!("{stem}.{ext}")
            });
            if same_path(f, &target) {
                return Ok(f.clone());
            }
            move_unique(f, dir, &stem, &ext)
        })();
        match result {
            Ok(to) => {
                lines.push(format!("{} -> {}", f.display(), to.display()));
                crate::feedback::created([to]);
            }
            Err(e) => errors.push(e),
        }
    }
    finish(lines, errors, files.len())
}

/// Folder name for `sort`.
fn sort_label(p: &Path, by: &str, tag: &str, max: u64) -> Result<String> {
    let label = match by {
        "size" => {
            let (w, h) = image_dims(p, max)?;
            format!("{w}x{h}")
        }
        "date" => meta::read_exif(p)
            .taken
            .map(|d| d.date())
            .ok_or_else(|| format!("{}: no EXIF date", p.display()))?,
        "tag" => {
            let t = meta::read_tags(p)?;
            let v = match tag {
                "artist" => t.artist,
                "album" => t.album,
                "genre" => t.genre,
                "year" => t.year.map(|y| y.to_string()),
                other => return Err(format!("bad --tag {other} (artist|album|genre|year)")),
            };
            v.ok_or_else(|| format!("{}: no {tag} tag", p.display()))?
        }
        other => return Err(format!("bad --by {other} (size|date|tag)")),
    };
    let label = sanitize(&label);
    if label.is_empty() {
        return Err(format!("{}: empty folder name", p.display()));
    }
    Ok(label)
}

/// `sort`: moves each file into a sibling folder named by image size, date taken or an
/// audio tag.
pub fn sort(files: &[PathBuf], by: &str, tag: &str, max: u64) -> Result<String> {
    let (mut lines, mut errors) = (Vec::new(), Vec::new());
    for f in files {
        let result = (|| {
            let label = sort_label(f, by, tag, max)?;
            let dir = f.parent().unwrap_or(Path::new(".")).join(&label);
            std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            let name = f.file_name().ok_or("bad file name")?.to_string_lossy();
            let (stem, ext) = match name.rsplit_once('.') {
                Some((s, e)) if !s.is_empty() => (s.to_owned(), e.to_owned()),
                _ => (name.to_string(), String::new()),
            };
            move_unique(f, &dir, &stem, &ext)
        })();
        match result {
            Ok(to) => {
                lines.push(format!("{} -> {}", f.display(), to.display()));
                // The new folders are what the user wants to see; one window for all.
                crate::feedback::created(to.parent().map(Path::to_path_buf));
            }
            Err(e) => errors.push(e),
        }
    }
    finish(lines, errors, files.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts_with(exif: Exif) -> Facts {
        Facts {
            stem: "IMG_0001".into(),
            exif,
            tags: Tags {
                artist: Some("A/B".into()),
                title: Some("Song".into()),
                track: Some(3),
                ..Tags::default()
            },
            dims: Some((640, 480)),
        }
    }

    #[test]
    fn sanitize_handles_forbidden_and_reserved_names() {
        assert_eq!(sanitize("a<b>:c/d\\e|f?g*h\"i"), "a_b__c_d_e_f_g_h_i");
        assert_eq!(sanitize("name. . "), "name");
        assert_eq!(sanitize("CON"), "_CON");
        assert_eq!(sanitize("lpt1.txt"), "_lpt1.txt");
        assert_eq!(sanitize("COM0"), "COM0");
    }

    #[test]
    fn pattern_expansion_and_missing_data() {
        let taken = meta::DateTime::parse_exif("2020:02:29 23:59:58").unwrap();
        let f = facts_with(Exif {
            taken: Some(taken),
            make: Some("Canon".into()),
            model: Some("Canon EOS R5".into()),
        });
        let p = Path::new("x.jpg");
        assert_eq!(
            expand("{camera} {date} {time}", &f, 1, p).unwrap(),
            "Canon EOS R5 2020-02-29 23.59.58"
        );
        assert_eq!(
            expand("{track:2} - {title} #{n:3}", &f, 7, p).unwrap(),
            "03 - Song #007"
        );
        let none = facts_with(Exif::default());
        assert!(
            expand("{date}", &none, 1, p)
                .unwrap_err()
                .contains("no EXIF date")
        );
        assert!(needs("{nope}").is_err());
        assert!(needs("{n:0}").is_err());
        assert!(needs("{date").is_err());
    }

    #[test]
    fn rename_never_replaces_and_numbers_collisions() {
        let dir = std::env::temp_dir().join(format!("twctl-rename-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (n, body) in [("a.txt", "A"), ("b.txt", "B"), ("keep.txt", "K")] {
            std::fs::write(dir.join(n), body).unwrap();
        }
        let files = vec![dir.join("a.txt"), dir.join("b.txt")];
        rename(&files, "keep", 1 << 20).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("keep.txt")).unwrap(), "K");
        let mut got: Vec<String> = ["keep (1).txt", "keep (2).txt"]
            .iter()
            .map(|n| std::fs::read_to_string(dir.join(n)).unwrap())
            .collect();
        got.sort();
        assert_eq!(got, ["A", "B"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sort_by_size_groups_into_dimension_folders() {
        let dir = std::env::temp_dir().join(format!("twctl-sort-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        image::RgbImage::new(4, 3).save(dir.join("a.png")).unwrap();
        image::RgbImage::new(4, 3).save(dir.join("b.png")).unwrap();
        image::RgbImage::new(2, 2).save(dir.join("c.png")).unwrap();
        let files: Vec<PathBuf> = ["a.png", "b.png", "c.png"]
            .iter()
            .map(|n| dir.join(n))
            .collect();
        sort(&files, "size", "artist", 1 << 20).unwrap();
        assert!(dir.join("4x3").join("a.png").is_file());
        assert!(dir.join("4x3").join("b.png").is_file());
        assert!(dir.join("2x2").join("c.png").is_file());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
