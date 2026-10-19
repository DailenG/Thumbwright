//! Plain-language text for errors shown to menu users (`--notify`).
//!
//! `twctl` errors are strings like `C:\pics\a.txt: unsupported file type` or
//! `2 of 5 failed:\n<error>\n<error>`. [`explain`] turns one into a one-line sentence a
//! non-technical user understands, followed by the technical detail (which Ctrl+C in the
//! message box copies together with the rest of the text).

/// Most failing files listed in the details of a partial batch failure.
const MAX_LISTED: usize = 5;
/// Longest single technical detail kept (usage errors can carry the whole usage text).
const MAX_DETAIL: usize = 300;

/// What went wrong with one file or action, as far as the error text reveals.
#[derive(Debug, PartialEq, Eq)]
enum Reason {
    Unsupported,
    NoWriter(String),
    InUse,
    Denied,
    Panicked,
    TooLarge,
    Damaged,
    OverBudget,
    TimedOut,
    NotFound,
    DiskFull,
    NoText,
    TooSmall,
    Usage,
    Other,
}

impl Reason {
    fn sentence(&self) -> String {
        match self {
            Self::Unsupported => "Thumbwright can't open this kind of file.".into(),
            Self::NoWriter(ext) => format!("Thumbwright can't save files as .{ext}."),
            Self::InUse => "The file is open in another program. Close it and try again.".into(),
            Self::Denied => {
                "Windows denied access to the file or folder. Check that it isn't read-only or protected."
                    .into()
            }
            Self::Panicked => {
                "Thumbwright ran into a problem reading this file. The file may be damaged.".into()
            }
            Self::TooLarge => {
                "The file is larger than the size limit set in Thumbwright's settings.".into()
            }
            Self::Damaged => "The file looks damaged or incomplete.".into(),
            Self::OverBudget => "The image is too large to process safely.".into(),
            Self::TimedOut => "Reading this file took too long, so Thumbwright stopped.".into(),
            Self::NotFound => "The file is no longer there. It may have been moved or renamed.".into(),
            Self::DiskFull => "There isn't enough free space on the disk.".into(),
            Self::NoText => "No text was found in the image.".into(),
            Self::TooSmall => {
                "The image can't be made that small. Try a larger size limit.".into()
            }
            Self::Usage => "Thumbwright couldn't run this action (internal error).".into(),
            Self::Other => "Thumbwright couldn't finish this action.".into(),
        }
    }
}

/// Splits `C:\dir\file.ext: message` into the file name and the message; text without a
/// drive-letter path prefix comes back whole.
fn split_path(raw: &str) -> (Option<&str>, &str) {
    let b = raw.as_bytes();
    let drive =
        b.len() > 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/');
    if !drive {
        return (None, raw);
    }
    match raw[3..].find(": ") {
        Some(i) => {
            let path = &raw[..3 + i];
            let name = path.rsplit(['\\', '/']).next().filter(|n| !n.is_empty());
            (name, &raw[3 + i + 2..])
        }
        None => (None, raw),
    }
}

fn classify(msg: &str) -> Reason {
    let m = msg.to_ascii_lowercase();
    let has = |s: &str| m.contains(s);
    if let Some(rest) = m.strip_prefix("no writer for .") {
        return Reason::NoWriter(rest.trim().to_owned());
    }
    if has("unsupported file type") || has("unsupported input") {
        Reason::Unsupported
    } else if has("(os error 32)") || has("used by another process") {
        Reason::InUse
    } else if has("(os error 5)") || has("access is denied") || has("permission denied") {
        Reason::Denied
    } else if has("decoder panicked") {
        Reason::Panicked
    } else if has("larger than the max file size") {
        Reason::TooLarge
    } else if has("malformed input") {
        Reason::Damaged
    } else if has("exceeds resource budget") {
        Reason::OverBudget
    } else if has("time budget exceeded") {
        Reason::TimedOut
    } else if has("(os error 2)")
        || has("(os error 3)")
        || has("cannot find the file")
        || has("cannot find the path")
        || has("no such file")
    {
        Reason::NotFound
    } else if has("(os error 112)") || has("not enough space") {
        Reason::DiskFull
    } else if has("no text found") {
        Reason::NoText
    } else if has("cannot compress below") {
        Reason::TooSmall
    } else if has("takes exactly")
        || has("needs at least one")
        || has("does not take")
        || has("unknown option")
        || has("unknown command")
        || has("needs a value")
        || has("bad --")
        || has(" needs --")
        || has(" needs <")
        || m.starts_with("usage:")
    {
        Reason::Usage
    } else {
        Reason::Other
    }
}

/// First line of `raw`, shortened to [`MAX_DETAIL`] characters.
fn one_line(raw: &str) -> String {
    let first = raw.lines().next().unwrap_or("").trim();
    if first.chars().count() > MAX_DETAIL {
        let cut: String = first.chars().take(MAX_DETAIL).collect();
        format!("{cut}...")
    } else {
        first.to_owned()
    }
}

/// `N of M failed:` header of a partial batch failure.
fn batch_header(line: &str) -> Option<(usize, usize)> {
    let rest = line.trim().strip_suffix(" failed:")?;
    let (n, m) = rest.split_once(" of ")?;
    Some((n.trim().parse().ok()?, m.trim().parse().ok()?))
}

fn single(raw: &str) -> String {
    let line = one_line(raw);
    let (name, msg) = split_path(&line);
    let reason = classify(msg);
    let sentence = match name {
        Some(n) if reason != Reason::Usage => format!("{n}: {}", reason.sentence()),
        _ => reason.sentence(),
    };
    format!("{sentence}\nDetails: {line}")
}

/// Plain-language message: a sentence, then the technical detail.
pub fn explain(raw: &str) -> String {
    let raw = raw.trim();
    let mut lines = raw.lines();
    let header = lines.next().and_then(batch_header);
    let Some((failed, total)) = header else {
        return single(raw);
    };
    // The failing items are the lines up to the "done:" list (rename/sort) and without the
    // trailing "note:" lines (rotate).
    let errors: Vec<&str> = lines
        .take_while(|l| l.trim() != "done:")
        .filter(|l| !l.starts_with("note: ") && !l.trim().is_empty())
        .collect();
    if failed == 1 && total == 1 && errors.len() == 1 {
        return single(errors[0]);
    }
    let reasons: Vec<Reason> = errors.iter().map(|e| classify(split_path(e).1)).collect();
    let same = reasons.first().filter(|r| reasons.iter().all(|o| o == *r));
    let lead = if failed == total {
        format!("None of the {total} files could be processed.")
    } else {
        format!("{failed} of {total} files could not be processed.")
    };
    let sentence = match same {
        Some(r) => format!("{lead} {}", r.sentence()),
        None => format!("{lead} See the details below."),
    };
    let mut out = format!("{sentence}\nDetails:");
    for e in errors.iter().take(MAX_LISTED) {
        out.push_str("\n- ");
        out.push_str(&one_line(e));
    }
    if errors.len() > MAX_LISTED {
        out.push_str(&format!("\n...and {} more", errors.len() - MAX_LISTED));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_line(s: &str) -> &str {
        s.lines().next().unwrap()
    }

    #[test]
    fn maps_the_common_errors_to_plain_sentences() {
        let cases = [
            (
                r"C:\pics\notes.txt: unsupported file type",
                "notes.txt: Thumbwright can't open this kind of file.",
            ),
            (
                r"C:\pics\a.zip: unsupported input",
                "a.zip: Thumbwright can't open this kind of file.",
            ),
            (
                "no writer for .heic",
                "Thumbwright can't save files as .heic.",
            ),
            (
                r"C:\p\a.jpg: The process cannot access the file because it is being used by another process. (os error 32)",
                "a.jpg: The file is open in another program. Close it and try again.",
            ),
            (
                r"C:\p\a.jpg: Access is denied. (os error 5)",
                "a.jpg: Windows denied access to the file or folder. Check that it isn't read-only or protected.",
            ),
            (
                "decoder panicked",
                "Thumbwright ran into a problem reading this file. The file may be damaged.",
            ),
            (
                r"C:\p\big.tif: larger than the max file size",
                "big.tif: The file is larger than the size limit set in Thumbwright's settings.",
            ),
            (
                r"C:\p\x.png: malformed input: bad chunk",
                "x.png: The file looks damaged or incomplete.",
            ),
            (
                r"C:\p\x.png: The system cannot find the file specified. (os error 2)",
                "x.png: The file is no longer there. It may have been moved or renamed.",
            ),
            (
                "no text found in the image",
                "No text was found in the image.",
            ),
        ];
        for (raw, want) in cases {
            let got = explain(raw);
            assert_eq!(first_line(&got), want, "{raw}");
            assert_eq!(got.lines().nth(1).unwrap(), format!("Details: {raw}"));
        }
    }

    #[test]
    fn os_error_5_does_not_match_longer_codes() {
        assert_eq!(classify("x (os error 50)"), Reason::Other);
        assert_eq!(classify("x (os error 5)"), Reason::Denied);
        assert_eq!(classify("x (os error 32)"), Reason::InUse);
    }

    #[test]
    fn usage_errors_never_show_usage_text() {
        for raw in [
            "convert takes exactly 2 file argument(s)",
            "resize needs --to",
            "unknown option --frobnicate",
            "convert-to needs at least one file",
            "unknown command nope\nusage: twctl <command> [args]\n  thumbnail <in> <out.png>",
        ] {
            let got = explain(raw);
            assert_eq!(
                first_line(&got),
                "Thumbwright couldn't run this action (internal error).",
                "{raw}"
            );
            assert!(got.contains("\nDetails: "), "{got}");
            assert!(!got.contains("thumbnail <in>"), "{got}");
            assert_eq!(got.lines().count(), 2, "{got}");
        }
    }

    #[test]
    fn unknown_errors_get_a_generic_sentence_with_the_detail() {
        let got = explain("something odd happened");
        assert_eq!(
            got,
            "Thumbwright couldn't finish this action.\nDetails: something odd happened"
        );
    }

    #[test]
    fn a_single_failed_file_in_a_batch_reads_like_a_single_error() {
        let got = explain("1 of 1 failed:\nC:\\pics\\notes.txt: unsupported file type");
        assert_eq!(
            got,
            "notes.txt: Thumbwright can't open this kind of file.\nDetails: C:\\pics\\notes.txt: unsupported file type"
        );
    }

    #[test]
    fn partial_batch_failure_counts_and_lists_the_files() {
        let got = explain(
            "2 of 5 failed:\nC:\\p\\a.txt: unsupported file type\nC:\\p\\b.txt: unsupported file type",
        );
        assert_eq!(
            got,
            "2 of 5 files could not be processed. Thumbwright can't open this kind of file.\nDetails:\n- C:\\p\\a.txt: unsupported file type\n- C:\\p\\b.txt: unsupported file type"
        );
        let mixed = explain(
            "2 of 4 failed:\nC:\\p\\a.txt: unsupported file type\nC:\\p\\b.png: malformed input: x",
        );
        assert_eq!(
            first_line(&mixed),
            "2 of 4 files could not be processed. See the details below."
        );
        let all = explain(
            "2 of 2 failed:\nC:\\p\\a.txt: unsupported file type\nC:\\p\\b.png: malformed input: x",
        );
        assert_eq!(
            first_line(&all),
            "None of the 2 files could be processed. See the details below."
        );
    }

    #[test]
    fn long_failure_lists_are_cut_and_done_lists_and_notes_ignored() {
        let mut raw = String::from("7 of 9 failed:");
        for i in 0..7 {
            raw.push_str(&format!("\nC:\\p\\{i}.txt: unsupported file type"));
        }
        raw.push_str("\ndone:\nC:\\p\\x.jpg -> C:\\p\\y.jpg");
        let got = explain(&raw);
        assert_eq!(got.matches("\n- ").count(), MAX_LISTED, "{got}");
        assert!(got.ends_with("...and 2 more"), "{got}");
        assert!(!got.contains("y.jpg"), "{got}");

        let notes = explain(
            "1 of 3 failed:\nC:\\p\\a.txt: unsupported file type\nnote: C:\\p\\b.jpg: kept EXIF",
        );
        assert!(!notes.contains("kept EXIF"), "{notes}");
    }

    #[test]
    fn split_path_needs_a_drive_prefix() {
        assert_eq!(split_path(r"C:\a b\c.png: bad"), (Some("c.png"), "bad"));
        assert_eq!(split_path("no writer for .x"), (None, "no writer for .x"));
        assert_eq!(split_path(r"C:\only-a-path"), (None, r"C:\only-a-path"));
    }

    #[test]
    fn long_details_are_shortened() {
        let raw = format!("C:\\p\\a.png: {}", "x".repeat(1000));
        let got = explain(&raw);
        assert!(got.lines().nth(1).unwrap().len() < MAX_DETAIL + 20, "{got}");
        assert!(got.ends_with("..."), "{got}");
    }
}
