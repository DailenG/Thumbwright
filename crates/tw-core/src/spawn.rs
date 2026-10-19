//! Child-process creation with an explicit inheritance list (audit U-4, REQ-ISO-2).
//!
//! `std::process::Command` calls `CreateProcessW` with `bInheritHandles = TRUE` and no handle
//! list, so every inheritable handle that any component of the host process (Explorer, the
//! thumbnail surrogate) happens to hold also reaches the child. The worker parses hostile
//! bytes, and `twctl` runs on hostile file names; neither should see a handle it was not
//! given. The stable `std` API cannot restrict this (`raw_attribute` and
//! `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` support are unstable), so this module calls
//! `CreateProcessW` directly with a `STARTUPINFOEXW` whose
//! `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` names exactly the child's three standard handles:
//! each is either one end of an anonymous pipe or the `NUL` device.
//!
//! The child's ends are inheritable only from just before `CreateProcessW` until it
//! returns, and [`spawn`] serialises that window process-wide so two concurrent spawns never
//! see each other's ends. (A `CreateProcess(bInheritHandles = TRUE)` by some other component
//! inside that window can still inherit them; that is the residual exposure of every
//! handle-inheriting spawn and is microseconds wide.)

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::Path;
use std::sync::Mutex;

use windows::Win32::Foundation::{
    HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

/// `GENERIC_READ | GENERIC_WRITE`.
const GENERIC_READ_WRITE: u32 = 0xC000_0000;

/// What the child gets for a standard stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stdio {
    /// The `NUL` device.
    Null,
    /// One end of a new anonymous pipe; the parent keeps the other end in [`Child`].
    Piped,
}

/// A running child. Dropping it closes the process handle; it does not kill the process.
pub struct Child {
    process: OwnedHandle,
    pid: u32,
    /// Write end of the child's stdin pipe, when it was [`Stdio::Piped`].
    pub stdin: Option<File>,
    /// Read end of the child's stdout pipe, when it was [`Stdio::Piped`].
    pub stdout: Option<File>,
}

impl Child {
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// The process handle (valid while `self` lives), for `AssignProcessToJobObject`.
    pub fn as_raw_handle(&self) -> RawHandle {
        self.process.as_raw_handle()
    }

    /// The exit code if the process has exited, `None` while it runs.
    pub fn try_wait(&self) -> io::Result<Option<u32>> {
        self.wait_for(0)
    }

    /// Waits for the process to exit and returns its exit code.
    pub fn wait(&self) -> io::Result<u32> {
        self.wait_for(INFINITE).map(|c| c.unwrap_or(u32::MAX))
    }

    /// Terminates the process (a process that already exited is not an error).
    pub fn kill(&self) -> io::Result<()> {
        // SAFETY: valid process handle owned by `self`.
        match unsafe { TerminateProcess(HANDLE(self.process.as_raw_handle()), 1) } {
            Ok(()) => Ok(()),
            Err(_) if matches!(self.try_wait(), Ok(Some(_))) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn wait_for(&self, millis: u32) -> io::Result<Option<u32>> {
        let h = HANDLE(self.process.as_raw_handle());
        // SAFETY: valid process handle owned by `self`.
        let r = unsafe { WaitForSingleObject(h, millis) };
        if r == WAIT_TIMEOUT {
            return Ok(None);
        }
        if r != WAIT_OBJECT_0 {
            return Err(io::Error::last_os_error());
        }
        let mut code = 0u32;
        // SAFETY: valid process handle and out-pointer.
        unsafe { GetExitCodeProcess(h, &mut code)? };
        Ok(Some(code))
    }
}

/// Only one spawn at a time may have inheritable child-side handles outstanding.
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

/// Starts `exe` (an absolute path; no `PATH` search) with `args`, `CREATE_NO_WINDOW`, the
/// parent's environment and working directory, and the three standard handles described by
/// `stdin` and `stdout` (stderr is always `NUL`). Those three are the only handles the child
/// inherits.
pub fn spawn(exe: &Path, args: &[OsString], stdin: Stdio, stdout: Stdio) -> io::Result<Child> {
    let mut command_line = build_command_line(exe, args)?;
    let mut application: Vec<u16> = exe.as_os_str().encode_wide().collect();
    if application.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "nul in path"));
    }
    application.push(0);

    // Parent-side ends and the handles the child will hold, all created non-inheritable.
    let mut parent_stdin = None;
    let mut parent_stdout = None;
    let mut child_side: Vec<OwnedHandle> = Vec::new();
    let nul = open_nul()?;
    let nul_raw = nul.as_raw_handle();
    child_side.push(nul);

    let h_in = match stdin {
        Stdio::Null => nul_raw,
        Stdio::Piped => {
            let (read, write) = pipe()?;
            let raw = read.as_raw_handle();
            child_side.push(read);
            parent_stdin = Some(File::from(write));
            raw
        }
    };
    let h_out = match stdout {
        Stdio::Null => nul_raw,
        Stdio::Piped => {
            let (read, write) = pipe()?;
            let raw = write.as_raw_handle();
            child_side.push(write);
            parent_stdout = Some(File::from(read));
            raw
        }
    };
    let h_err = nul_raw;

    // The list must not repeat a handle, so the NUL device appears once however many
    // streams use it.
    let mut list: Vec<HANDLE> = Vec::new();
    for h in [h_in, h_out, h_err] {
        let h = HANDLE(h);
        if !list.contains(&h) {
            list.push(h);
        }
    }

    let _window = SPAWN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for h in &list {
        // SAFETY: valid handle owned by `child_side`.
        unsafe { SetHandleInformation(*h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT)? };
    }
    let attributes = AttributeList::handle_list(&list)?;
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = HANDLE(h_in);
    startup.StartupInfo.hStdOutput = HANDLE(h_out);
    startup.StartupInfo.hStdError = HANDLE(h_err);
    startup.lpAttributeList = attributes.as_ptr();
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: `application` and `command_line` are NUL-terminated UTF-16 buffers that live
    // across the call (`command_line` is writable, as CreateProcessW requires); `startup`
    // and its attribute list, and every listed handle, are valid for the call.
    let created = unsafe {
        CreateProcessW(
            PCWSTR(application.as_ptr()),
            Some(PWSTR(command_line.as_mut_ptr())),
            None,
            None,
            true,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW,
            None,
            PCWSTR::null(),
            &startup.StartupInfo,
            &mut info,
        )
    };
    // Closing the child's ends while still holding the lock ends their inheritable window.
    drop(attributes);
    drop(child_side);
    created?;
    // SAFETY: CreateProcessW returned two new handles that we now own.
    let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess.0) };
    drop(unsafe { OwnedHandle::from_raw_handle(info.hThread.0) });
    Ok(Child {
        process,
        pid: info.dwProcessId,
        stdin: parent_stdin,
        stdout: parent_stdout,
    })
}

/// A non-inheritable anonymous pipe: `(read end, write end)`.
fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let (mut read, mut write) = (HANDLE::default(), HANDLE::default());
    // SAFETY: valid out-pointers; no security attributes means non-inheritable ends.
    unsafe { CreatePipe(&mut read, &mut write, None, 0)? };
    // SAFETY: CreatePipe returned two new handles that we now own.
    Ok(unsafe {
        (
            OwnedHandle::from_raw_handle(read.0),
            OwnedHandle::from_raw_handle(write.0),
        )
    })
}

/// The `NUL` device, readable and writable, non-inheritable.
fn open_nul() -> io::Result<OwnedHandle> {
    let name: Vec<u16> = "NUL\0".encode_utf16().collect();
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        ..Default::default()
    };
    // SAFETY: `name` is NUL-terminated; `sa` is a valid non-inheritable descriptor.
    let h = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            GENERIC_READ_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            Some(&sa),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )?
    };
    // SAFETY: CreateFileW returned a new handle that we now own.
    Ok(unsafe { OwnedHandle::from_raw_handle(h.0) })
}

/// An initialised `PROC_THREAD_ATTRIBUTE_LIST` holding one `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`.
struct AttributeList {
    /// Backing storage, `usize`-aligned as the API requires pointer alignment.
    buffer: Vec<usize>,
    /// The handle array the attribute points into; must outlive `buffer`'s use.
    _handles: Vec<HANDLE>,
}

impl AttributeList {
    fn handle_list(handles: &[HANDLE]) -> io::Result<Self> {
        let mut size = 0usize;
        // SAFETY: the documented size query: a null list and a valid size out-pointer. It
        // reports ERROR_INSUFFICIENT_BUFFER, which is the expected outcome here.
        let _ = unsafe { InitializeProcThreadAttributeList(None, 1, None, &mut size) };
        if size == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = vec![0usize; size.div_ceil(size_of::<usize>())];
        let list = LPPROC_THREAD_ATTRIBUTE_LIST(buffer.as_mut_ptr().cast());
        // SAFETY: `buffer` is at least `size` bytes and pointer-aligned.
        unsafe { InitializeProcThreadAttributeList(Some(list), 1, None, &mut size)? };
        let me = Self {
            buffer,
            _handles: handles.to_vec(),
        };
        // SAFETY: the list is initialised; `_handles` is heap storage that lives as long as
        // the list and is not moved (a Vec's buffer does not move with the Vec).
        let updated = unsafe {
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(me._handles.as_ptr().cast()),
                std::mem::size_of_val(me._handles.as_slice()),
                None,
                None,
            )
        };
        // `me` owns the initialised list, so Drop deletes it on this error path too.
        updated?;
        Ok(me)
    }

    fn as_ptr(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        LPPROC_THREAD_ATTRIBUTE_LIST(self.buffer.as_ptr().cast_mut().cast())
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: the list was initialised by `handle_list` and is deleted exactly once.
        unsafe { DeleteProcThreadAttributeList(self.as_ptr()) };
    }
}

/// The command line `"exe" arg arg...`, quoted by the rules `CommandLineToArgvW` and the C
/// runtime undo (the same rules `std::process::Command` applies), NUL-terminated.
fn build_command_line(exe: &Path, args: &[OsString]) -> io::Result<Vec<u16>> {
    let mut line: Vec<u16> = Vec::new();
    let exe_wide: Vec<u16> = exe.as_os_str().encode_wide().collect();
    if exe_wide.contains(&u16::from(b'"')) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "quote in executable path",
        ));
    }
    line.push(u16::from(b'"'));
    line.extend_from_slice(&exe_wide);
    line.push(u16::from(b'"'));
    for arg in args {
        line.push(u16::from(b' '));
        let wide: Vec<u16> = arg.encode_wide().collect();
        if wide.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "nul in argument",
            ));
        }
        let quote = wide.is_empty()
            || wide
                .iter()
                .any(|&c| c == u16::from(b' ') || c == u16::from(b'\t'));
        if quote {
            line.push(u16::from(b'"'));
        }
        let mut backslashes = 0usize;
        for &c in &wide {
            if c == u16::from(b'\\') {
                backslashes += 1;
            } else {
                if c == u16::from(b'"') {
                    // 2n + 1 backslashes before an embedded quote.
                    line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes + 1));
                }
                backslashes = 0;
            }
            line.push(c);
        }
        if quote {
            // 2n backslashes before the closing quote.
            line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
            line.push(u16::from(b'"'));
        }
    }
    line.push(0);
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use windows::Win32::Foundation::{CloseHandle, GetHandleInformation};
    use windows::Win32::System::Threading::CreateEventW;

    fn line(args: &[&str]) -> String {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        let mut v = build_command_line(Path::new(r"C:\a b\x.exe"), &args).unwrap();
        assert_eq!(v.pop(), Some(0));
        String::from_utf16(&v).unwrap()
    }

    #[test]
    fn command_line_quoting_follows_the_msvcrt_rules() {
        assert_eq!(line(&[]), r#""C:\a b\x.exe""#);
        assert_eq!(line(&["a", "b"]), r#""C:\a b\x.exe" a b"#);
        assert_eq!(line(&[""]), r#""C:\a b\x.exe" """#);
        assert_eq!(line(&["a b"]), r#""C:\a b\x.exe" "a b""#);
        // A trailing backslash inside quotes is doubled so it cannot escape the quote.
        assert_eq!(line(&[r"C:\my dir\"]), r#""C:\a b\x.exe" "C:\my dir\\""#);
        assert_eq!(line(&[r"C:\dir\"]), r#""C:\a b\x.exe" C:\dir\"#);
        assert_eq!(line(&[r#"say "hi""#]), r#""C:\a b\x.exe" "say \"hi\"""#);
        assert_eq!(line(&[r#"a\"b"#]), r#""C:\a b\x.exe" a\\\"b"#);
    }

    #[test]
    fn rejects_nul_and_quote() {
        let nul = OsString::from("a\0b");
        assert!(build_command_line(Path::new("x.exe"), &[nul]).is_err());
        assert!(build_command_line(Path::new("x\".exe"), &[]).is_err());
    }

    /// Name of the env var that tells [`probe_child`] which handle value to examine. The value
    /// is `<parent pid>:<handle>`; the variable is process-wide in the parent too, so the child
    /// acts only when the parent pid is not its own (otherwise the parent's own run of
    /// `probe_child` would block reading its stdin).
    const PROBE_ENV: &str = "TW_SPAWN_TEST_PROBE_HANDLE";

    /// Runs as the *child* of the tests below (and as a no-op in a normal test run): reads one
    /// line from stdin, then reports whether the handle value named by the environment is a
    /// valid handle in this process.
    #[test]
    fn probe_child() {
        let Ok(value) = std::env::var(PROBE_ENV) else {
            return;
        };
        let (parent, handle) = value.split_once(':').unwrap();
        if parent.parse::<u32>().unwrap() == std::process::id() {
            return;
        }
        let raw: usize = handle.parse().unwrap();
        let mut said = String::new();
        std::io::stdin().read_line(&mut said).unwrap();
        let mut flags = 0u32;
        // SAFETY: probing a handle value for validity; no ownership is taken.
        let valid = unsafe { GetHandleInformation(HANDLE(raw as *mut _), &mut flags) }.is_ok();
        println!("PROBE echo={} valid={valid}", said.trim());
    }

    fn run_probe(restricted: bool) -> String {
        // An inheritable handle standing in for "some other component's inheritable handle".
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            bInheritHandle: true.into(),
            ..Default::default()
        };
        // SAFETY: valid security attributes; the returned handle is closed below.
        let event = unsafe { CreateEventW(Some(&sa), true, false, PCWSTR::null()) }.unwrap();
        // SAFETY: tests that read PROBE_ENV are this one and the child process; no other
        // thread of this process reads the environment concurrently in a way that matters.
        unsafe {
            std::env::set_var(
                PROBE_ENV,
                format!("{}:{}", std::process::id(), event.0 as usize),
            )
        };
        let exe = std::env::current_exe().unwrap();
        let args = [
            "spawn::tests::probe_child",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ];
        let output = if restricted {
            let args: Vec<OsString> = args.iter().map(OsString::from).collect();
            let mut child = spawn(&exe, &args, Stdio::Piped, Stdio::Piped).unwrap();
            writeln!(child.stdin.take().unwrap(), "ping").unwrap();
            let mut text = String::new();
            for l in BufReader::new(child.stdout.take().unwrap()).lines() {
                text.push_str(&l.unwrap());
                text.push('\n');
            }
            assert_eq!(child.wait().unwrap(), 0, "child failed:\n{text}");
            text
        } else {
            // Control: the same child through std, which inherits every inheritable handle.
            use std::process::{Command, Stdio as S};
            let mut child = Command::new(&exe)
                .args(args)
                .stdin(S::piped())
                .stdout(S::piped())
                .spawn()
                .unwrap();
            writeln!(child.stdin.take().unwrap(), "ping").unwrap();
            let out = child.wait_with_output().unwrap();
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        // SAFETY: removing the variable and closing the handle created above.
        unsafe {
            std::env::remove_var(PROBE_ENV);
            let _ = CloseHandle(event);
        }
        output
    }

    #[test]
    fn child_inherits_only_its_three_standard_handles() {
        let control = run_probe(false);
        assert!(
            control.contains("valid=true"),
            "control: std::process::Command is expected to leak an inheritable handle:\n{control}"
        );
        let restricted = run_probe(true);
        // stdin/stdout pipes work, and the unrelated inheritable handle did not arrive.
        assert!(
            restricted.contains("PROBE echo=ping valid=false"),
            "restricted spawn leaked a handle or lost stdio:\n{restricted}"
        );
    }

    #[test]
    fn exit_code_wait_and_kill() {
        let cmd = std::env::var_os("ComSpec")
            .map(std::path::PathBuf::from)
            .unwrap();
        let child = spawn(
            &cmd,
            &[OsString::from("/c"), OsString::from("exit 7")],
            Stdio::Null,
            Stdio::Null,
        )
        .unwrap();
        assert_eq!(child.wait().unwrap(), 7);
        assert_eq!(child.try_wait().unwrap(), Some(7));
        // Killing a process that already exited is not an error.
        child.kill().unwrap();

        // A child blocked on stdin can be killed.
        let child = spawn(
            &cmd,
            &[OsString::from("/c"), OsString::from("pause")],
            Stdio::Piped,
            Stdio::Null,
        )
        .unwrap();
        assert_eq!(child.try_wait().unwrap(), None);
        child.kill().unwrap();
        assert_eq!(child.wait().unwrap(), 1);
    }

    #[test]
    fn nonexistent_executable_is_an_error_not_a_panic() {
        let r = spawn(
            Path::new(r"C:\definitely\not\here.exe"),
            &[],
            Stdio::Null,
            Stdio::Null,
        );
        assert!(r.is_err());
    }
}
