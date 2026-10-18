//! File metadata for the rename and sort verbs: EXIF date and camera, audio tags.
//!
//! Audio tags are parsed by `lofty`, which a hostile file can drive into unbounded
//! recursion. They are therefore read by a child copy of `twctl` (hidden `__tags`
//! command) under a wall-clock limit, so a crash or hang ends the child, not the verb.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::ops::Result;

/// Wall-clock budget for the tag-reading child.
const TAGS_TIMEOUT: Duration = Duration::from_secs(15);
/// Largest tag-child output read back (a handful of short fields).
const TAGS_OUTPUT_MAX: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DateTime {
    pub year: u32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl DateTime {
    /// EXIF `YYYY:MM:DD HH:MM:SS` (also tolerates `-` and `T` separators).
    pub fn parse_exif(s: &str) -> Option<Self> {
        let s = s.trim().trim_matches('\0');
        let b = s.as_bytes();
        if b.len() < 19 {
            return None;
        }
        let num = |a: usize, z: usize| s.get(a..z)?.parse::<u32>().ok();
        let dt = Self {
            year: num(0, 4)?,
            month: num(5, 7)?,
            day: num(8, 10)?,
            hour: num(11, 13)?,
            minute: num(14, 16)?,
            second: num(17, 19)?,
        };
        let sep_ok = matches!(b[4], b':' | b'-')
            && matches!(b[7], b':' | b'-')
            && matches!(b[10], b' ' | b'T')
            && b[13] == b':'
            && b[16] == b':';
        let valid = (1..=12).contains(&dt.month)
            && (1..=31).contains(&dt.day)
            && dt.hour < 24
            && dt.minute < 60
            && dt.second < 61
            && dt.year >= 1;
        (sep_ok && valid).then_some(dt)
    }

    pub fn date(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    pub fn time(&self) -> String {
        format!("{:02}.{:02}.{:02}", self.hour, self.minute, self.second)
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct Exif {
    pub taken: Option<DateTime>,
    pub make: Option<String>,
    pub model: Option<String>,
}

fn field_text(e: &exif::Exif, tag: exif::Tag) -> Option<String> {
    let f = e.get_field(tag, exif::In::PRIMARY)?;
    match &f.value {
        exif::Value::Ascii(parts) => {
            let s = parts
                .iter()
                .map(|p| {
                    String::from_utf8_lossy(p)
                        .trim_matches(['\0', ' '])
                        .to_owned()
                })
                .collect::<Vec<_>>()
                .join(" ");
            (!s.is_empty()).then_some(s)
        }
        _ => None,
    }
}

/// EXIF from a JPEG/TIFF/PNG/WebP/HEIF container; empty when there is none.
pub fn read_exif(path: &Path) -> Exif {
    let Ok(file) = std::fs::File::open(path) else {
        return Exif::default();
    };
    let mut reader = std::io::BufReader::new(file);
    let Ok(e) = exif::Reader::new().read_from_container(&mut reader) else {
        return Exif::default();
    };
    let taken = [
        exif::Tag::DateTimeOriginal,
        exif::Tag::DateTimeDigitized,
        exif::Tag::DateTime,
    ]
    .into_iter()
    .find_map(|t| field_text(&e, t).and_then(|s| DateTime::parse_exif(&s)));
    Exif {
        taken,
        make: field_text(&e, exif::Tag::Make),
        model: field_text(&e, exif::Tag::Model),
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct Tags {
    pub artist: Option<String>,
    pub title: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub track: Option<u32>,
    pub year: Option<u32>,
}

/// Hidden `__tags <file>` command: parses in this (child) process, prints one JSON object.
pub fn tags_child(path: &Path, max_bytes: u64) -> Result<String> {
    use lofty::config::ParseOptions;
    use lofty::file::TaggedFileExt;
    use lofty::probe::Probe;
    use lofty::tag::Accessor;

    let data = crate::ops::read_input(path, max_bytes)?;
    let tagged = Probe::new(std::io::Cursor::new(data))
        .options(ParseOptions::new().read_properties(false))
        .guess_file_type()
        .map_err(|e| e.to_string())?
        .read()
        .map_err(|e| e.to_string())?;
    let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) else {
        return Err(format!("{}: no audio tags", path.display()));
    };
    let text = |v: Option<std::borrow::Cow<'_, str>>| {
        v.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
    };
    let out = serde_json::json!({
        "artist": text(tag.artist()),
        "title": text(tag.title()),
        "album": text(tag.album()),
        "genre": text(tag.genre()),
        "track": tag.track(),
        "year": tag.date().map(|d| u32::from(d.year)),
    });
    Ok(out.to_string())
}

/// Audio tags of `path`, read in an isolated child.
pub fn read_tags(path: &Path) -> Result<Tags> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut child = Command::new(exe)
        .arg("__tags")
        .arg("--")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("tag reader: {e}"))?;
    let mut stdout = child.stdout.take().ok_or("tag reader: no output pipe")?;
    let mut stderr = child.stderr.take().ok_or("tag reader: no error pipe")?;
    let out_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = (&mut stdout).take(TAGS_OUTPUT_MAX).read_to_string(&mut s);
        s
    });
    let err_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = (&mut stderr).take(TAGS_OUTPUT_MAX).read_to_string(&mut s);
        s
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() > TAGS_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{}: reading tags timed out", path.display()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(format!("tag reader: {e}")),
        }
    };
    let out = out_thread.join().unwrap_or_default();
    let err = err_thread.join().unwrap_or_default();
    if !status.success() {
        let msg = err.lines().next().unwrap_or("tag reader failed");
        return Err(format!(
            "{}: {}",
            path.display(),
            msg.trim_start_matches("twctl: ")
        ));
    }
    let v: serde_json::Value =
        serde_json::from_str(&out).map_err(|e| format!("tag reader: {e}"))?;
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_owned);
    let n = |k: &str| {
        v.get(k)
            .and_then(serde_json::Value::as_u64)
            .and_then(|x| u32::try_from(x).ok())
    };
    Ok(Tags {
        artist: s("artist"),
        title: s("title"),
        album: s("album"),
        genre: s("genre"),
        track: n("track"),
        year: n("year"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exif_date_parsing() {
        let d = DateTime::parse_exif("2023:07:04 09:05:03\0").unwrap();
        assert_eq!(
            (d.date(), d.time()),
            ("2023-07-04".into(), "09.05.03".into())
        );
        assert!(DateTime::parse_exif("0000:00:00 00:00:00").is_none());
        assert!(DateTime::parse_exif("2023:13:04 09:05:03").is_none());
        assert!(DateTime::parse_exif("2023/07/04 09:05:03").is_none());
        assert!(DateTime::parse_exif("garbage").is_none());
    }
}
