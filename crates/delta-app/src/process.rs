use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(windows)]
static ACTIVE_JOB: std::sync::atomic::AtomicPtr<std::ffi::c_void> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
static OBSERVED_WINDOWS: AtomicUsize = AtomicUsize::new(0);
static WINDOW_DETAILS: std::sync::Mutex<Vec<WindowEvidence>> = std::sync::Mutex::new(Vec::new());

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowEvidence {
    pub process_id: u32,
    pub title: String,
}

pub fn observed_window_details() -> Vec<WindowEvidence> {
    WINDOW_DETAILS.lock().unwrap().clone()
}

pub fn current_process_priority() -> String {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, GetPriorityClass, BELOW_NORMAL_PRIORITY_CLASS,
        };
        let actual = GetPriorityClass(GetCurrentProcess());
        if actual == BELOW_NORMAL_PRIORITY_CLASS {
            return "below_normal".into();
        }
        format!("windows_priority_class:{actual}")
    }
    #[cfg(not(windows))]
    {
        "platform_default".into()
    }
}

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuietCommandResult {
    pub program: String,
    pub args: Vec<String>,
    pub status_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Apply process-wide Windows quiet guards (`D-25` / S-10):
/// 1. `SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX)`
///    so crashing child processes never trigger a Windows Error Reporting GUI dialog.
/// 2. `SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS)`
///    so background builds/tests/sync jobs never starve foreground user interaction.
pub fn apply_process_quiet_guards() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Diagnostics::Debug::{
            SetErrorMode, SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SEM_NOOPENFILEERRORBOX,
        };
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, SetPriorityClass, BELOW_NORMAL_PRIORITY_CLASS,
        };
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX);
        let _ = SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS);
    }
}

/// Windows Job Object wrapper configured with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (S-10)
/// so any child process assigned to the job is automatically terminated if the parent exits.
pub struct QuietJobObject {
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
    #[cfg(windows)]
    holds_current_process: std::sync::atomic::AtomicBool,
}

impl QuietJobObject {
    pub fn new() -> Result<Self, String> {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::JobObjects::{
                CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            };
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err("CreateJobObjectW returned null".to_string());
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                CloseHandle(handle);
                return Err("SetInformationJobObject failed".to_string());
            }
            Ok(Self {
                handle,
                holds_current_process: std::sync::atomic::AtomicBool::new(false),
            })
        }
        #[cfg(not(windows))]
        {
            Ok(Self {})
        }
    }

    pub fn assign_current_process(&self) -> Result<(), String> {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
            use windows_sys::Win32::System::Threading::GetCurrentProcess;
            let ok = AssignProcessToJobObject(self.handle, GetCurrentProcess());
            if ok == 0 {
                return Err("AssignProcessToJobObject failed".into());
            }
            self.holds_current_process.store(true, Ordering::SeqCst);
            ACTIVE_JOB.store(self.handle, Ordering::SeqCst);
            Ok(())
        }
        #[cfg(not(windows))]
        {
            Ok(())
        }
    }

    fn assign_child(&self, child: &std::process::Child) -> Result<(), String> {
        #[cfg(windows)]
        unsafe {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
            if AssignProcessToJobObject(self.handle, child.as_raw_handle()) == 0 {
                return Err("AssignProcessToJobObject for child failed".into());
            }
        }
        #[cfg(not(windows))]
        let _ = child;
        Ok(())
    }

    fn terminate_child_tree(&self, child: &mut std::process::Child) -> Result<(), String> {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::System::JobObjects::TerminateJobObject;
            let _ = child;
            if TerminateJobObject(self.handle, 1) == 0 {
                return Err("TerminateJobObject for child failed".into());
            }
        }
        #[cfg(not(windows))]
        child.kill().map_err(|e| e.to_string())?;
        Ok(())
    }
}

impl Drop for QuietJobObject {
    fn drop(&mut self) {
        // Intentionally do NOT call CloseHandle here if the current process is assigned
        // to this Job Object with JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: closing the job handle
        // before `main()` returns would cause the kernel to terminate the current process
        // before stdout flushes or ExitCode is returned to ExitProcess.
        // The Windows kernel automatically closes the job handle when the process exits,
        // which triggers JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE for any remaining child processes.
        #[cfg(windows)]
        if !self.holds_current_process.load(Ordering::SeqCst) {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(self.handle);
            }
        }
    }
}

/// Unified quiet child process execution helper (`D-25` / S-10).
/// All other callers in the workspace are forbidden by `clippy.toml` from calling `std::process::Command::new`.
pub fn run_quiet_command<I, S>(
    program: &OsStr,
    args: I,
    current_dir: Option<&Path>,
    extra_env: &BTreeMap<String, String>,
) -> Result<QuietCommandResult, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_quiet_command_with_timeout(
        program,
        args,
        current_dir,
        extra_env,
        std::time::Duration::from_secs(300),
    )
}

/// Bound a command's entire Windows process tree, including descendants retaining pipes.
#[allow(clippy::disallowed_methods)]
pub fn run_quiet_command_with_timeout<I, S>(
    program: &OsStr,
    args: I,
    current_dir: Option<&Path>,
    extra_env: &BTreeMap<String, String>,
    timeout: std::time::Duration,
) -> Result<QuietCommandResult, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    apply_process_quiet_guards();
    if timeout.is_zero() || timeout > std::time::Duration::from_secs(300) {
        return Err("command timeout must be between zero and 300 seconds".into());
    }

    let mut cmd = Command::new(program);
    let mut arg_strings = Vec::new();
    for a in args {
        let os = a.as_ref();
        arg_strings.push(os.to_string_lossy().to_string());
        cmd.arg(os);
    }

    if let Some(dir) = current_dir {
        cmd.current_dir(dir);
    }

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env("NO_COLOR", "1")
        .env("CI", "1");

    for (k, v) in extra_env {
        cmd.env(k, v);
    }

    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let child_job = QuietJobObject::new()?;
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to execute {:?}: {e}", program))?;
    if let Err(error) = child_job.assign_child(&child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    use std::io::Read;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let err_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });
    let started = std::time::Instant::now();
    let mut timed_out = false;
    let status = loop {
        let visible = count_visible_windows_for_current_process();
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if visible > 0 || started.elapsed() >= timeout {
            timed_out = started.elapsed() >= timeout;
            child_job.terminate_child_tree(&mut child)?;
            break child.wait().map_err(|e| e.to_string())?;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    // A successful direct child may leave a background descendant holding stdout/stderr.
    // Close its private job before joining readers so those inherited pipes close too.
    drop(child_job);
    let output = Output {
        status,
        stdout: out_reader
            .join()
            .map_err(|_| "stdout reader panicked")?
            .map_err(|e| e.to_string())?,
        stderr: err_reader
            .join()
            .map_err(|_| "stderr reader panicked")?
            .map_err(|e| e.to_string())?,
    };

    Ok(QuietCommandResult {
        program: program.to_string_lossy().to_string(),
        args: arg_strings,
        status_code: output.status.code(),
        timed_out,
        stdout: sanitize_terminal_output(&String::from_utf8_lossy(&output.stdout)),
        stderr: sanitize_terminal_output(&String::from_utf8_lossy(&output.stderr)),
    })
}

/// Spawn a quiet background child process (e.g. for cross-process lock contention tests)
/// with `CREATE_NO_WINDOW`, null `stdin`, and piped `stdout`/`stderr`.
#[allow(clippy::disallowed_methods)]
pub fn spawn_quiet_child<I, S>(
    program: &OsStr,
    args: I,
    extra_env: &BTreeMap<String, String>,
) -> Result<std::process::Child, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    apply_process_quiet_guards();
    let mut cmd = Command::new(program);
    for a in args {
        cmd.arg(a.as_ref());
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env("NO_COLOR", "1")
        .env("CI", "1");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn()
        .map_err(|e| format!("failed to spawn {:?}: {e}", program))
}

/// Escape ASCII control characters (`\x07` BEL, `\x1b` ESC/ANSI sequences, etc.) from
/// untrusted strings before writing to logs or terminal output (S-10).
pub fn sanitize_terminal_output(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '\n' | '\r' | '\t' => out.push(ch),
            '\x07' => out.push_str("\\x07"),
            '\x1b' => out.push_str("\\x1b"),
            c if c.is_control() => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// Window sentinel (`EnumWindows` + `IsWindowVisible` + `GetWindowThreadProcessId`)
/// that counts visible top-level windows belonging to the current process (`D-25` / S-10).
pub fn count_visible_windows_for_current_process() -> usize {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::{HWND, LPARAM};
        use windows_sys::Win32::System::Threading::GetCurrentProcessId;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
        };

        struct CallbackCtx {
            target_pid: u32,
            count: AtomicUsize,
        }

        unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> i32 {
            let ctx = &*(lparam as *const CallbackCtx);
            if IsWindowVisible(hwnd) != 0 {
                let mut pid: u32 = 0;
                GetWindowThreadProcessId(hwnd, &mut pid);
                let mut belongs = pid == ctx.target_pid;
                let job = ACTIVE_JOB.load(Ordering::SeqCst);
                if !job.is_null() {
                    use windows_sys::Win32::System::JobObjects::IsProcessInJob;
                    use windows_sys::Win32::System::Threading::{
                        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
                    };
                    let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
                    if !process.is_null() {
                        let mut in_job = 0;
                        if IsProcessInJob(process, job, &mut in_job) != 0 {
                            belongs |= in_job != 0;
                        }
                        windows_sys::Win32::Foundation::CloseHandle(process);
                    }
                }
                if belongs {
                    ctx.count.fetch_add(1, Ordering::Relaxed);
                    let mut title = [0u16; 512];
                    let length = GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32).max(0)
                        as usize;
                    let title =
                        sanitize_terminal_output(&String::from_utf16_lossy(&title[..length]));
                    let mut details = WINDOW_DETAILS.lock().unwrap();
                    if !details
                        .iter()
                        .any(|w| w.process_id == pid && w.title == title)
                    {
                        details.push(WindowEvidence {
                            process_id: pid,
                            title,
                        });
                    }
                }
            }
            1
        }

        let ctx = CallbackCtx {
            target_pid: GetCurrentProcessId(),
            count: AtomicUsize::new(0),
        };
        let _ = EnumWindows(Some(enum_proc), &ctx as *const CallbackCtx as LPARAM);
        let observed = ctx.count.load(Ordering::Relaxed);
        OBSERVED_WINDOWS.fetch_max(observed, Ordering::SeqCst);
        OBSERVED_WINDOWS.load(Ordering::SeqCst)
    }
    #[cfg(not(windows))]
    {
        0
    }
}
