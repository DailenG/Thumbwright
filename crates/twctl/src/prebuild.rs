//! `prebuild` / "Build thumbnails here": warms Explorer's thumbnail cache for a folder by
//! asking the OS cache for each supported file at each Explorer size. Extraction runs in the
//! registered surrogate exactly as it does when Explorer browses the folder, so this needs
//! the thumbnail handler to be registered to do anything useful.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use tw_core::config::Settings;
use tw_core::decode::route;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::UI::Shell::{
    IShellItem, IThumbnailCache, LocalThumbnailCache, SHCreateItemFromParsingName, WTS_CACHEFLAGS,
    WTS_EXTRACT, WTS_FLAGS, WTS_FORCEEXTRACTION, WTS_INCACHEONLY,
};
use windows::core::HSTRING;

use crate::ops::{self, Result};

/// One size per Explorer view (small icons, medium, extra large).
pub const DEFAULT_SIZES: [u32; 3] = [96, 256, 768];
const MAX_SIZE: u32 = 4096;
/// Failure samples shown in the summary.
const SAMPLE_FAILURES: usize = 5;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub files: usize,
    pub extracted: usize,
    pub already_cached: usize,
    /// File-size pairs that failed.
    pub failed: usize,
    /// Files with at least one failed size.
    pub failed_files: usize,
    /// Files processed before the run ended (fewer than `files` after Cancel).
    pub done: usize,
    pub skipped: usize,
    pub samples: Vec<String>,
}

impl Summary {
    pub fn text(&self, sizes: &[u32]) -> String {
        let sizes: Vec<String> = sizes.iter().map(u32::to_string).collect();
        let mut s = format!(
            "{} file(s) at {} px: {} extracted, {} already cached, {} failed, {} skipped (not a supported type)",
            self.files,
            sizes.join("/"),
            self.extracted,
            self.already_cached,
            self.failed,
            self.skipped
        );
        for f in &self.samples {
            s.push_str(&format!("\n  failed: {f}"));
        }
        s
    }

    /// One-line outcome for the auto-closing confirmation after the folder-menu entry.
    pub fn confirmation(&self, cancelled: bool) -> String {
        let ready = self.done.saturating_sub(self.failed_files);
        let mut s = if cancelled {
            format!(
                "Stopped: thumbnails ready for {ready} of {} files",
                self.files
            )
        } else if ready == 1 {
            "Thumbnails ready for 1 file".to_string()
        } else {
            format!("Thumbnails ready for {ready} files")
        };
        if self.failed_files > 0 {
            s.push_str(&format!(" ({} failed)", self.failed_files));
        }
        s
    }
}

/// `96,256,768` style list; each 16..=4096.
pub fn parse_sizes(spec: &str) -> Result<Vec<u32>> {
    let sizes: Vec<u32> = spec
        .split(',')
        .map(|p| {
            p.trim()
                .parse::<u32>()
                .ok()
                .filter(|n| (16..=MAX_SIZE).contains(n))
                .ok_or_else(|| format!("bad --size {spec} (comma-separated, 16-{MAX_SIZE})"))
        })
        .collect::<Result<_>>()?;
    Ok(sizes)
}

fn supported(p: &Path, settings: &Settings) -> bool {
    let ext = ops::ext_of(p);
    !route::chain_for_ext(&ext).is_empty() && settings.type_enabled(&ext)
}

/// Supported files under `input`: a file itself, a folder's children one level deep, or the
/// whole tree with `recurse`. The count of unsupported explicit files is returned too.
pub fn collect(
    inputs: &[PathBuf],
    recurse: bool,
    settings: &Settings,
) -> Result<(Vec<PathBuf>, usize)> {
    let (mut files, mut skipped) = (Vec::new(), 0);
    let mut stack: Vec<(PathBuf, bool)> = inputs.iter().map(|p| (p.clone(), true)).collect();
    while let Some((path, top)) = stack.pop() {
        let meta = std::fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.is_dir() {
            let read = std::fs::read_dir(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let mut children: Vec<PathBuf> =
                read.filter_map(|e| e.ok().map(|e| e.path())).collect();
            children.sort();
            // Pushed reversed so entries pop in name order.
            for child in children.into_iter().rev() {
                let is_dir = child.is_dir();
                if is_dir && !recurse {
                    continue;
                }
                stack.push((child, false));
            }
        } else if supported(&path, settings) {
            files.push(path);
        } else if top {
            skipped += 1;
        }
    }
    Ok((files, skipped))
}

/// One `GetThumbnail` call; the thumbnail itself is dropped (the cache keeps it).
fn get_thumbnail(
    cache: &IThumbnailCache,
    item: &IShellItem,
    size: u32,
    flags: WTS_FLAGS,
) -> windows::core::Result<()> {
    let mut bitmap = None;
    let mut out = WTS_CACHEFLAGS(0);
    // SAFETY: out-pointers reference live locals.
    unsafe { cache.GetThumbnail(item, size, flags, Some(&mut bitmap), Some(&mut out), None) }
    // The shared bitmap owns its HBITMAP; dropping it releases the pixels.
}

/// Outcome of warming one file at one size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Warmed {
    /// Already in the cache; nothing was extracted.
    Cached,
    /// Extracted through the handler and stored.
    Extracted,
}

fn warm_one(cache: &IThumbnailCache, path: &Path, size: u32, rebuild: bool) -> Result<Warmed> {
    let item: IShellItem = {
        // SAFETY: valid NUL-terminated wide path.
        unsafe {
            SHCreateItemFromParsingName(
                &HSTRING::from(crate::win::shell_path(path).as_os_str()),
                None,
            )
        }
        .map_err(|e| e.to_string())?
    };
    if !rebuild && get_thumbnail(cache, &item, size, WTS_INCACHEONLY).is_ok() {
        return Ok(Warmed::Cached);
    }
    let flags = if rebuild {
        WTS_FORCEEXTRACTION
    } else {
        WTS_EXTRACT
    };
    get_thumbnail(cache, &item, size, flags).map_err(|e| e.to_string())?;
    Ok(Warmed::Extracted)
}

/// Warms the cache for `files` at every size, on `jobs` threads. `progress` counts files and
/// stops the workers before their next file once Cancel was pressed.
pub fn warm(
    files: &[PathBuf],
    sizes: &[u32],
    rebuild_all: bool,
    jobs: usize,
    progress: &crate::feedback::Progress,
) -> Summary {
    let next = AtomicUsize::new(0);
    let extracted = AtomicUsize::new(0);
    let cached = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let failed_files = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let samples = Mutex::new(Vec::new());
    let workers = jobs.clamp(1, files.len().max(1));
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                // SAFETY: STA for the shell cache on this thread; balanced below.
                let inited = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
                // SAFETY: documented in-proc shell class.
                let cache: windows::core::Result<IThumbnailCache> =
                    unsafe { CoCreateInstance(&LocalThumbnailCache, None, CLSCTX_INPROC_SERVER) };
                loop {
                    if progress.cancelled() {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(file) = files.get(i) else { break };
                    let mut file_failed = false;
                    for &size in sizes {
                        let result = match &cache {
                            Ok(c) => warm_one(c, file, size, rebuild_all),
                            Err(e) => Err(e.to_string()),
                        };
                        match result {
                            Ok(Warmed::Cached) => cached.fetch_add(1, Ordering::Relaxed),
                            Ok(Warmed::Extracted) => extracted.fetch_add(1, Ordering::Relaxed),
                            Err(e) => {
                                failed.fetch_add(1, Ordering::Relaxed);
                                file_failed = true;
                                if let Ok(mut s) = samples.lock()
                                    && s.len() < SAMPLE_FAILURES
                                {
                                    s.push(format!("{} @{size}: {e}", file.display()));
                                }
                                0
                            }
                        };
                    }
                    if file_failed {
                        failed_files.fetch_add(1, Ordering::Relaxed);
                    }
                    done.fetch_add(1, Ordering::Relaxed);
                    progress.step();
                }
                if inited {
                    // SAFETY: balances the successful CoInitializeEx above.
                    unsafe { CoUninitialize() };
                }
            });
        }
    });
    Summary {
        files: files.len(),
        extracted: extracted.into_inner(),
        already_cached: cached.into_inner(),
        failed: failed.into_inner(),
        failed_files: failed_files.into_inner(),
        done: done.into_inner(),
        skipped: 0,
        samples: samples.into_inner().unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_list_validation() {
        assert_eq!(parse_sizes("96, 256,768").unwrap(), [96, 256, 768]);
        assert!(parse_sizes("8").is_err());
        assert!(parse_sizes("96,").is_err());
        assert!(parse_sizes("5000").is_err());
    }

    #[test]
    fn confirmation_counts_files_not_sizes() {
        let s = |done, failed_files| Summary {
            files: 10,
            done,
            failed_files,
            failed: failed_files * 3,
            ..Summary::default()
        };
        assert_eq!(
            s(10, 0).confirmation(false),
            "Thumbnails ready for 10 files"
        );
        assert_eq!(
            s(10, 2).confirmation(false),
            "Thumbnails ready for 8 files (2 failed)"
        );
        assert_eq!(
            s(4, 1).confirmation(true),
            "Stopped: thumbnails ready for 3 of 10 files (1 failed)"
        );
        let one = Summary {
            files: 1,
            done: 1,
            ..Summary::default()
        };
        assert_eq!(one.confirmation(false), "Thumbnails ready for 1 file");
    }

    #[test]
    fn collect_filters_by_type_and_honours_recursion() {
        let dir = std::env::temp_dir().join(format!("twctl-prebuild-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        for f in ["a.png", "b.txt", "sub\\c.png"] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        let s = Settings::default();
        let (flat, _) = collect(std::slice::from_ref(&dir), false, &s).unwrap();
        assert_eq!(flat, vec![dir.join("a.png")]);
        let (deep, _) = collect(std::slice::from_ref(&dir), true, &s).unwrap();
        assert_eq!(deep.len(), 2);
        let (_, skipped) = collect(&[dir.join("b.txt")], false, &s).unwrap();
        assert_eq!(skipped, 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
