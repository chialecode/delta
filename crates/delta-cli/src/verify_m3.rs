//! `verify-stage --stage M3 --plan-version 1.0 [--installer] [--ui-smoke]` (`S-35`).
//!
//! Offline contract checks always run. `--installer` builds and really installs the gate-only
//! identities `delta-test` / `delta-dev-test`; the developer's `delta` / `delta-dev` installs,
//! uninstall keys, shortcuts and data roots are snapshotted and must stay identical.

use super::{
    verify::{case_from_tests, failure, owned_test_directories, record_tests, source_fingerprint},
    CaseResultRecord, PocResultRecord, VerifyStageReport,
};
use crate::installer::{
    self, file_version_info, install_dir, installed_value, installed_version, run_setup_silently,
    run_uninstall_silently, verify_identity_binary, InstallerArtifact, PackagingContext,
};
use chrono::Utc;
use delta_app::{
    count_visible_windows_for_current_process, current_process_priority, observed_window_details,
    run_quiet_command, run_quiet_command_with_timeout, sanitize_terminal_output, spawn_quiet_child,
    update::{self, UpdateLoopbackServer},
    AppHost, AppService, BuildChannel, ControlCommand, ErrorEnvelope, InstallIdentity,
    QuietJobObject, ResolvedAppPaths,
};
use delta_data::sha256_file;
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsStr,
    fs,
    io::BufRead,
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Path, PathBuf},
    process::{Child, ExitCode},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

const TEST_IDENTITIES: [InstallIdentity; 2] = [InstallIdentity::Test, InstallIdentity::DevTest];
const PENDING_NOTE_BODY: &str = "Typed just before the upgrade; autosave had not run yet.";
const UNKNOWN_DATA_FILE: &str = "m3-unknown-user-file.txt";

fn ensure(condition: bool, message: impl std::fmt::Display) -> Result<(), ErrorEnvelope> {
    if condition {
        Ok(())
    } else {
        Err(failure("M3_CHECK", message))
    }
}

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    condition()
}

fn read_lossy(path: &Path) -> String {
    fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Developer-state snapshot (never touched by the gate)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
struct DeveloperState {
    /// Every file of `Programs\delta` and `Programs\delta-dev` with its SHA-256.
    installs: BTreeMap<String, String>,
    /// Directory tree of the developer's data and document roots (files may legitimately
    /// change while the developer's own app runs; directories created by the gate may not).
    data_directories: BTreeSet<String>,
    release_data_modified: Option<String>,
    uninstall_keys: BTreeMap<String, Option<String>>,
    shortcuts: BTreeMap<String, Option<String>>,
}

fn start_menu_programs() -> Option<PathBuf> {
    env::var_os("APPDATA")
        .map(|base| PathBuf::from(base).join("Microsoft/Windows/Start Menu/Programs"))
}

fn shortcut(identity: InstallIdentity) -> Option<PathBuf> {
    start_menu_programs().map(|dir| dir.join(format!("{}.lnk", identity.display_name())))
}

fn walk_files(root: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => walk_files(&path, out),
                Ok(_) => out.push(path),
                Err(_) => {}
            }
        }
    }
}

fn walk_directories(root: &Path, out: &mut BTreeSet<String>) {
    if !root.is_dir() {
        return;
    }
    out.insert(root.display().to_string());
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                walk_directories(&entry.path(), out);
            }
        }
    }
}

fn developer_state() -> DeveloperState {
    let mut state = DeveloperState::default();
    for identity in [InstallIdentity::Release, InstallIdentity::Dev] {
        if let Some(directory) = install_dir(identity) {
            let mut files = Vec::new();
            walk_files(&directory, &mut files);
            for file in files {
                state.installs.insert(
                    file.display().to_string(),
                    sha256_file(&file).unwrap_or_else(|e| format!("unreadable: {e}")),
                );
            }
        }
        state.uninstall_keys.insert(
            identity.name().into(),
            installed_value(identity, "DisplayVersion"),
        );
        if let Some(link) = shortcut(identity) {
            state.shortcuts.insert(
                link.display().to_string(),
                link.is_file()
                    .then(|| sha256_file(&link).unwrap_or_default()),
            );
        }
    }
    let local = env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let documents = delta_app::resolve_user_documents_root().ok();
    for base in local.iter().chain(documents.iter()) {
        for name in ["delta", "delta-dev"] {
            walk_directories(&base.join(name), &mut state.data_directories);
        }
    }
    state.release_data_modified = local
        .as_ref()
        .and_then(|p| p.join("delta").metadata().ok())
        .and_then(|m| m.modified().ok())
        .map(|t| format!("{t:?}"));
    state
}

// ---------------------------------------------------------------------------
// Quietness monitor (`D-25`, `M3-K04`): processes outside the gate's job (installers,
// uninstallers, relaunched apps) are matched by image name.
// ---------------------------------------------------------------------------

struct QuietnessMonitor {
    stop: Arc<AtomicBool>,
    findings: Arc<Mutex<BTreeSet<String>>>,
    samples: Arc<Mutex<u64>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

fn gate_owned_image(image: &str) -> bool {
    let name = image
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(image)
        .to_ascii_lowercase();
    name.starts_with("delta-test")
        || name.starts_with("delta-dev-test")
        || name.starts_with("delta-migrate-helper")
        || name.starts_with("unins")
        || name.starts_with("_unins")
        || name.starts_with("_iu")
        || name.starts_with("iscc")
        || name.contains("-setup-")
}

#[cfg(windows)]
fn process_image(pid: u32) -> Option<String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return None;
        }
        let mut buffer = [0u16; 1024];
        let mut size = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut size);
        CloseHandle(process);
        (ok != 0).then(|| String::from_utf16_lossy(&buffer[..size as usize]))
    }
}

#[cfg(windows)]
fn sample_windows(findings: &Mutex<BTreeSet<String>>) {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId,
    };
    fn describe(hwnd: HWND) -> Option<String> {
        unsafe {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, &mut pid);
            let image = process_image(pid)?;
            if !gate_owned_image(&image) {
                return None;
            }
            let mut title = [0u16; 256];
            let length = GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32).max(0);
            Some(format!(
                "{} \"{}\"",
                image.rsplit('\\').next().unwrap_or(&image),
                String::from_utf16_lossy(&title[..length as usize])
            ))
        }
    }
    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> i32 {
        let findings = &*(lparam as *const Mutex<BTreeSet<String>>);
        if delta_app::window_is_perceivable(hwnd as isize) {
            if let Some(text) = describe(hwnd) {
                findings
                    .lock()
                    .unwrap()
                    .insert(format!("visible window: {text}"));
            }
        }
        1
    }
    unsafe {
        let foreground = GetForegroundWindow();
        if !foreground.is_null() {
            if let Some(text) = describe(foreground) {
                findings
                    .lock()
                    .unwrap()
                    .insert(format!("foreground: {text}"));
            }
        }
        EnumWindows(Some(visit), findings as *const _ as LPARAM);
    }
}

#[cfg(not(windows))]
fn sample_windows(_findings: &Mutex<BTreeSet<String>>) {}

impl QuietnessMonitor {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let findings = Arc::new(Mutex::new(BTreeSet::new()));
        let samples = Arc::new(Mutex::new(0));
        let thread = {
            let (stop, findings, samples) = (
                Arc::clone(&stop),
                Arc::clone(&findings),
                Arc::clone(&samples),
            );
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    sample_windows(&findings);
                    *samples.lock().unwrap() += 1;
                    std::thread::sleep(Duration::from_millis(200));
                }
            })
        };
        Self {
            stop,
            findings,
            samples,
            thread: Some(thread),
        }
    }

    fn finish(mut self) -> (u64, Vec<String>) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let samples = *self.samples.lock().unwrap();
        let findings = self.findings.lock().unwrap().iter().cloned().collect();
        (samples, findings)
    }
}

// ---------------------------------------------------------------------------
// Hidden resident app (`--verification-resident`, test identities only)
// ---------------------------------------------------------------------------

struct Resident {
    child: Child,
    events: Arc<Mutex<Vec<Value>>>,
    stderr: Arc<Mutex<String>>,
    identity: InstallIdentity,
}

impl Resident {
    fn spawn(
        identity: InstallIdentity,
        data_dir: &Path,
        extra: &[&str],
        environment: &BTreeMap<String, String>,
    ) -> Result<Self, ErrorEnvelope> {
        let exe = install_dir(identity)
            .ok_or_else(|| failure("RESIDENT", "LOCALAPPDATA is not set"))?
            .join(identity.desktop_exe());
        let mut args = vec![
            "--verification-resident".to_string(),
            "--data-dir".into(),
            data_dir.display().to_string(),
        ];
        args.extend(extra.iter().map(|a| a.to_string()));
        let mut child = spawn_quiet_child(exe.as_os_str(), &args, environment)
            .map_err(|e| failure("RESIDENT", e))?;
        let events = Arc::new(Mutex::new(Vec::new()));
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(stdout) = child.stdout.take() {
            let events = Arc::clone(&events);
            std::thread::spawn(move || {
                for line in std::io::BufReader::new(stdout)
                    .lines()
                    .map_while(Result::ok)
                {
                    if let Ok(value) = serde_json::from_str::<Value>(&line) {
                        events.lock().unwrap().push(value);
                    }
                }
            });
        }
        if let Some(pipe) = child.stderr.take() {
            let stderr = Arc::clone(&stderr);
            std::thread::spawn(move || {
                for line in std::io::BufReader::new(pipe).lines().map_while(Result::ok) {
                    let mut text = stderr.lock().unwrap();
                    if text.len() < 16 * 1024 {
                        text.push_str(&line);
                        text.push('\n');
                    }
                }
            });
        }
        Ok(Self {
            child,
            events,
            stderr,
            identity,
        })
    }

    fn event(&self, name: &str) -> Option<Value> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .find(|e| e["event"] == name)
            .cloned()
    }

    fn names(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| e["event"].as_str().map(str::to_string))
            .collect()
    }

    fn wait_event(&mut self, name: &str, timeout: Duration) -> Result<Value, ErrorEnvelope> {
        let started = Instant::now();
        loop {
            if let Some(event) = self.event(name) {
                return Ok(event);
            }
            let exited = self.child.try_wait().ok().flatten();
            if exited.is_some() || started.elapsed() >= timeout {
                std::thread::sleep(Duration::from_millis(300));
                if let Some(event) = self.event(name) {
                    return Ok(event);
                }
                return Err(failure(
                    "RESIDENT_EVENT",
                    format!(
                        "{} did not emit {name} (exit {:?}); events {:?}; stderr {}",
                        self.identity.name(),
                        exited.map(|s| s.code()),
                        self.names(),
                        sanitize_terminal_output(&self.stderr.lock().unwrap())
                    ),
                ));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Wait for the app's own exit; the gate never terminates it.
    fn wait_exit(&mut self, timeout: Duration) -> Result<Option<i32>, ErrorEnvelope> {
        let started = Instant::now();
        while started.elapsed() < timeout {
            if let Some(status) = self.child.try_wait().map_err(|e| failure("RESIDENT", e))? {
                std::thread::sleep(Duration::from_millis(200));
                return Ok(status.code());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Err(failure(
            "RESIDENT_EXIT",
            format!(
                "{} still running after {timeout:?}; events {:?}",
                self.identity.name(),
                self.names()
            ),
        ))
    }
}

impl Drop for Resident {
    fn drop(&mut self) {
        // A failed case may leave the app running: ask it to save and exit like a user would.
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = installer::control(
                self.identity,
                ControlCommand::QuitForUpdate,
                Duration::from_secs(10),
            );
            let _ = self.wait_exit(Duration::from_secs(20));
        }
    }
}

/// Events appended by verification residents (including relaunched ones) in `data_dir`.
fn resident_events(data_dir: &Path) -> Vec<Value> {
    read_lossy(&data_dir.join("verification-events.jsonl"))
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn quit_running(identity: InstallIdentity, timeout: Duration) -> Result<(), ErrorEnvelope> {
    let started = Instant::now();
    loop {
        let (code, detail) = installer::control(
            identity,
            ControlCommand::QuitForUpdate,
            Duration::from_secs(15),
        );
        if code == 0 {
            return ensure(
                delta_app::wait_for_exit(&identity.mutex_name(), Duration::from_secs(30)),
                format!("{} did not exit after quit-for-update", identity.name()),
            );
        }
        if started.elapsed() >= timeout {
            return Err(failure(
                "QUIT",
                format!("{} refused to quit: {detail}", identity.name()),
            ));
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

// ---------------------------------------------------------------------------
// Library inspection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct LibraryState {
    accounts: usize,
    notes: BTreeMap<String, (String, String)>,
    attachments_verified: usize,
    attachments_broken: usize,
    settings: BTreeMap<String, String>,
}

fn gate_paths(data_dir: &Path) -> ResolvedAppPaths {
    let documents = data_dir.join("gate-documents");
    ResolvedAppPaths {
        channel: BuildChannel::current(),
        data_dir: data_dir.to_path_buf(),
        documents_dir: documents.clone(),
        default_export_dir: documents.join("exports"),
        default_backup_dir: documents.join("backups"),
        credential_prefix: format!("delta-test-gate-{}", uuid::Uuid::new_v4()),
    }
}

fn inspect_library(data_dir: &Path) -> Result<LibraryState, ErrorEnvelope> {
    let service = AppService::open(gate_paths(data_dir), "USD", "USD", "UTC", true)?;
    let host = AppHost::from_service(service);
    let accounts = host.list_accounts()?.len();
    let mut notes = BTreeMap::new();
    let (mut verified, mut broken) = (0, 0);
    for note in host.list_notes(false)? {
        for attachment in &note.attachments {
            let ok = attachment.exists_on_disk
                && sha256_file(&data_dir.join(&attachment.relative_path))
                    .is_ok_and(|hash| hash == attachment.sha256_hash);
            if ok {
                verified += 1;
            } else {
                broken += 1;
            }
        }
        notes.insert(note.id.clone(), (note.title, note.body_markdown));
    }
    drop(host);
    let mut settings = BTreeMap::new();
    let mut files = Vec::new();
    walk_files(&data_dir.join("settings"), &mut files);
    for file in files {
        let name = file
            .strip_prefix(data_dir)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        settings.insert(name, sha256_file(&file).unwrap_or_default());
    }
    Ok(LibraryState {
        accounts,
        notes,
        attachments_verified: verified,
        attachments_broken: broken,
        settings,
    })
}

/// Library facts that must survive an upgrade: accounts, every earlier note, attachments,
/// and settings other than the window layout (re-saved on exit) and update bookkeeping.
fn ensure_library_kept(before: &LibraryState, after: &LibraryState) -> Result<(), ErrorEnvelope> {
    ensure(
        after.accounts == before.accounts && after.accounts > 0,
        format!("accounts {} -> {}", before.accounts, after.accounts),
    )?;
    for (id, note) in &before.notes {
        ensure(
            after.notes.get(id) == Some(note),
            format!("note {id} changed or disappeared"),
        )?;
    }
    ensure(
        after.attachments_broken == 0 && after.attachments_verified >= before.attachments_verified,
        format!(
            "attachments verified {} -> {}, broken {}",
            before.attachments_verified, after.attachments_verified, after.attachments_broken
        ),
    )?;
    for (name, hash) in &before.settings {
        if name.ends_with("layout.json") || name.ends_with("update.json") {
            ensure(
                after.settings.contains_key(name),
                format!("{name} disappeared"),
            )?;
        } else {
            ensure(
                after.settings.get(name) == Some(hash),
                format!("setting {name} changed"),
            )?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Gate
// ---------------------------------------------------------------------------

struct Gate {
    root: PathBuf,
    temp: PathBuf,
    logs: PathBuf,
    report: VerifyStageReport,
    ui_smoke: bool,
}

impl Gate {
    fn record(
        &mut self,
        id: &str,
        package: &str,
        started: Instant,
        outcome: Result<String, ErrorEnvelope>,
    ) -> bool {
        let passed = outcome.is_ok();
        let evidence = match outcome {
            Ok(evidence) => evidence,
            Err(error) => {
                self.report
                    .errors
                    .push(format!("{id}: {}: {}", error.code, error.message));
                format!("FAILED {}: {}", error.code, error.message)
            }
        };
        self.report.case_results.push(CaseResultRecord {
            case_id: id.into(),
            work_package: package.into(),
            tier: "required".into(),
            status: if passed { "passed" } else { "failed" }.into(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            evidence: sanitize_terminal_output(&evidence),
        });
        passed
    }

    fn not_run(&mut self, id: &str, package: &str, tier: &str, reason: &str) {
        self.report.case_results.push(CaseResultRecord {
            case_id: id.into(),
            work_package: package.into(),
            tier: tier.into(),
            status: "not-run".into(),
            elapsed_ms: 0,
            evidence: reason.into(),
        });
    }

    fn log_json(&mut self, name: &str, value: &impl Serialize) -> String {
        let path = self.logs.join(format!("{name}.json"));
        let _ = fs::write(&path, serde_json::to_vec_pretty(value).unwrap_or_default());
        let relative = path
            .strip_prefix(&self.root)
            .unwrap_or(&path)
            .display()
            .to_string()
            .replace('\\', "/");
        self.report.log_files.insert(name.into(), relative.clone());
        relative
    }

    fn log_path(&mut self, name: &str) -> PathBuf {
        let path = self.logs.join(name);
        let relative = path
            .strip_prefix(&self.root)
            .unwrap_or(&path)
            .display()
            .to_string()
            .replace('\\', "/");
        self.report.log_files.insert(name.into(), relative);
        path
    }

    fn data_dir(&self, identity: InstallIdentity) -> PathBuf {
        self.temp.join(identity.name()).join("data")
    }

    /// Environment for installed test binaries: data and documents in the gate's temp root.
    fn identity_env(&self, identity: InstallIdentity) -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                "DELTA_DATA_DIR".into(),
                self.data_dir(identity).display().to_string(),
            ),
            (
                "DELTA_DOCUMENTS_DIR".into(),
                self.temp
                    .join(identity.name())
                    .join("documents")
                    .display()
                    .to_string(),
            ),
        ])
    }
}

pub(crate) fn run_verify_stage_m3(
    installer_mode: bool,
    ui_smoke: bool,
) -> Result<ExitCode, ErrorEnvelope> {
    let root = installer::plain_canonical(&env::current_dir().map_err(|e| failure("cwd", e))?)
        .map_err(|e| failure("cwd", e))?;
    let fingerprint = source_fingerprint(&root)?;
    let run_id = format!("{}-{}", std::process::id(), Utc::now().timestamp_millis());
    let temp = env::temp_dir().join(format!("delta_m3_verify_{run_id}"));
    fs::create_dir(&temp).map_err(|e| failure("temp", e))?;
    let reports = root.join(".local/reports/m3");
    let logs = reports.join(format!("logs-{run_id}"));
    fs::create_dir_all(&logs).map_err(|e| failure("logs", e))?;
    let test_directories_before =
        owned_test_directories().map_err(|e| failure("temp_snapshot", e))?;
    let developer_before = developer_state();
    let jobs = env::var("DELTA_GATE_JOBS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get().saturating_sub(2).max(1))
                .unwrap_or(1)
        });
    let mut gate = Gate {
        root: root.clone(),
        temp: temp.clone(),
        logs,
        ui_smoke,
        report: VerifyStageReport {
            stage: "M3".into(),
            plan_version: "1.0".into(),
            executed_at: Utc::now().to_rfc3339(),
            live_mode: false,
            source_fingerprint: fingerprint,
            visible_window_count: 0,
            window_details: Vec::new(),
            temp_residual_dirs: 0,
            outside_temp_created_dirs: 0,
            path_observation_scope: "SHA-256 of every file in LOCALAPPDATA/Programs/delta and delta-dev, their HKCU uninstall keys and Start-menu shortcuts, and the directory trees of LOCALAPPDATA and Documents delta/delta-dev; test identities install into Programs/delta-test* and keep data in TEMP".into(),
            release_data_dir_touched: false,
            poc_results: Vec::new(),
            case_results: Vec::new(),
            live_probes: Vec::new(),
            overall_status: "running".into(),
            gate_jobs: jobs,
            process_priority: current_process_priority(),
            test_results: BTreeMap::new(),
            log_files: BTreeMap::new(),
            errors: Vec::new(),
        },
    };
    let monitor = QuietnessMonitor::start();

    let execution = catch_unwind(AssertUnwindSafe(|| -> Result<(), ErrorEnvelope> {
        let job = QuietJobObject::new().map_err(|e| failure("job_object", e))?;
        job.assign_current_process()
            .map_err(|e| failure("job_object", e))?;
        if env::var("DELTA_GATE_SKIP_MECHANICAL").is_ok_and(|v| v == "1") {
            // Iteration aid only: the skipped required cases make the overall result not-run.
            for (id, package) in [
                ("M3-G-01", "M3-W06"),
                ("M3-G-02", "M3-W06"),
                ("M3-G-03", "M3-W06"),
                ("M3-G-04", "M3-W06"),
                ("M3-G-05", "M3-W06"),
                ("M3-A-01", "M3-W01"),
                ("M3-A-02", "M3-W01"),
                ("M3-A-07", "M3-W04"),
            ] {
                gate.not_run(
                    id,
                    package,
                    "required",
                    "skipped by DELTA_GATE_SKIP_MECHANICAL=1",
                );
            }
        } else {
            mechanical_gates(&mut gate, jobs)?;
        }
        offline_contracts(&mut gate);
        if installer_mode {
            installer_cases(&mut gate);
        } else {
            for (id, package) in [
                ("M3-A-03", "M3-W02"),
                ("M3-A-04", "M3-W02,M3-W03"),
                ("M3-A-05", "M3-W02"),
                ("M3-A-06", "M3-W03"),
                ("M3-A-08", "M3-W04,M3-W05"),
                ("M3-A-09", "M3-W05"),
            ] {
                gate.not_run(id, package, "required", "requires --installer");
            }
        }
        Ok(())
    }));
    match execution {
        Ok(Ok(())) => {}
        Ok(Err(error)) => gate
            .report
            .errors
            .push(format!("{}: {}", error.code, error.message)),
        Err(payload) => gate.report.errors.push(format!(
            "gate panic: {}",
            payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("non-string panic")
        )),
    }

    // M3-A-10: quietness, isolation, cleanup and regressions.
    let started = Instant::now();
    let (samples, findings) = monitor.finish();
    let cleanup = cleanup_test_roots(&gate);
    if let Err(error) = fs::remove_dir_all(&temp) {
        gate.report
            .errors
            .push(format!("temporary cleanup failed: {error}"));
    }
    gate.report.temp_residual_dirs = usize::from(temp.exists())
        + owned_test_directories()
            .map_err(|e| failure("temp_snapshot", e))?
            .difference(&test_directories_before)
            .count();
    let developer_after = developer_state();
    gate.log_json(
        "developer-state",
        &json!({"before": developer_before, "after": developer_after}),
    );
    gate.report.release_data_dir_touched =
        developer_before.release_data_modified != developer_after.release_data_modified;
    gate.report.outside_temp_created_dirs = developer_after
        .data_directories
        .difference(&developer_before.data_directories)
        .count();
    gate.report.visible_window_count = count_visible_windows_for_current_process();
    gate.report.window_details = observed_window_details();
    let regressions = regression_reports(&gate);
    let a10 = (|| -> Result<String, ErrorEnvelope> {
        ensure(
            !cfg!(windows) || gate.report.process_priority == "below_normal",
            "process priority is not below normal",
        )?;
        ensure(
            gate.report.visible_window_count == 0 && findings.is_empty(),
            format!("visible or foreground windows: {findings:?}"),
        )?;
        ensure(
            developer_after == developer_before && !gate.report.release_data_dir_touched,
            "developer delta/delta-dev installs, keys, shortcuts or data roots changed",
        )?;
        ensure(
            gate.report.temp_residual_dirs == 0,
            "test temporary directories remain",
        )?;
        let cleanup = cleanup.clone()?;
        let regressions = regressions.clone()?;
        ensure(installer_mode, "requires --installer")?;
        Ok(format!(
            "below_normal; {samples} window samples, 0 visible/foreground test windows; developer installs ({} files), uninstall keys, shortcuts and {} data directories identical; {cleanup}; {regressions}",
            developer_before.installs.len(),
            developer_before.data_directories.len()
        ))
    })();
    if installer_mode || a10.is_err() {
        gate.record("M3-A-10", "M3-W06", started, a10.clone());
    } else {
        gate.not_run("M3-A-10", "M3-W06", "required", "requires --installer");
    }
    let k04 = a10.is_ok();
    poc(
        &mut gate,
        "M3-K04",
        "Silent install does not steal focus",
        installer_mode,
        k04,
    );

    for (id, package, description) in [
        (
            "M3-M-01",
            "M3-W02,M3-W06",
            "Manual check USER-ACTIONS H-03 (clean machine install of both channels, SmartScreen, coexistence, uninstall keeps data)",
        ),
        (
            "M3-M-02",
            "M3-W04",
            "Manual check USER-ACTIONS H-04 (release update wording, timing, restart, automatic-check switch)",
        ),
    ] {
        gate.not_run(id, package, "manual", description);
    }
    gate.not_run(
        "M3-L-01",
        "M3-W05",
        "live",
        "First public release v0.1.1 needs ACT-10 (signing key, embedded public key) and ACT-11 (authorized gh release create); loopback rehearsal M3-A-08 is not a real GitHub release",
    );

    if source_fingerprint(&root)? != gate.report.source_fingerprint {
        gate.report
            .errors
            .push("source changed during verification".into());
    }
    let failed = !gate.report.errors.is_empty()
        || gate
            .report
            .case_results
            .iter()
            .any(|c| c.status == "failed")
        || gate.report.poc_results.iter().any(|p| p.status == "failed");
    let required_not_run = gate
        .report
        .case_results
        .iter()
        .any(|c| c.tier == "required" && c.status == "not-run")
        || gate
            .report
            .poc_results
            .iter()
            .any(|p| p.status == "not-run");
    gate.report.overall_status = if failed {
        "failed"
    } else if required_not_run {
        "not-run"
    } else {
        "passed"
    }
    .into();
    let json = serde_json::to_string_pretty(&gate.report).map_err(|e| failure("report", e))?;
    fs::write(reports.join("verify-stage-report.json"), &json).map_err(|e| failure("report", e))?;
    println!("{}", sanitize_terminal_output(&json));
    Ok(if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn poc(gate: &mut Gate, id: &str, title: &str, ran: bool, passed: bool) {
    gate.report.poc_results.push(PocResultRecord {
        id: id.into(),
        title: title.into(),
        status: if !ran {
            "not-run"
        } else if passed {
            "passed"
        } else {
            "failed"
        }
        .into(),
        metrics: BTreeMap::new(),
        detail: if ran {
            "see the linked M3-A cases".into()
        } else {
            "requires --installer".into()
        },
    });
}

fn mechanical_gates(gate: &mut Gate, jobs: usize) -> Result<(), ErrorEnvelope> {
    let gate_env = BTreeMap::from([
        (
            "DELTA_DATA_DIR".into(),
            gate.temp.join("gate-data").display().to_string(),
        ),
        (
            "DELTA_DOCUMENTS_DIR".into(),
            gate.temp.join("gate-documents").display().to_string(),
        ),
        (
            "LOCALAPPDATA".into(),
            gate.temp.join("gate-localappdata").display().to_string(),
        ),
        ("CARGO_BUILD_JOBS".into(), jobs.to_string()),
        ("CARGO_INCREMENTAL".into(), "0".into()),
        ("PYTHONUTF8".into(), "1".into()),
        ("RUST_TEST_THREADS".into(), jobs.to_string()),
        (
            "DELTA_TEST_ROOT".into(),
            gate.temp.join("tests").display().to_string(),
        ),
    ]);
    for (id, program, args) in [
        ("M3-G-01", "python", vec!["scripts/check_docs.py"]),
        ("M3-G-02", "git", vec!["diff", "--check"]),
        ("M3-G-03", "cargo", vec!["fmt", "--all", "--check"]),
        (
            "M3-G-04",
            "cargo",
            vec![
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ],
        ),
        ("M3-G-05", "cargo", vec!["test", "--workspace", "--locked"]),
    ] {
        let started = Instant::now();
        let result = run_quiet_command(
            OsStr::new(program),
            args.clone(),
            Some(&gate.root),
            &gate_env,
        )
        .map_err(|e| failure("gate_command", e))?;
        let log = gate.log_json(id, &result);
        if id == "M3-G-05" {
            record_tests(&result.stdout, &mut gate.report.test_results);
        }
        let passed = result.status_code == Some(0) && !result.timed_out;
        gate.report.case_results.push(CaseResultRecord {
            case_id: id.into(),
            work_package: "M3-W06".into(),
            tier: "required".into(),
            status: if passed { "passed" } else { "failed" }.into(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            evidence: format!(
                "{program} {}; exit={:?}; log {log}",
                args.join(" "),
                result.status_code
            ),
        });
        if !passed {
            return Err(failure("gate_failed", format!("{id}: see {log}")));
        }
    }
    case_from_tests(
        &mut gate.report,
        "M3-A-01",
        "M3-W01",
        "required",
        &[
            "version::tests::m3_a01_version_derivation_for_release_dev_and_test_identities",
            "identity::tests::identities_are_fully_isolated_and_test_identities_never_compile_as_release",
        ],
    );
    case_from_tests(
        &mut gate.report,
        "M3-A-02",
        "M3-W01",
        "required",
        &["instance::tests::m3_a02_single_instance_and_control_pipe_reject_cross_identity_requests"],
    );
    case_from_tests(
        &mut gate.report,
        "M3-A-07",
        "M3-W04",
        "required",
        &[
            "update::tests::m3_a07_signature_downgrade_and_transport_policy",
            "update::tests::m3_a08_loopback_check_resume_and_redirect_rejection",
            "update::tests::update_preferences_default_on_and_respect_interval",
        ],
    );
    Ok(())
}

/// Offline contracts that need no installer (`S-35` default mode): template directives,
/// embedded key file, identity isolation.
fn offline_contracts(gate: &mut Gate) {
    let started = Instant::now();
    let outcome = (|| -> Result<String, ErrorEnvelope> {
        let template = read_lossy(&gate.root.join("crates/delta-cli/installer/delta.iss"));
        for directive in [
            "PrivilegesRequired=lowest",
            "ArchitecturesAllowed=x64compatible",
            "DefaultDirName={localappdata}\\Programs\\{#Identity}",
            "AppMutex={#MutexName}",
            "CloseApplications=no",
            "RestartApplications=no",
            "ShowLanguageDialog=no",
            "EncodeVer(7, 1, 0)",
            "compiler:Languages\\ChineseSimplified.isl",
            "Flags: unchecked",
            "__migrate-portable",
            "control quit-for-update",
        ] {
            ensure(
                template.contains(directive),
                format!("delta.iss lacks {directive}"),
            )?;
        }
        for forbidden in [
            "[UninstallDelete]",
            "{userappdata}",
            "{userdocs}",
            "taskkill",
        ] {
            ensure(
                !template.contains(forbidden),
                format!("delta.iss must not contain {forbidden}"),
            )?;
        }
        ensure(
            template.is_ascii(),
            "delta.iss must stay ASCII (wizard text comes from the .isl files)",
        )?;
        let key: Value =
            serde_json::from_str(update::RELEASE_KEY_JSON).map_err(|e| failure("M3_CHECK", e))?;
        let embedded = key["public_key_hex"].as_str().unwrap_or_default();
        let key_state = if embedded.is_empty() {
            "release public key not embedded yet (ACT-10): release updates fail closed"
        } else {
            update::parse_public_key(embedded).map_err(|e| failure("M3_CHECK", e))?;
            "release public key embedded and valid"
        };
        let identities = [
            InstallIdentity::Release,
            InstallIdentity::Dev,
            InstallIdentity::Test,
            InstallIdentity::DevTest,
        ];
        let distinct = |f: &dyn Fn(InstallIdentity) -> String| {
            identities
                .iter()
                .map(|i| f(*i))
                .collect::<BTreeSet<_>>()
                .len()
                == identities.len()
        };
        ensure(
            distinct(&|i| i.app_id().to_string())
                && distinct(&|i| i.mutex_name())
                && distinct(&|i| i.pipe_name())
                && distinct(&|i| i.desktop_exe().to_string())
                && distinct(&|i| i.app_user_model_id().to_string()),
            "identity values are not distinct",
        )?;
        Ok(format!(
            "delta.iss directives present, no data deletion; {key_state}; four identities have distinct AppId, mutex, pipe, executable and AUMID"
        ))
    })();
    gate.record("M3-G-06", "M3-W02,M3-W04", started, outcome);
}

fn regression_reports(gate: &Gate) -> Result<String, ErrorEnvelope> {
    let mut parts = Vec::new();
    for (stage, plan) in [("m1", "2.6"), ("m2", "1.0")] {
        let path = gate
            .root
            .join(format!(".local/reports/{stage}/verify-stage-report.json"));
        let report: Value = serde_json::from_slice(&fs::read(&path).map_err(|e| {
            failure(
                "REGRESSION",
                format!("{stage} report missing ({e}); run verify-stage {stage} first"),
            )
        })?)
        .map_err(|e| failure("REGRESSION", e))?;
        ensure(
            report["plan_version"] == plan
                && report["overall_status"] == "passed"
                && report["source_fingerprint"] == gate.report.source_fingerprint.as_str(),
            format!(
                "{stage} regression must be passed for this source fingerprint (found {} / {})",
                report["overall_status"], report["source_fingerprint"]
            ),
        )?;
        parts.push(format!(
            "{} v{plan} passed at {} for this fingerprint",
            stage.to_uppercase(),
            report["executed_at"].as_str().unwrap_or("?")
        ));
    }
    Ok(parts.join("; "))
}

/// Remove everything the gate created for the test identities and confirm it is gone.
fn cleanup_test_roots(gate: &Gate) -> Result<String, ErrorEnvelope> {
    let mut problems = Vec::new();
    for identity in TEST_IDENTITIES {
        if delta_app::identity_is_running(identity) {
            let _ = quit_running(identity, Duration::from_secs(30));
        }
        if installed_version(identity).is_some() {
            let log = gate
                .logs
                .join(format!("cleanup-uninstall-{}.log", identity.name()));
            let _ = run_uninstall_silently(identity, &log);
        }
        if let Some(directory) = install_dir(identity) {
            if directory.exists() {
                if let Err(error) = fs::remove_dir_all(&directory) {
                    problems.push(format!("{}: {error}", directory.display()));
                }
            }
        }
        if delta_app::identity_is_running(identity) {
            problems.push(format!("{} still running", identity.name()));
        }
        if installed_version(identity).is_some() {
            problems.push(format!("{} uninstall key remains", identity.name()));
        }
        if shortcut(identity).is_some_and(|link| link.exists()) {
            problems.push(format!("{} shortcut remains", identity.name()));
        }
    }
    if problems.is_empty() {
        Ok("test installs, keys, shortcuts and data roots removed; test mutexes released".into())
    } else {
        Err(failure("CLEANUP", problems.join("; ")))
    }
}

// ---------------------------------------------------------------------------
// --installer cases
// ---------------------------------------------------------------------------

struct Packages {
    test_v1: InstallerArtifact,
    test_v2: InstallerArtifact,
    dev_test_v1: InstallerArtifact,
    dev_test_v2: InstallerArtifact,
}

fn installer_cases(gate: &mut Gate) {
    let pending = [
        ("M3-A-05", "M3-W02"),
        ("M3-A-03", "M3-W02"),
        ("M3-A-04", "M3-W02,M3-W03"),
        ("M3-A-06", "M3-W03"),
        ("M3-A-09", "M3-W05"),
        ("M3-A-08", "M3-W04,M3-W05"),
    ];
    let mut done: BTreeSet<&str> = BTreeSet::new();
    let started = Instant::now();
    let blocked = |gate: &mut Gate, done: &BTreeSet<&str>, reason: &str| {
        for (id, package) in pending {
            if !done.contains(id) {
                gate.record(
                    id,
                    package,
                    started,
                    Err(failure("BLOCKED", format!("not reached: {reason}"))),
                );
            }
        }
    };

    // Preflight: leftovers of an earlier run are removed; a running test app stops the gate.
    if let Err(error) = preflight(gate) {
        blocked(gate, &done, &error.message);
        poc(gate, "M3-K01", "Inno per-user install", true, false);
        poc(gate, "M3-K02", "AppMutex and control pipe", true, false);
        poc(gate, "M3-K03", "Ed25519 and loopback update", true, false);
        return;
    }
    let build_started = Instant::now();
    let packages = match build_packages(gate) {
        Ok(packages) => packages,
        Err(error) => {
            gate.report
                .errors
                .push(format!("package build: {}", error.message));
            blocked(
                gate,
                &done,
                &format!("package build failed: {}", error.message),
            );
            poc(gate, "M3-K01", "Inno per-user install", true, false);
            poc(gate, "M3-K02", "AppMutex and control pipe", true, false);
            poc(gate, "M3-K03", "Ed25519 and loopback update", true, false);
            return;
        }
    };
    let build_ms = build_started.elapsed().as_millis();

    let started = Instant::now();
    let a05 = case_a05(gate, &packages);
    let ok05 = gate.record("M3-A-05", "M3-W02", started, a05);
    done.insert("M3-A-05");
    if !ok05 {
        blocked(gate, &done, "M3-A-05 failed");
        poc(gate, "M3-K01", "Inno per-user install", true, false);
        poc(gate, "M3-K02", "AppMutex and control pipe", true, false);
        poc(gate, "M3-K03", "Ed25519 and loopback update", true, false);
        return;
    }

    let started_a03 = Instant::now();
    let a03_install = case_a03_install(gate, &packages);
    if let Err(error) = &a03_install {
        let error = error.clone();
        gate.record("M3-A-03", "M3-W02", started_a03, Err(error));
        done.insert("M3-A-03");
        blocked(gate, &done, "M3-A-03 failed");
        poc(gate, "M3-K01", "Inno per-user install", true, false);
        poc(gate, "M3-K02", "AppMutex and control pipe", true, false);
        poc(gate, "M3-K03", "Ed25519 and loopback update", true, false);
        return;
    }
    let a03_install_ms = started_a03.elapsed();

    let started = Instant::now();
    let a04 = case_a04(gate, &packages);
    let ok04 = gate.record("M3-A-04", "M3-W02,M3-W03", started, a04);
    done.insert("M3-A-04");

    let started = Instant::now();
    let a06 = case_a06(gate, &packages);
    gate.record("M3-A-06", "M3-W03", started, a06);
    done.insert("M3-A-06");

    let started = Instant::now();
    let a09 = case_a09(gate, &packages);
    let (ok09, release) = match a09 {
        Ok((evidence, release)) => {
            gate.record("M3-A-09", "M3-W05", started, Ok(evidence));
            (true, Some(release))
        }
        Err(error) => {
            gate.record("M3-A-09", "M3-W05", started, Err(error));
            (false, None)
        }
    };
    done.insert("M3-A-09");

    let started = Instant::now();
    let a08 = match (&release, ok04) {
        (Some(release), true) => case_a08(gate, &packages, release),
        _ => Err(failure(
            "BLOCKED",
            "needs the M3-A-09 test release and the M3-A-04 upgrade state",
        )),
    };
    let ok08 = gate.record("M3-A-08", "M3-W04,M3-W05", started, a08);
    done.insert("M3-A-08");

    // A-03 finishes with the silent uninstall of both identities.
    let started = Instant::now();
    let a03 = case_a03_uninstall(gate, a03_install.unwrap_or_default());
    let ok03 = gate.record(
        "M3-A-03",
        "M3-W02",
        started - a03_install_ms,
        a03.map(|e| format!("{e}; packages built in {build_ms} ms")),
    );
    done.insert("M3-A-03");
    poc(gate, "M3-K01", "Inno per-user install", true, ok03 && ok05);
    poc(gate, "M3-K02", "AppMutex and control pipe", true, ok04);
    poc(
        gate,
        "M3-K03",
        "Ed25519 and loopback update",
        true,
        ok08 && ok09,
    );
}

fn preflight(gate: &mut Gate) -> Result<(), ErrorEnvelope> {
    installer::find_iscc()?;
    for identity in TEST_IDENTITIES {
        ensure(
            !delta_app::identity_is_running(identity),
            format!(
                "{} is running; close it before the gate (it is never force-closed)",
                identity.name()
            ),
        )?;
        if installed_version(identity).is_some() {
            let log = gate.log_path(&format!("preflight-uninstall-{}.log", identity.name()));
            run_uninstall_silently(identity, &log)?;
        }
        if let Some(directory) = install_dir(identity) {
            if directory.exists() {
                fs::remove_dir_all(&directory).map_err(|e| failure("PREFLIGHT", e))?;
            }
        }
        if let Some(link) = shortcut(identity) {
            if link.exists() {
                fs::remove_file(&link).map_err(|e| failure("PREFLIGHT", e))?;
            }
        }
        fs::create_dir_all(gate.data_dir(identity)).map_err(|e| failure("PREFLIGHT", e))?;
    }
    Ok(())
}

fn build_packages(gate: &mut Gate) -> Result<Packages, ErrorEnvelope> {
    let output = gate.temp.join("packages");
    let context = PackagingContext::prepare(&gate.root, &output)?;
    let base = Utc::now();
    let at = |seconds: i64| -> Result<String, ErrorEnvelope> {
        installer::default_dev_version(&gate.root, base + chrono::Duration::seconds(seconds))
    };
    let (v1, v2) = (at(0)?, at(61)?);
    let packages = Packages {
        test_v1: context.build(InstallIdentity::Test, &v1, &output)?,
        test_v2: context.build(InstallIdentity::Test, &v2, &output)?,
        dev_test_v1: context.build(InstallIdentity::DevTest, &v1, &output)?,
        dev_test_v2: context.build(InstallIdentity::DevTest, &v2, &output)?,
    };
    gate.log_json(
        "packages",
        &json!([
            packages.test_v1,
            packages.test_v2,
            packages.dev_test_v1,
            packages.dev_test_v2
        ]),
    );
    // A-01: versions that must be refused before anything is built.
    let refused = [
        (InstallIdentity::Test, "0.1.0".to_string()),
        (
            InstallIdentity::Test,
            "0.0.1-dev.20200101000000".to_string(),
        ),
        (InstallIdentity::Release, "0.0.9".to_string()),
        (
            InstallIdentity::Release,
            "0.1.1-dev.20200101000000".to_string(),
        ),
        (InstallIdentity::DevTest, "v0.1".to_string()),
    ];
    for (identity, version) in &refused {
        ensure(
            context.validate_version(*identity, version).is_err(),
            format!("{} accepted invalid version {version}", identity.name()),
        )?;
    }
    Ok(packages)
}

fn setup_log(gate: &mut Gate, name: &str) -> PathBuf {
    gate.log_path(&format!("{name}.log"))
}

/// M3-A-05: an M2 portable layout in `Programs\delta-test` is migrated by the v1 installer.
fn case_a05(gate: &mut Gate, packages: &Packages) -> Result<String, ErrorEnvelope> {
    let identity = InstallIdentity::Test;
    let directory = install_dir(identity).ok_or_else(|| failure("A05", "no LOCALAPPDATA"))?;
    fs::create_dir_all(directory.join("third-party-licenses")).map_err(|e| failure("A05", e))?;
    let mut files = BTreeMap::new();
    for (name, bytes) in [
        (
            identity.desktop_exe().to_string(),
            b"m2 portable desktop".to_vec(),
        ),
        (identity.cli_exe().to_string(), b"m2 portable cli".to_vec()),
        ("README.txt".into(), b"m2 readme".to_vec()),
        ("LICENSE".into(), b"m2 license".to_vec()),
        ("binary-imports.json".into(), b"{}".to_vec()),
        (
            "third-party-licenses/example.txt".into(),
            b"m2 notice".to_vec(),
        ),
        (
            "package-manifest.json".into(),
            b"{\"fixture\":true}".to_vec(),
        ),
    ] {
        let path = directory.join(&name);
        fs::write(&path, bytes).map_err(|e| failure("A05", e))?;
        files.insert(name, sha256_file(&path).map_err(|e| failure("A05", e))?);
    }
    let manifest = crate::packaging::InstallManifest {
        schema_version: 1,
        product: identity.name().into(),
        channel: "dev".into(),
        version: "0.1.0".into(),
        install_dir: directory.clone(),
        files,
    };
    fs::write(
        directory.join("install-manifest.json"),
        serde_json::to_vec_pretty(&manifest).map_err(|e| failure("A05", e))?,
    )
    .map_err(|e| failure("A05", e))?;
    fs::write(
        directory.join("README.txt"),
        b"m2 readme edited by the user",
    )
    .map_err(|e| failure("A05", e))?;
    fs::write(directory.join("user-notes.txt"), b"unknown user file")
        .map_err(|e| failure("A05", e))?;

    let log = setup_log(gate, "a05-setup-test-v1");
    let run = run_setup_silently(&packages.test_v1.setup_path, &log)?;
    let text = read_lossy(&log);
    ensure(
        run.exit_code == Some(0) && !run.timed_out,
        format!("setup exit {:?}", run.exit_code),
    )?;
    ensure(
        installed_version(identity).as_deref() == Some(packages.test_v1.version.as_str()),
        "installed version differs after migration",
    )?;
    ensure(
        read_lossy(&directory.join("README.txt.m2-modified")) == "m2 readme edited by the user",
        "the modified README was not kept",
    )?;
    ensure(
        directory.join("user-notes.txt").is_file(),
        "unknown file was removed",
    )?;
    ensure(
        !directory.join("install-manifest.json").exists()
            && !directory.join("package-manifest.json").exists()
            && !directory.join("binary-imports.json").exists()
            && !directory.join("third-party-licenses/example.txt").exists(),
        "M2 manifest-owned files remain",
    )?;
    ensure(
        text.contains("M2 portable installation found")
            && text.contains("README.txt.m2-modified")
            && text.contains("user-notes.txt"),
        "setup log does not list the kept files",
    )?;
    let info = verify_identity_binary(
        &directory.join(identity.desktop_exe()),
        "delta-desktop",
        identity,
        &packages.test_v1.version,
    )?;
    Ok(format!(
        "M2 layout (7 manifest files, 1 edited, 1 unknown) migrated by {} in {} ms: verified files and both manifests removed, README.txt kept as README.txt.m2-modified, user-notes.txt kept, both listed in {}; installed app reports {}",
        packages.test_v1.setup_path.file_name().unwrap_or_default().to_string_lossy(),
        run.elapsed_ms,
        log.file_name().unwrap_or_default().to_string_lossy(),
        info["version"]
    ))
}

/// M3-A-03 (first half) and M3-A-01 binary evidence: both test identities installed side by side.
fn case_a03_install(gate: &mut Gate, packages: &Packages) -> Result<String, ErrorEnvelope> {
    let log = setup_log(gate, "a03-setup-dev-test-v1");
    let run = run_setup_silently(&packages.dev_test_v1.setup_path, &log)?;
    ensure(
        run.exit_code == Some(0) && !run.timed_out,
        format!("dev-test setup exit {:?}", run.exit_code),
    )?;
    let mut evidence = Vec::new();
    for (identity, artifact) in [
        (InstallIdentity::Test, &packages.test_v1),
        (InstallIdentity::DevTest, &packages.dev_test_v1),
    ] {
        let directory = install_dir(identity).ok_or_else(|| failure("A03", "no LOCALAPPDATA"))?;
        ensure(
            installed_version(identity).as_deref() == Some(artifact.version.as_str()),
            format!("{} DisplayVersion differs", identity.name()),
        )?;
        let location = installed_value(identity, "InstallLocation").unwrap_or_default();
        ensure(
            Path::new(location.trim_end_matches('\\')) == directory,
            format!("{} InstallLocation {location}", identity.name()),
        )?;
        let inno = installed_value(identity, "Inno Setup: Setup Version").unwrap_or_default();
        ensure(
            inno.starts_with("7.1."),
            format!("{} built with Inno Setup {inno}", identity.name()),
        )?;
        ensure(
            installed_value(identity, "QuietUninstallString").is_some(),
            "QuietUninstallString missing",
        )?;
        ensure(
            shortcut(identity).is_some_and(|link| link.is_file()),
            format!("{} Start-menu shortcut missing", identity.name()),
        )?;
        let desktop = directory.join(identity.desktop_exe());
        verify_identity_binary(&desktop, "delta-desktop", identity, &artifact.version)?;
        verify_identity_binary(
            &directory.join(identity.cli_exe()),
            "delta-cli",
            identity,
            &artifact.version,
        )?;
        let parsed = semver::Version::parse(&artifact.version).map_err(|e| failure("A03", e))?;
        let (product_version, numbers) =
            file_version_info(&desktop).ok_or_else(|| failure("A01", "no VERSIONINFO"))?;
        ensure(
            product_version == artifact.version
                && numbers
                    == [
                        parsed.major as u16,
                        parsed.minor as u16,
                        parsed.patch as u16,
                        0,
                    ],
            format!("VERSIONINFO {product_version} {numbers:?}"),
        )?;
        let mut smoke = String::from("ui-smoke not requested");
        if gate.ui_smoke {
            let base = gate.temp.join(identity.name()).join("smoke");
            fs::create_dir_all(&base).map_err(|e| failure("A03", e))?;
            let result = run_quiet_command(
                desktop.as_os_str(),
                ["--ui-smoke", "--data-dir", &base.display().to_string()],
                None,
                &gate.identity_env(identity),
            )
            .map_err(|e| failure("A03", e))?;
            let name = format!("a03-ui-smoke-{}", identity.name());
            gate.log_json(&name, &result);
            let value: Value = serde_json::from_str(result.stdout.trim()).unwrap_or(Value::Null);
            ensure(
                result.status_code == Some(0)
                    && !result.timed_out
                    && value["build_info"]["identity"] == identity.name()
                    && value["build_info"]["version"] == artifact.version.as_str(),
                format!("installed --ui-smoke failed; see {name}"),
            )?;
            smoke = "installed --ui-smoke passed (hidden window)".into();
        }
        fs::write(
            gate.data_dir(identity).join(UNKNOWN_DATA_FILE),
            b"must survive uninstall",
        )
        .map_err(|e| failure("A03", e))?;
        evidence.push(format!(
            "{} {} in Programs/{} (HKCU key, Inno {inno}, Start-menu shortcut, build-info and VERSIONINFO {product_version} {numbers:?}, {smoke})",
            identity.name(),
            artifact.version,
            identity.name()
        ));
    }
    Ok(format!(
        "{}; silent per-user install without elevation, setup {} ms",
        evidence.join("; "),
        run.elapsed_ms
    ))
}

/// M3-A-03 (second half): silent uninstall removes files, key and shortcut, keeps data.
fn case_a03_uninstall(gate: &mut Gate, installed: String) -> Result<String, ErrorEnvelope> {
    let mut evidence = vec![installed];
    for identity in TEST_IDENTITIES {
        ensure(
            !delta_app::identity_is_running(identity),
            format!("{} still running", identity.name()),
        )?;
        let log = setup_log(gate, &format!("a03-uninstall-{}", identity.name()));
        let run = run_uninstall_silently(identity, &log)?;
        let directory = install_dir(identity).ok_or_else(|| failure("A03", "no LOCALAPPDATA"))?;
        ensure(
            run.exit_code == Some(0) && !run.timed_out,
            format!("uninstall exit {:?}", run.exit_code),
        )?;
        ensure(
            installed_version(identity).is_none(),
            "uninstall key remains",
        )?;
        ensure(
            !directory.join(identity.desktop_exe()).exists()
                && !directory.join(identity.cli_exe()).exists()
                && !directory.join("unins000.exe").exists(),
            "installed files remain",
        )?;
        ensure(
            !shortcut(identity).is_some_and(|link| link.exists()),
            "shortcut remains",
        )?;
        let data = gate.data_dir(identity);
        ensure(
            read_lossy(&data.join(UNKNOWN_DATA_FILE)) == "must survive uninstall",
            "data root or the unknown file in it was removed",
        )?;
        ensure(
            data.join("library.sqlite3").is_file()
                || fs::read_dir(&data).is_ok_and(|d| d.count() > 1),
            "library data removed",
        )?;
        let kept = if identity == InstallIdentity::Test {
            // Files kept by the M2 migration are not the installer's to delete.
            ensure(
                directory.join("user-notes.txt").is_file()
                    && directory.join("README.txt.m2-modified").is_file(),
                "files kept by the migration were removed",
            )?;
            "; user files kept by the migration remain"
        } else {
            ensure(!directory.exists(), "install directory remains")?;
            ""
        };
        evidence.push(format!(
            "{} uninstalled silently in {} ms: files, HKCU key and shortcut removed, data root with {UNKNOWN_DATA_FILE} kept{kept}",
            identity.name(),
            run.elapsed_ms
        ));
    }
    Ok(evidence.join("; "))
}

/// M3-A-04: busy and stuck apps keep v1; an idle app saves its pending note and is upgraded.
fn case_a04(gate: &mut Gate, packages: &Packages) -> Result<String, ErrorEnvelope> {
    let identity = InstallIdentity::Test;
    let data = gate.data_dir(identity);
    let environment = gate.identity_env(identity);
    let v1 = packages.test_v1.version.clone();
    let v2 = packages.test_v2.version.clone();

    // Busy: sync running, the app answers busy, setup aborts and changes nothing.
    let mut resident = Resident::spawn(
        identity,
        &data,
        &[
            "--verification-seed",
            "--verification-busy-sync-ms",
            "50000",
            "--verification-max-seconds",
            "180",
        ],
        &environment,
    )?;
    resident.wait_event("resident-ready", Duration::from_secs(120))?;
    let log = setup_log(gate, "a04-setup-busy");
    let busy = run_setup_silently(&packages.test_v2.setup_path, &log)?;
    ensure(busy.exit_code != Some(0), "setup upgraded a busy app")?;
    ensure(
        installed_version(identity).as_deref() == Some(v1.as_str()),
        "busy case changed the installed version",
    )?;
    ensure(
        delta_app::identity_is_running(identity),
        "the busy app was closed",
    )?;
    ensure(
        read_lossy(&log).contains("installation aborted"),
        "setup log lacks the abort reason",
    )?;
    let busy_events = resident.names();
    ensure(
        busy_events.iter().any(|e| e == "exit-refused"),
        format!("busy app did not refuse: {busy_events:?}"),
    )?;
    quit_running(identity, Duration::from_secs(90))?;
    ensure(
        resident.wait_exit(Duration::from_secs(30))? == Some(0),
        "busy app exit code",
    )?;
    drop(resident);

    // Stuck: the app never answers; setup gives up after its wait and changes nothing.
    let mut resident = Resident::spawn(
        identity,
        &data,
        &[
            "--verification-ignore-control",
            "--verification-max-seconds",
            "110",
        ],
        &environment,
    )?;
    resident.wait_event("resident-ready", Duration::from_secs(60))?;
    let log = setup_log(gate, "a04-setup-stuck");
    let stuck = run_setup_silently(&packages.test_v2.setup_path, &log)?;
    ensure(stuck.exit_code != Some(0), "setup upgraded a stuck app")?;
    ensure(
        installed_version(identity).as_deref() == Some(v1.as_str()),
        "stuck case changed the installed version",
    )?;
    ensure(
        delta_app::identity_is_running(identity),
        "the stuck app was killed",
    )?;
    resident.wait_event("resident-timeout", Duration::from_secs(150))?;
    resident.wait_exit(Duration::from_secs(30))?;
    drop(resident);

    // Idle: pending note is saved, the app exits, v2 is installed and data is kept.
    let before = inspect_library(&data)?;
    let mut resident = Resident::spawn(
        identity,
        &data,
        &[
            "--verification-pending-note",
            PENDING_NOTE_BODY,
            "--verification-max-seconds",
            "180",
        ],
        &environment,
    )?;
    resident.wait_event("resident-ready", Duration::from_secs(60))?;
    let log = setup_log(gate, "a04-setup-upgrade");
    let upgrade = run_setup_silently(&packages.test_v2.setup_path, &log)?;
    ensure(
        upgrade.exit_code == Some(0) && !upgrade.timed_out,
        format!(
            "upgrade setup exit {:?}; events {:?}",
            upgrade.exit_code,
            resident.names()
        ),
    )?;
    ensure(
        resident.wait_exit(Duration::from_secs(30))? == Some(0),
        "app exit code",
    )?;
    let events = resident.names();
    for expected in [
        "control-quit-for-update",
        "exit-note-saved",
        "exit-for-update",
    ] {
        ensure(
            events.iter().any(|e| e == expected),
            format!("missing {expected}: {events:?}"),
        )?;
    }
    drop(resident);
    ensure(
        installed_version(identity).as_deref() == Some(v2.as_str()),
        "upgrade did not install v2",
    )?;
    verify_identity_binary(
        &install_dir(identity)
            .unwrap_or_default()
            .join(identity.desktop_exe()),
        "delta-desktop",
        identity,
        &v2,
    )?;
    let after = inspect_library(&data)?;
    ensure_library_kept(&before, &after)?;
    ensure(
        after
            .notes
            .values()
            .any(|(_, body)| body == PENDING_NOTE_BODY),
        "the pending note was not saved before exit",
    )?;
    gate.log_json("a04-library", &json!({"before": before, "after": after}));
    Ok(format!(
        "busy sync: app answered busy, setup aborted after {} ms, still {v1}, app kept running; ignoring control: setup aborted after {} ms, still {v1}, app ended on its own deadline; idle: quit-for-update saved the pending note and layout, exited 0, {v1} -> {v2} in {} ms; {} accounts, {} notes, {} attachments verified, settings kept",
        busy.elapsed_ms,
        stuck.elapsed_ms,
        upgrade.elapsed_ms,
        after.accounts,
        after.notes.len(),
        after.attachments_verified
    ))
}

/// M3-A-06: `install-dev` (through the real CLI, `--test-identity` = `delta-dev-test`).
fn case_a06(gate: &mut Gate, packages: &Packages) -> Result<String, ErrorEnvelope> {
    let identity = InstallIdentity::DevTest;
    let data = gate.data_dir(identity);
    let environment = gate.identity_env(identity);
    let cli = env::current_exe().map_err(|e| failure("A06", e))?;
    let mut resident = Resident::spawn(
        identity,
        &data,
        &["--verification-seed", "--verification-max-seconds", "200"],
        &environment,
    )?;
    resident.wait_event("resident-ready", Duration::from_secs(120))?;
    let before = {
        // The library is locked while the app runs; compare against the restarted app later.
        resident.event("resident-ready").unwrap_or_default()
    };
    let install_dev = |gate: &mut Gate, package: &Path, name: &str| {
        let result = run_quiet_command_with_timeout(
            cli.as_os_str(),
            [
                "install-dev".to_string(),
                "--test-identity".into(),
                "--package".into(),
                package.display().to_string(),
            ],
            Some(&gate.root),
            &environment,
            Duration::from_secs(300),
        )
        .map_err(|e| failure("A06", e));
        if let Ok(result) = &result {
            gate.log_json(name, result);
        }
        result
    };
    for (package, name, code) in [
        (
            &packages.test_v2,
            "a06-refuse-identity",
            "INSTALL_DEV_IDENTITY",
        ),
        (
            &packages.dev_test_v1,
            "a06-refuse-downgrade",
            "INSTALL_DEV_DOWNGRADE",
        ),
    ] {
        let result = install_dev(gate, &package.setup_path, name)?;
        ensure(
            result.status_code != Some(0) && result.stderr.contains(code),
            format!("install-dev did not refuse with {code}"),
        )?;
        ensure(
            installed_version(identity).as_deref() == Some(packages.dev_test_v1.version.as_str())
                && delta_app::identity_is_running(identity),
            "a refused install-dev changed the installation or stopped the app",
        )?;
    }
    let result = install_dev(gate, &packages.dev_test_v2.setup_path, "a06-install-dev")?;
    let receipt: Value = serde_json::from_str(result.stdout.trim()).unwrap_or(Value::Null);
    ensure(
        result.status_code == Some(0)
            && receipt["was_running"] == true
            && receipt["restarted"] == true
            && receipt["installed_version"] == packages.dev_test_v2.version.as_str(),
        format!("install-dev failed: {}", result.stderr.trim()),
    )?;
    ensure(
        resident.wait_exit(Duration::from_secs(30))? == Some(0),
        "the app did not exit cleanly",
    )?;
    drop(resident);
    let restarted = receipt["restart_process_id"].clone();
    let restarted_ready = wait_until(Duration::from_secs(90), || {
        resident_events(&data)
            .iter()
            .any(|event| event["event"] == "resident-ready" && event["process_id"] == restarted)
    });
    // Liveness is a real round-trip over the relaunched app's own control pipe.
    let (focus_code, focus) =
        installer::control(identity, ControlCommand::Focus, Duration::from_secs(15));
    let mutex_held = delta_app::identity_is_running(identity);
    ensure(
        restarted_ready
            && focus_code == 0
            && focus["reply"]["version"] == packages.dev_test_v2.version.as_str(),
        format!(
            "the app was not restarted: ready event {restarted_ready}, focus {focus_code} {focus}, mutex {mutex_held}; events of pid {restarted}: {:?}",
            resident_events(&data)
                .into_iter()
                .filter(|event| event["process_id"] == restarted)
                .collect::<Vec<_>>()
        ),
    )?;
    let restarted_args = resident_events(&data)
        .into_iter()
        .find(|event| event["event"] == "resident-started" && event["process_id"] == restarted)
        .map(|event| event["detail"]["args"].clone())
        .unwrap_or_default();
    quit_running(identity, Duration::from_secs(60))?;
    let after = inspect_library(&data)?;
    ensure(
        after.notes.contains_key("note:m3-seed") && after.attachments_broken == 0,
        "library changed by install-dev",
    )?;
    verify_identity_binary(
        &install_dir(identity)
            .unwrap_or_default()
            .join(identity.desktop_exe()),
        "delta-desktop",
        identity,
        &packages.dev_test_v2.version,
    )?;
    Ok(format!(
        "delta-test package refused (INSTALL_DEV_IDENTITY) and same-version package refused (INSTALL_DEV_DOWNGRADE) without changes; running app (pid {}) saved and exited, {} -> {} silently, restarted as pid {restarted} with its previous arguments {restarted_args} and answered focus as {} (mutex held: {mutex_held}), then quit on request; library kept",
        before["detail"]["process_id"],
        packages.dev_test_v1.version,
        packages.dev_test_v2.version,
        focus["reply"]["version"]
    ))
}

struct TestRelease {
    artifacts: installer::ReleaseArtifacts,
}

/// M3-A-09: `release-build` refuses without a key, then signs a test-identity release.
fn case_a09(gate: &mut Gate, packages: &Packages) -> Result<(String, TestRelease), ErrorEnvelope> {
    let run = gate
        .temp
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("run")
        .trim_start_matches("delta_m3_verify_")
        .to_string();
    let version =
        installer::default_dev_version(&gate.root, Utc::now() + chrono::Duration::seconds(122))?;
    ensure(
        semver::Version::parse(&version).ok()
            > semver::Version::parse(&packages.test_v2.version).ok(),
        "release version not above v2",
    )?;
    let missing_output = gate.temp.join("release-missing-key");
    let keys = gate.temp.join("release-keys");
    let missing = installer::release_build(
        &gate.root,
        &missing_output,
        InstallIdentity::Test,
        &version,
        &keys.join(format!("missing-{run}.key")),
    );
    ensure(
        matches!(&missing, Err(error) if error.code == "RELEASE_KEY_MISSING"),
        "release-build did not refuse without a key",
    )?;
    ensure(
        !missing_output.exists(),
        "release-build left output without a key",
    )?;
    // D-32: the signing key is a file; the rehearsal uses a throw-away key in the gate temp.
    let key_file = keys.join(format!("signing-{run}.key"));
    ensure(
        installer::default_release_key_file().map_or(true, |default| default != key_file),
        "rehearsal key file must not be the release key file",
    )?;
    let generated = installer::release_keygen(&key_file, &gate.root)?;
    let public_hex = generated["public_key_hex"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    ensure(
        matches!(installer::release_keygen(&key_file, &gate.root), Err(error) if error.code == "RELEASE_KEY_EXISTS"),
        "release-keygen replaced an existing key",
    )?;
    ensure(
        matches!(
            installer::release_keygen(&gate.root.join(".local/a09-key-probe/in-repo.key"), &gate.root),
            Err(error) if error.code == "RELEASE_KEY_STORE"
        ) && !gate.root.join(".local/a09-key-probe").exists(),
        "release-keygen wrote a key inside the repository",
    )?;
    let release = TestRelease {
        artifacts: installer::release_build(
            &gate.root,
            &gate.temp.join("release"),
            InstallIdentity::Test,
            &version,
            &key_file,
        )?,
    };
    let artifacts = &release.artifacts;
    let manifest: update::UpdateManifest = serde_json::from_slice(
        &fs::read(artifacts.directory.join(update::MANIFEST_FILE))
            .map_err(|e| failure("A09", e))?,
    )
    .map_err(|e| failure("A09", e))?;
    let signature = read_lossy(&artifacts.signature_file);
    let public = update::parse_public_key(&public_hex).map_err(|e| failure("A09", e))?;
    ensure(
        manifest == artifacts.manifest
            && manifest.channel == "test"
            && manifest.version == version
            && manifest.file == InstallIdentity::Test.setup_file_name(&version)
            && manifest.sha256 == sha256_file(&artifacts.setup).map_err(|e| failure("A09", e))?
            && manifest.size
                == fs::metadata(&artifacts.setup)
                    .map_err(|e| failure("A09", e))?
                    .len()
            && artifacts.public_key_hex == public_hex,
        "latest.json does not describe the installer",
    )?;
    update::verify_manifest_signature(&public, &manifest, &signature)
        .map_err(|e| failure("A09", e))?;
    let mut forged = manifest.clone();
    forged.channel = "release".into();
    ensure(
        update::verify_manifest_signature(&public, &forged, &signature).is_err(),
        "signature accepted for another channel",
    )?;
    for part in ["desktop", "cli"] {
        let imports = artifacts.binary_imports[part]
            .to_string()
            .to_ascii_lowercase();
        ensure(
            !imports.contains("vcruntime")
                && !imports.contains("msvcp")
                && !imports.contains("ucrtbase"),
            format!("{part} imports an MSVC runtime"),
        )?;
    }
    // The private key must not appear in any gate log, report or artifact text.
    let secret = hex::encode(installer::load_signing_key(&key_file, false)?.to_bytes());
    let mut scanned = 0;
    for directory in [
        gate.logs.clone(),
        gate.temp.join("release"),
        gate.temp.join("packages"),
    ] {
        let mut files = Vec::new();
        walk_files(&directory, &mut files);
        for file in files {
            if file.extension().is_some_and(|e| e == "exe") {
                continue;
            }
            scanned += 1;
            ensure(
                !read_lossy(&file).contains(&secret),
                format!("private key found in {}", file.display()),
            )?;
        }
    }
    Ok((
        format!(
            "no key: RELEASE_KEY_MISSING before building, no output; keygen wrote a throw-away key file, refused to overwrite it and refused a path inside the repository; release-build {version} produced {} ({} bytes), latest.json and .sig that verify with the generated public key and fail for another channel; no MSVC runtime imports; private key absent from {scanned} log/report files",
            manifest.file, manifest.size
        ),
        release,
    ))
}

/// M3-A-08: loopback online update v2 -> release v3 with busy prompt, resume, backup, restart.
fn case_a08(
    gate: &mut Gate,
    packages: &Packages,
    release: &TestRelease,
) -> Result<String, ErrorEnvelope> {
    let identity = InstallIdentity::Test;
    let data = gate.data_dir(identity);
    let artifacts = &release.artifacts;
    let target = artifacts.manifest.version.clone();
    let before = inspect_library(&data)?;
    let server = UpdateLoopbackServer::start().map_err(|e| failure("A08", e))?;
    let bytes = fs::read(&artifacts.setup).map_err(|e| failure("A08", e))?;
    let length = bytes.len();
    server.publish(
        &artifacts.manifest,
        bytes,
        Some(read_lossy(&artifacts.signature_file)),
    );
    server.interrupt_next_download_after(length / 3);
    let mut environment = gate.identity_env(identity);
    environment.insert("DELTA_UPDATE_SOURCE".into(), server.latest_url());
    environment.insert(
        "DELTA_UPDATE_PUBLIC_KEY".into(),
        artifacts.public_key_hex.clone(),
    );
    let mut resident = Resident::spawn(
        identity,
        &data,
        &[
            "--verification-apply-update",
            "--verification-busy-sync-ms",
            "20000",
            "--verification-max-seconds",
            "200",
        ],
        &environment,
    )?;
    resident.wait_event("update-busy-prompt", Duration::from_secs(120))?;
    ensure(
        resident.event("update-backup").is_none()
            && resident.event("update-installer-started").is_none()
            && installed_version(identity).as_deref() == Some(packages.test_v2.version.as_str()),
        "the busy app started updating without confirmation",
    )?;
    resident.wait_event("update-retry-when-idle", Duration::from_secs(60))?;
    let backup = resident.wait_event("update-backup", Duration::from_secs(60))?;
    let started = resident.wait_event("update-installer-started", Duration::from_secs(60))?;
    ensure(
        started["detail"]["broke_away"] == true,
        "installer did not leave the app's job",
    )?;
    ensure(
        resident.wait_exit(Duration::from_secs(60))? == Some(0),
        "the app did not exit for the update",
    )?;
    let events = resident.names();
    drop(resident);
    ensure(
        wait_until(Duration::from_secs(240), || {
            installed_version(identity).as_deref() == Some(target.as_str())
        }),
        format!("the update did not install {target}"),
    )?;
    ensure(
        wait_until(Duration::from_secs(90), || {
            delta_app::identity_is_running(identity)
        }),
        "the app was not restarted after the update",
    )?;
    let relaunched = wait_until(Duration::from_secs(60), || {
        resident_events(&data).iter().any(|event| {
            event["event"] == "resident-ready"
                && event["detail"]["build_info"]["version"] == target.as_str()
        })
    });
    ensure(relaunched, "the relaunched app did not become ready")?;
    let ranges = server.ranges_seen.lock().unwrap().clone();
    ensure(
        !ranges.is_empty(),
        "the interrupted download was not resumed",
    )?;
    let info = verify_identity_binary(
        &install_dir(identity)
            .unwrap_or_default()
            .join(identity.desktop_exe()),
        "delta-desktop",
        identity,
        &target,
    )?;
    quit_running(identity, Duration::from_secs(60))?;
    drop(server);
    let after = inspect_library(&data)?;
    ensure_library_kept(&before, &after)?;
    let backup_path = PathBuf::from(backup["detail"]["path"].as_str().unwrap_or_default());
    let restored_dir = gate.temp.join("a08-restored");
    AppService::restore_backup(&backup_path, &restored_dir)?;
    let restored = inspect_library(&restored_dir)?;
    ensure(
        restored.accounts == after.accounts
            && restored.notes.keys().collect::<Vec<_>>() == before.notes.keys().collect::<Vec<_>>()
            && restored.attachments_broken == 0,
        "the pre-update backup does not restore the library",
    )?;
    let install_log = gate.temp.join("installer-log-copy.log");
    let _ = fs::copy(
        data.join("updates").join(format!(
            "{}.install.log",
            artifacts.manifest.file.trim_end_matches(".exe")
        )),
        &install_log,
    );
    gate.log_json(
        "a08-update",
        &json!({"events": events, "ranges": ranges, "backup": backup, "installer": started}),
    );
    Ok(format!(
        "loopback source served latest.json, .sig and a 1/3 interrupted installer; app downloaded and resumed (Range {:?}), prompted while busy and did nothing, retried when idle, wrote backup {}, re-verified and started the installer outside its job, exited; {} -> {target} silently and restarted ({}); library kept; backup restored with {} notes. Loopback rehearsal only, not a GitHub release",
        ranges,
        backup_path.file_name().unwrap_or_default().to_string_lossy(),
        packages.test_v2.version,
        info["identity"],
        restored.notes.len()
    ))
}
