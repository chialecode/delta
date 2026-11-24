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
    WINDOW_DETAILS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
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

/// Power-throttling state recorded by native frame benchmarks.
pub const EXECUTION_SPEED_OPT_OUT: &str = "execution_speed_opt_out";

/// Opt this process out of Windows execution-speed power throttling (EcoQoS).
///
/// Windows 11 throttles processes whose windows are hidden or in the background. A native
/// frame benchmark runs in a hidden window (`D-25`), so without this opt-out it measures OS
/// background scheduling rather than the frame cost a user sees in a foreground window. The
/// process keeps its below-normal priority. Returns [`EXECUTION_SPEED_OPT_OUT`] on success.
pub fn opt_out_of_execution_speed_throttling() -> String {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation,
            PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            PROCESS_POWER_THROTTLING_STATE,
        };
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: 0,
        };
        let ok = SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            (&state as *const PROCESS_POWER_THROTTLING_STATE).cast(),
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
        if ok != 0 {
            EXECUTION_SPEED_OPT_OUT.into()
        } else {
            format!("unavailable:{}", std::io::Error::last_os_error())
        }
    }
    #[cfg(not(windows))]
    {
        "platform_default".into()
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
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            };
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err("CreateJobObjectW returned null".to_string());
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            // Only `spawn_detached_app` (explicit CREATE_BREAKAWAY_FROM_JOB) may leave the job:
            // an app relaunched after an update must outlive the CLI or app that started it.
            info.BasicLimitInformation.LimitFlags =
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
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

    fn count_visible_windows(&self) -> usize {
        #[cfg(windows)]
        {
            count_visible_windows_for_filter(None, Some(self.handle))
        }
        #[cfg(not(windows))]
        {
            0
        }
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
    const MAX_CAPTURED_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
    fn read_capped<R: std::io::Read>(mut reader: R, limit: usize) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut chunk = [0u8; 8192];
        let mut truncated = false;
        loop {
            let n = reader.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            if out.len() < limit {
                let take = (limit - out.len()).min(n);
                out.extend_from_slice(&chunk[..take]);
                if take < n {
                    truncated = true;
                }
            } else {
                truncated = true;
            }
        }
        if truncated {
            out.extend_from_slice(b"\n...[output truncated at 32 MiB cap]");
        }
        Ok(out)
    }

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let out_reader = std::thread::spawn(move || read_capped(stdout, MAX_CAPTURED_OUTPUT_BYTES));
    let err_reader = std::thread::spawn(move || read_capped(stderr, MAX_CAPTURED_OUTPUT_BYTES));
    let started = std::time::Instant::now();
    let mut timed_out = false;
    let status = loop {
        // Inspect only windows created inside `child_job`, never the parent desktop app's own
        // main window or unrelated process-wide high-water marks (`APP-B06`).
        let visible = child_job.count_visible_windows();
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

/// Outcome of starting a process that must outlive its parent (`S-30`, `S-31`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DetachedLaunch {
    pub process_id: u32,
    /// False when an enclosing job forbids breakaway; the process then ends with that job.
    pub broke_away_from_job: bool,
}

/// Start a detached process (installer or relaunched app) that survives the caller's
/// kill-on-close job. Console windows are suppressed; GUI apps decide their own visibility.
///
/// With `allow_in_job` false, a failed breakaway is an error instead of a process that would
/// be terminated together with the caller (an installer must never be cut off mid-install).
#[allow(clippy::disallowed_methods)]
pub fn spawn_detached_app<I, S>(
    program: &OsStr,
    args: I,
    extra_env: &BTreeMap<String, String>,
    allow_in_job: bool,
) -> Result<DetachedLaunch, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<std::ffi::OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
    stop_inheriting_own_stdio();
    let build = |breakaway: bool| {
        let mut cmd = Command::new(program);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        #[cfg(windows)]
        {
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
            let mut flags = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
            if breakaway {
                flags |= CREATE_BREAKAWAY_FROM_JOB;
            }
            cmd.creation_flags(flags);
        }
        #[cfg(not(windows))]
        let _ = breakaway;
        cmd
    };
    match build(true).spawn() {
        Ok(child) => Ok(DetachedLaunch {
            process_id: child.id(),
            broke_away_from_job: cfg!(windows),
        }),
        Err(error) if error.raw_os_error() == Some(5) && !allow_in_job => Err(format!(
            "cannot start {:?} outside the caller's job: {error}",
            program
        )),
        Err(error) if error.raw_os_error() == Some(5) => build(false)
            .spawn()
            .map(|child| DetachedLaunch {
                process_id: child.id(),
                broke_away_from_job: false,
            })
            .map_err(|e| format!("failed to start {:?}: {e}", program)),
        Err(error) => Err(format!("failed to start {:?}: {error}", program)),
    }
}

/// Whether the process `process_id` (or this process for `None`) belongs to any job object.
pub fn process_in_job(process_id: Option<u32>) -> bool {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::IsProcessInJob;
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let (process, owned) = match process_id {
            None => (GetCurrentProcess(), false),
            Some(pid) => (OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid), true),
        };
        if process.is_null() {
            return false;
        }
        let mut in_job = 0;
        let ok = IsProcessInJob(process, std::ptr::null_mut(), &mut in_job) != 0;
        if owned {
            CloseHandle(process);
        }
        ok && in_job != 0
    }
    #[cfg(not(windows))]
    {
        let _ = process_id;
        false
    }
}

/// Whether `process_id` belongs to this process's own [`QuietJobObject`] (`ACTIVE_JOB`).
///
/// Use this instead of `process_in_job(Some(pid))` after launching via the desktop shell:
/// `delta-desktop` creates and assigns itself to its own `QuietJobObject` at startup, and
/// Windows 10/11 PCA/Shell routinely places GUI apps started via `ShellExecute` into a system
/// job, so checking against `NULL` ("any job") falsely reports `true` even though the app is
/// `explorer.exe`'s child and completely outside the caller's job.
pub fn process_in_active_job(process_id: u32) -> bool {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::IsProcessInJob;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let job = ACTIVE_JOB.load(Ordering::SeqCst);
        if job.is_null() {
            return false;
        }
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id);
        if process.is_null() {
            return false;
        }
        let mut in_job = 0;
        let ok = IsProcessInJob(process, job, &mut in_job) != 0;
        CloseHandle(process);
        ok && in_job != 0
    }
    #[cfg(not(windows))]
    {
        let _ = process_id;
        false
    }
}

/// Asks the desktop shell (`explorer.exe`, `Shell.Application` of the desktop window) to start
/// `program` with the `ShowWindow` command `show`, then waits up to 15 s for the new process of
/// that executable whose parent is not the same executable and returns its id. The program is
/// explorer's child, so it is in no job of the caller even when that job forbids breaking away
/// and ends its processes on close (an agent's command sandbox). It gets the shell's
/// environment, not the caller's. A missing `program` is refused before the shell is asked,
/// because the shell would show a modal error.
pub fn spawn_via_desktop_shell<I, S>(program: &Path, args: I, show: i32) -> Result<u32, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    if !cfg!(windows) {
        return Err("starting through the desktop shell is Windows only".into());
    }
    if !program.is_file() {
        return Err(format!("{} does not exist", program.display()));
    }
    let directory = program.parent().unwrap_or(Path::new("."));
    let params: Vec<String> = args
        .into_iter()
        .map(|a| quote_windows_arg(&a.as_ref().to_string_lossy()))
        .collect();
    let mut env = BTreeMap::new();
    env.insert(
        "DELTA_LAUNCH_EXE".to_string(),
        program.display().to_string(),
    );
    env.insert("DELTA_LAUNCH_ARGS".to_string(), params.join(" "));
    env.insert(
        "DELTA_LAUNCH_DIR".to_string(),
        directory.display().to_string(),
    );
    env.insert("DELTA_LAUNCH_SHOW".to_string(), show.to_string());
    let result = run_quiet_command_with_timeout(
        OsStr::new("powershell.exe"),
        [
            "-NoProfile",
            "-NonInteractive",
            "-Sta",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            DESKTOP_SHELL_LAUNCH_SCRIPT,
        ],
        Some(directory),
        &env,
        std::time::Duration::from_secs(45),
    )?;
    if result.timed_out || result.status_code != Some(0) {
        return Err(format!(
            "desktop shell launch failed (exit {:?}, timed out {}): {}",
            result.status_code,
            result.timed_out,
            result.stderr.trim()
        ));
    }
    result
        .stdout
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid != 0)
        .ok_or_else(|| {
            format!(
                "desktop shell launch printed no process id: {:?}",
                result.stdout.trim()
            )
        })
}

const DESKTOP_SHELL_LAUNCH_SCRIPT: &str = r#"$ErrorActionPreference = 'Stop'
$exe = $env:DELTA_LAUNCH_EXE; $argLine = $env:DELTA_LAUNCH_ARGS; $dir = $env:DELTA_LAUNCH_DIR; $show = [int]$env:DELTA_LAUNCH_SHOW
if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) { exit 2 }
$name = [IO.Path]::GetFileName($exe).Replace("'", "''")
function Get-Same { @(Get-CimInstance Win32_Process -Filter "Name='$name'" | Where-Object { $_.ExecutablePath -and [string]::Equals($_.ExecutablePath, $exe, [StringComparison]::OrdinalIgnoreCase) }) }
$before = @{}; foreach ($p in Get-Same) { $before[[int]$p.ProcessId] = $true }
$exp = @{}; foreach ($p in @(Get-CimInstance Win32_Process -Filter "Name='explorer.exe'")) { $exp[[int]$p.ProcessId] = $true }
$f = [Reflection.BindingFlags]; $m = [Reflection.Missing]::Value
$t = [Type]::GetTypeFromCLSID([Guid]'9BA05972-F6A8-11CF-A442-00A0C90A8F39')
$sw = [Activator]::CreateInstance($t)
$d = $t.InvokeMember('FindWindowSW', $f::InvokeMethod, $null, $sw, @($m, $m, 8, 0, 1))
if ($null -eq $d) { exit 3 }
$doc = $d.GetType().InvokeMember('Document', $f::GetProperty, $null, $d, $null)
$app = $doc.GetType().InvokeMember('Application', $f::GetProperty, $null, $doc, $null)
[void]$app.GetType().InvokeMember('ShellExecute', $f::InvokeMethod, $null, $app, @($exe, $argLine, $dir, 'open', $show))
$deadline = (Get-Date).AddSeconds(15)
while ((Get-Date) -lt $deadline) {
  $now = Get-Same; $ids = @{}; foreach ($p in $now) { $ids[[int]$p.ProcessId] = $true }
  $cand = @($now | Where-Object { -not $before.ContainsKey([int]$_.ProcessId) -and -not $ids.ContainsKey([int]$_.ParentProcessId) })
  $root = @($cand | Where-Object { $exp.ContainsKey([int]$_.ParentProcessId) }) | Select-Object -First 1
  if (-not $root) { $root = $cand | Select-Object -First 1 }
  if ($root) { [Console]::Out.Write([int]$root.ProcessId); exit 0 }
  Start-Sleep -Milliseconds 200
}
exit 4"#;

/// Quotes one argument for a Windows command line (`CommandLineToArgvW` rules).
fn quote_windows_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.chars().any(|c| c.is_whitespace() || c == '"') {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut slashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(slashes * 2 + 1));
                out.push('"');
                slashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(slashes));
                out.push(c);
                slashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(slashes * 2));
    out.push('"');
    out
}

/// `CreateProcessW` is called with handle inheritance enabled, so a detached process would
/// also inherit this process's own inheritable stdio handles (for example the pipe a gate or
/// `delta-cli install-dev | tee` reads). The reader would then wait for EOF until the detached
/// app exits. Clear the inherit flag on our own standard handles first; `Stdio::inherit()`
/// keeps working because std duplicates those handles explicitly for each child.
fn stop_inheriting_own_stdio() {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{
            SetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
        };
        for handle in [
            std::io::stdin().as_raw_handle(),
            std::io::stdout().as_raw_handle(),
            std::io::stderr().as_raw_handle(),
        ] {
            if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                // Failure (no handle, or a console handle) leaves the previous behaviour.
                unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
            }
        }
    }
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

/// Whether a top-level window can be seen by the user: `WS_VISIBLE`, a non-empty window
/// rectangle and not cloaked by DWM. Some installer toolkits keep zero-size, disabled
/// `WS_VISIBLE` helper windows (`TWindowDisabler-Window`) that nobody can see or activate;
/// those are not popups in the sense of `D-25`.
#[cfg(windows)]
/// `handle` is the window handle value (`HWND as isize`).
pub fn window_is_perceivable(handle: isize) -> bool {
    let hwnd = handle as windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowRect, IsWindowVisible};
    unsafe {
        if IsWindowVisible(hwnd) == 0 {
            return false;
        }
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if GetWindowRect(hwnd, &mut rect) != 0
            && (rect.right <= rect.left || rect.bottom <= rect.top)
        {
            return false;
        }
        let mut cloaked = 0u32;
        let cloak_known = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED as u32,
            (&mut cloaked as *mut u32).cast(),
            std::mem::size_of::<u32>() as u32,
        ) == 0;
        !(cloak_known && cloaked != 0)
    }
}

#[cfg(windows)]
fn count_visible_windows_for_filter(
    target_pid: Option<u32>,
    target_job: Option<windows_sys::Win32::Foundation::HANDLE>,
) -> usize {
    unsafe {
        use windows_sys::Win32::Foundation::{HWND, LPARAM};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            EnumWindows, GetWindowTextW, GetWindowThreadProcessId,
        };

        struct CallbackCtx {
            target_pid: Option<u32>,
            target_job: windows_sys::Win32::Foundation::HANDLE,
            count: AtomicUsize,
        }

        unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> i32 {
            let ctx = &*(lparam as *const CallbackCtx);
            if window_is_perceivable(hwnd as isize) {
                let mut pid: u32 = 0;
                GetWindowThreadProcessId(hwnd, &mut pid);
                let mut belongs = ctx.target_pid.is_some_and(|target| pid == target);
                if !ctx.target_job.is_null() {
                    use windows_sys::Win32::System::JobObjects::IsProcessInJob;
                    use windows_sys::Win32::System::Threading::{
                        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
                    };
                    let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
                    if !process.is_null() {
                        let mut in_job = 0;
                        if IsProcessInJob(process, ctx.target_job, &mut in_job) != 0 {
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
                    // Never unwind across the Win32 `extern "system"` callback boundary (`XCR-U04`).
                    let mut details = WINDOW_DETAILS
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
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
            target_pid,
            target_job: target_job.unwrap_or(std::ptr::null_mut()),
            count: AtomicUsize::new(0),
        };
        let _ = EnumWindows(Some(enum_proc), &ctx as *const CallbackCtx as LPARAM);
        let observed = ctx.count.load(Ordering::Relaxed);
        OBSERVED_WINDOWS.fetch_max(observed, Ordering::SeqCst);
        observed
    }
}

/// Whether a process started from `exe` shows a top-level window the user can see
/// (`window_is_perceivable`: a minimized window counts, one hidden to the tray does not).
/// `install-dev` waits while the installed dev build has one open: the person is using it.
pub fn exe_has_visible_window(exe: &Path) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
            PROCESS_QUERY_LIMITED_INFORMATION,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId};

        struct Search {
            exe: String,
            found: bool,
        }

        unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> i32 {
            let search = &mut *(lparam as *mut Search);
            if !window_is_perceivable(hwnd as isize) {
                return 1;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut pid);
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return 1;
            }
            let mut buffer = [0u16; 1024];
            let mut length = buffer.len() as u32;
            let named = QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                buffer.as_mut_ptr(),
                &mut length,
            ) != 0;
            CloseHandle(process);
            if named
                && String::from_utf16_lossy(&buffer[..length as usize]).to_lowercase() == search.exe
            {
                search.found = true;
                return 0;
            }
            1
        }

        let mut search = Search {
            exe: exe.to_string_lossy().to_lowercase(),
            found: false,
        };
        unsafe {
            let _ = EnumWindows(Some(visit), &mut search as *mut Search as LPARAM);
        }
        search.found
    }
    #[cfg(not(windows))]
    {
        let _ = exe;
        false
    }
}

/// Window sentinel (`EnumWindows` + `window_is_perceivable` + `GetWindowThreadProcessId`)
/// that counts visible top-level windows belonging to the current process (`D-25` / S-10).
pub fn count_visible_windows_for_current_process() -> usize {
    #[cfg(windows)]
    {
        let job = ACTIVE_JOB.load(Ordering::SeqCst);
        let pid = unsafe { windows_sys::Win32::System::Threading::GetCurrentProcessId() };
        let _ = count_visible_windows_for_filter(Some(pid), (!job.is_null()).then_some(job));
        OBSERVED_WINDOWS.load(Ordering::SeqCst)
    }
    #[cfg(not(windows))]
    {
        0
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn desktop_shell_arguments_follow_command_line_quoting() {
        assert_eq!(quote_windows_arg("--background"), "--background");
        assert_eq!(quote_windows_arg(""), "\"\"");
        assert_eq!(
            quote_windows_arg(r"Programs\DELTA Dev\delta.exe"),
            r#""Programs\DELTA Dev\delta.exe""#
        );
        assert_eq!(quote_windows_arg(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote_windows_arg(r"dir\ x\\"), r#""dir\ x\\\\""#);
    }

    #[test]
    fn desktop_shell_launch_refuses_a_missing_program_without_asking_the_shell() {
        let missing = std::env::temp_dir().join("delta-missing-program-for-shell-launch.exe");
        let error = spawn_via_desktop_shell(&missing, ["--background"], 7).unwrap_err();
        assert!(error.contains("does not exist"), "{error}");
    }

    #[test]
    fn process_in_active_job_returns_false_for_unrelated_process() {
        // Without an active job (or for a process outside `ACTIVE_JOB`), the check must not
        // falsely claim the target belongs to our job.
        assert!(!process_in_active_job(0));
        let pid = std::process::id();
        if ACTIVE_JOB.load(Ordering::SeqCst).is_null() {
            assert!(!process_in_active_job(pid));
        }
    }

    /// Opt-in (`DELTA_RUN_SHELL_LAUNCH_TEST=1`, it starts a real process through explorer):
    /// a hidden `ping` started through the desktop shell is in no job. Run it inside a job
    /// that forbids breaking away and ends its processes on close to check the escape.
    #[test]
    fn desktop_shell_launch_starts_the_program_outside_every_job() {
        if std::env::var("DELTA_RUN_SHELL_LAUNCH_TEST").as_deref() != Ok("1") {
            return;
        }
        let ping = std::env::var_os("SystemRoot")
            .map(|root| Path::new(&root).join("System32").join("PING.EXE"))
            .expect("SystemRoot");
        let pid = spawn_via_desktop_shell(&ping, ["-n", "4", "127.0.0.1"], 0).unwrap();
        assert!(!process_in_job(Some(pid)), "process {pid} is in a job");
        assert!(
            !process_in_active_job(pid),
            "process {pid} is in ACTIVE_JOB"
        );
    }

    /// Child half of the test below: launch a detached sleeper and return immediately.
    fn launch_detached_sleeper() {
        let ping = std::env::var_os("SystemRoot")
            .map(|root| Path::new(&root).join("System32").join("PING.EXE"))
            .unwrap();
        let launch = spawn_detached_app(
            ping.as_os_str(),
            ["-n", "16", "127.0.0.1"],
            &BTreeMap::new(),
            true,
        )
        .unwrap();
        println!("detached {}", launch.process_id);
    }

    #[test]
    fn m3_a06_detached_launch_does_not_hold_the_callers_stdout_open() {
        if std::env::var_os("DELTA_TEST_DETACHED_CHILD").is_some() {
            return launch_detached_sleeper();
        }
        let exe = std::env::current_exe().unwrap();
        let env = BTreeMap::from([("DELTA_TEST_DETACHED_CHILD".to_string(), "1".to_string())]);
        let started = Instant::now();
        let result = run_quiet_command_with_timeout(
            exe.as_os_str(),
            [
                "--exact",
                "process::tests::m3_a06_detached_launch_does_not_hold_the_callers_stdout_open",
                "--nocapture",
                "--test-threads=1",
            ],
            None,
            &env,
            Duration::from_secs(60),
        )
        .unwrap();
        let elapsed = started.elapsed();
        assert_eq!(result.status_code, Some(0), "{}", result.stderr);
        assert!(result.stdout.contains("detached "), "{}", result.stdout);
        // The sleeper runs ~15 s; reading the child's output must not wait for it.
        assert!(
            elapsed < Duration::from_secs(8),
            "caller waited {elapsed:?} for a detached process to release its stdout"
        );
    }
}
