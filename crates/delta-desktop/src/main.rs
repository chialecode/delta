// GUI subsystem: launching the desktop app (Start menu, shortcut, Explorer) must not open a
// console window. Parent processes that pipe stdout (gates, installer `--build-info`) still get
// the output through the inherited handles.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::process::ExitCode;

fn main() -> ExitCode {
    attach_parent_console_for_text_flags();
    delta_desktop::run_desktop_entrypoint()
}

/// When a text-only flag (`--version`, `--help`, `--ui-smoke`, ...) is typed in a terminal, a
/// GUI-subsystem process has no stdout; attach to the parent console so the text is visible.
/// Redirected/piped handles are left untouched.
#[cfg(windows)]
fn attach_parent_console_for_text_flags() {
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        System::Console::{AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE},
    };
    let text_flag = std::env::args().skip(1).any(|a| {
        matches!(
            a.as_str(),
            "--help" | "-h" | "--version" | "-V" | "--build-info" | "--restore-args"
        ) || a.ends_with("-ui-smoke")
            || a.ends_with("-ui-bench")
    });
    if !text_flag {
        return;
    }
    // SAFETY: plain Win32 calls without pointers; failure only means no console is available.
    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

#[cfg(not(windows))]
fn attach_parent_console_for_text_flags() {}
