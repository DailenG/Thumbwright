//! `thumbwright-settings.exe`: the Thumbwright settings window (FUNCTIONAL_SPEC.md section 8).
//!
//! `--self-test` creates the window hidden, drives it, destroys it, and exits 0 on success.

#![windows_subsystem = "windows"]

mod cache;
mod model;
mod ui;
mod win;

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use tw_core::config::Settings;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Console::{
    ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_OUTPUT_HANDLE,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_SYSTEM_AWARE, SetProcessDpiAwarenessContext,
};

const USAGE: &str = "usage: thumbwright-settings [--self-test]\n\
    Opens the Thumbwright settings window. --self-test creates the window hidden, exercises \
    it, destroys it, and exits.";

/// Prints a line. This is a GUI-subsystem program, so unless the parent redirected stdout
/// the process has no console handle; attach to the parent's console and write there.
fn out(line: &str) {
    // SAFETY: plain handle query and console attach.
    let have_stdout =
        unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }.is_ok_and(|h| !h.is_invalid() && !h.0.is_null());
    if have_stdout {
        let _ = writeln!(std::io::stdout(), "{line}");
        return;
    }
    // SAFETY: as above.
    let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    if let Ok(mut console) = std::fs::OpenOptions::new().write(true).open("CONOUT$") {
        let _ = writeln!(console, "{line}");
    }
}

fn exe_dir() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    exe.parent()
        .map(PathBuf::from)
        .ok_or_else(|| "cannot determine the program folder".to_string())
}

fn main() -> ExitCode {
    tw_core::log::init("settings");
    // One DPI for the process lifetime keeps the fixed layout consistent.
    // SAFETY: documented call, made before any window exists.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_SYSTEM_AWARE) };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] => exe_dir().and_then(|dir| {
            let settings = Settings::load(&dir);
            ui::run(dir, settings)
        }),
        ["--self-test"] => exe_dir()
            .and_then(ui::self_test)
            .map(|summary| out(&summary)),
        ["--help" | "-h"] => {
            out(USAGE);
            Ok(())
        }
        _ => {
            out(USAGE);
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tw_core::log::error(&format!("settings: {e}"));
            if args.is_empty() {
                win::error(HWND::default(), &e);
            } else {
                out(&format!("thumbwright-settings: {e}"));
            }
            ExitCode::FAILURE
        }
    }
}
