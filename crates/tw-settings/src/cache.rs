//! "Rebuild thumbnail cache": Windows has no API that purges the Explorer thumbnail cache
//! (`IThumbnailCache` can only add and read entries), and Explorer keeps the
//! `thumbcache_*.db` files open. The documented route is Disk Cleanup; this app does the
//! equivalent without `cleanmgr`: end this user's Explorer processes, delete the cache
//! files, and make sure the shell is running again. Windows normally restarts a shell that
//! exits; if it has not done so shortly after, this starts `explorer.exe` itself.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess,
    WaitForSingleObject,
};

/// Time Explorer gets to exit after `TerminateProcess` before the delete is attempted anyway.
const EXIT_WAIT: Duration = Duration::from_secs(10);
/// How long deletion is retried while a dying Explorer still holds the files.
const DELETE_RETRY: Duration = Duration::from_secs(4);
/// How long Windows gets to restart the shell by itself before this app does it.
const SHELL_RESTART_WAIT: Duration = Duration::from_secs(6);

/// `%LOCALAPPDATA%\Microsoft\Windows\Explorer`, where the OS keeps the thumbnail cache.
pub fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(Path::new(&base).join(r"Microsoft\Windows\Explorer"))
}

fn is_cache_file(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with("thumbcache_") && n.ends_with(".db")
}

/// The `thumbcache_*.db` files in `dir`.
pub fn cache_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter(|e| e.file_name().to_str().is_some_and(is_cache_file))
        .map(|e| e.path())
        .collect()
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Cleared {
    pub deleted: usize,
    /// Files that are still locked or otherwise could not be deleted.
    pub failed: usize,
}

/// Deletes every cache file in `dir`; files that cannot be deleted (open elsewhere) are
/// counted, not fatal.
pub fn delete_cache_files(dir: &Path) -> Cleared {
    let mut out = Cleared::default();
    for file in cache_files(dir) {
        match std::fs::remove_file(&file) {
            Ok(()) => out.deleted += 1,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(_) => out.failed += 1,
        }
    }
    out
}

struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the handle came from OpenProcess/CreateToolhelp32Snapshot and is closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn own_session() -> Option<u32> {
    let mut session = 0;
    // SAFETY: `session` is a valid out pointer.
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) }.ok()?;
    Some(session)
}

/// Process ids of every `explorer.exe` in this user's session.
fn explorer_pids() -> Vec<u32> {
    let Some(session) = own_session() else {
        return Vec::new();
    };
    // SAFETY: a process snapshot walked with correctly sized PROCESSENTRY32W structs.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return Vec::new();
        };
        let snap = Owned(snap);
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut pids = Vec::new();
        let mut more = Process32FirstW(snap.0, &mut entry).is_ok();
        while more {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
            let mut their_session = u32::MAX;
            if name.eq_ignore_ascii_case("explorer.exe")
                && ProcessIdToSessionId(entry.th32ProcessID, &mut their_session).is_ok()
                && their_session == session
            {
                pids.push(entry.th32ProcessID);
            }
            more = Process32NextW(snap.0, &mut entry).is_ok();
        }
        pids
    }
}

/// Ends the process and waits for it to be gone.
fn terminate(pid: u32) -> Result<(), String> {
    // SAFETY: plain handle use; the handle is owned and closed by `Owned`.
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, false, pid)
            .map_err(|e| format!("cannot open Explorer (pid {pid}): {e}"))?;
        let h = Owned(h);
        TerminateProcess(h.0, 1).map_err(|e| format!("cannot end Explorer (pid {pid}): {e}"))?;
        if WaitForSingleObject(h.0, EXIT_WAIT.as_millis() as u32) != WAIT_OBJECT_0 {
            return Err(format!("Explorer (pid {pid}) did not exit"));
        }
    }
    Ok(())
}

fn wait_for_shell(limit: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if !explorer_pids().is_empty() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    !explorer_pids().is_empty()
}

/// Ends this user's Explorer, deletes the thumbnail cache files, and restarts the shell.
/// Returns a summary for the user. Nothing happens (and Explorer is left alone) when there
/// are no cache files.
pub fn rebuild() -> Result<String, String> {
    let dir = cache_dir().ok_or("LOCALAPPDATA is not set")?;
    if cache_files(&dir).is_empty() {
        return Ok("The thumbnail cache is already empty.".into());
    }
    let mut problems = Vec::new();
    for pid in explorer_pids() {
        if let Err(e) = terminate(pid) {
            problems.push(e);
        }
    }
    let mut cleared = delete_cache_files(&dir);
    let start = Instant::now();
    while cleared.failed > 0 && start.elapsed() < DELETE_RETRY {
        std::thread::sleep(Duration::from_millis(400));
        let again = delete_cache_files(&dir);
        cleared = Cleared {
            deleted: cleared.deleted + again.deleted,
            failed: again.failed,
        };
    }
    if !wait_for_shell(SHELL_RESTART_WAIT) {
        let explorer = std::env::var_os("SystemRoot")
            .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from)
            .join("explorer.exe");
        if let Err(e) = std::process::Command::new(explorer).spawn() {
            problems.push(format!("cannot restart Explorer: {e}"));
        }
    }
    let mut msg = format!(
        "Deleted {} thumbnail cache file(s); Windows rebuilds thumbnails as folders are viewed.",
        cleared.deleted
    );
    if cleared.failed > 0 {
        msg.push_str(&format!(
            "\n{} file(s) are still in use and were kept; sign out and run this again.",
            cleared.failed
        ));
    }
    if problems.is_empty() {
        Ok(msg)
    } else {
        Err(format!("{msg}\n{}", problems.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "tw-cache-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn only_thumbnail_cache_databases_are_deleted() {
        let dir = TempDir::new();
        for name in [
            "thumbcache_32.db",
            "thumbcache_idx.db",
            "THUMBCACHE_256.DB",
            "iconcache_32.db",
            "thumbcache_notes.txt",
            "mythumbcache_1.db",
        ] {
            std::fs::write(dir.0.join(name), b"x").unwrap();
        }
        // A directory whose name matches is not a cache file.
        std::fs::create_dir(dir.0.join("thumbcache_dir.db")).unwrap();

        assert_eq!(
            delete_cache_files(&dir.0),
            Cleared {
                deleted: 3,
                failed: 0
            }
        );
        let mut left: Vec<String> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "iconcache_32.db",
                "mythumbcache_1.db",
                "thumbcache_dir.db",
                "thumbcache_notes.txt"
            ]
        );
    }

    #[test]
    fn a_locked_cache_file_is_counted_and_does_not_stop_the_rest() {
        let dir = TempDir::new();
        std::fs::write(dir.0.join("thumbcache_16.db"), b"x").unwrap();
        std::fs::write(dir.0.join("thumbcache_96.db"), b"x").unwrap();
        // Share mode 0: nobody may delete or open the file while this handle lives, like
        // Explorer's own handle.
        let _held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(dir.0.join("thumbcache_16.db"))
            .unwrap();
        assert_eq!(
            delete_cache_files(&dir.0),
            Cleared {
                deleted: 1,
                failed: 1
            }
        );
        assert!(dir.0.join("thumbcache_16.db").exists());
    }

    #[test]
    fn missing_cache_directory_is_empty() {
        assert!(cache_files(Path::new(r"C:\definitely\not\here")).is_empty());
        assert_eq!(
            delete_cache_files(Path::new(r"C:\definitely\not\here")),
            Cleared::default()
        );
    }
}
