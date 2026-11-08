//! Installed-app lifecycle (`S-29`, `S-31`): single instance, control pipe bridge, the
//! save-then-exit sequence shared by `quit-for-update` and the in-app updater, and the
//! update state shown in the toolbar and settings.

use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

use delta_app::{
    update::{UpdateClient, UpdateOffer, UpdatePolicy, UpdatePreferences},
    ControlCommand, ControlReply, ControlStatus, InstallIdentity,
};
use serde::Serialize;

/// A control request handed from the pipe thread to the GPUI foreground loop.
pub(crate) type ControlEnvelope = (ControlCommand, mpsc::Sender<ControlReply>);

#[derive(Debug, Clone)]
pub(crate) struct LifecycleOptions {
    /// Arguments that reopen the app in the same state after an update.
    pub relaunch_args: Vec<String>,
    /// Interactive windows may come to the foreground on `focus`; hidden verification may not.
    pub allow_focus: bool,
    pub backup_root: PathBuf,
    /// Hidden verification runs print JSON-line events for the gate.
    pub emit_events: bool,
    /// Verification runs also append events here: an app restarted by an installer has no
    /// stdout the gate could read.
    pub event_log: Option<PathBuf>,
}

static OPTIONS: OnceLock<LifecycleOptions> = OnceLock::new();
static CONTROL_RECEIVER: Mutex<Option<mpsc::Receiver<ControlEnvelope>>> = Mutex::new(None);
pub(crate) static APPLY_REQUESTED: AtomicBool = AtomicBool::new(false);
pub(crate) static APPLY_CONFIRMED_BUSY: AtomicBool = AtomicBool::new(false);
/// Gate-only: simulate an app that never answers `quit-for-update` (`M3-A-04`).
pub(crate) static IGNORE_CONTROL: AtomicBool = AtomicBool::new(false);

pub(crate) fn install_options(options: LifecycleOptions) {
    let _ = OPTIONS.set(options);
}

pub(crate) fn options() -> Option<&'static LifecycleOptions> {
    OPTIONS.get()
}

pub(crate) fn install_control_receiver(receiver: mpsc::Receiver<ControlEnvelope>) {
    *CONTROL_RECEIVER.lock().unwrap() = Some(receiver);
}

pub(crate) fn next_control_request() -> Option<ControlEnvelope> {
    if IGNORE_CONTROL.load(Ordering::SeqCst) {
        return None;
    }
    CONTROL_RECEIVER
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|receiver| receiver.try_recv().ok())
}

/// Pipe-thread handler: forward to the UI loop and wait for its answer. An app that cannot
/// answer in time reports `busy`; it is never terminated.
pub(crate) fn bridge_handler(sender: mpsc::Sender<ControlEnvelope>) -> delta_app::ControlHandler {
    let sender = Mutex::new(sender);
    Arc::new(move |command| {
        let (reply_sender, reply_receiver) = mpsc::channel();
        if sender
            .lock()
            .unwrap()
            .send((command, reply_sender))
            .is_err()
        {
            return ControlReply::new(ControlStatus::Busy, "application is shutting down");
        }
        reply_receiver
            .recv_timeout(Duration::from_secs(60))
            .unwrap_or_else(|_| {
                ControlReply::new(
                    ControlStatus::Busy,
                    "application did not finish saving in time",
                )
            })
    })
}

pub(crate) fn emit_event(event: &str, detail: serde_json::Value) {
    let Some(options) = options().filter(|options| options.emit_events) else {
        return;
    };
    let line = serde_json::json!({
        "event": event,
        "at": chrono::Utc::now().to_rfc3339(),
        "process_id": std::process::id(),
        "detail": detail,
    })
    .to_string();
    use std::io::Write as _;
    let _ = writeln!(std::io::stdout(), "{line}");
    if let Some(path) = &options.event_log {
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(file, "{line}");
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum UpdateStatus {
    /// Online update is not available for this identity or not configured.
    Unavailable {
        reason: String,
    },
    Idle,
    Checking,
    UpToDate,
    Downloading {
        version: String,
    },
    Ready {
        version: String,
    },
    /// Work is running: the user must confirm before anything happens.
    BusyConfirm {
        version: String,
        detail: String,
    },
    Applying {
        version: String,
    },
    Failed {
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateUiState {
    pub status: UpdateStatus,
    pub current_version: String,
    pub source: String,
    pub auto_check: bool,
    pub last_check_at: Option<String>,
    #[serde(skip)]
    pub offer: Option<UpdateOffer>,
    #[serde(skip)]
    pub installer: Option<PathBuf>,
}

static UPDATE: Mutex<Option<UpdateUiState>> = Mutex::new(None);
static UPDATE_WORKER: AtomicBool = AtomicBool::new(false);

pub fn update_state() -> Option<UpdateUiState> {
    UPDATE.lock().unwrap().clone()
}

/// One-line Chinese status for the settings page.
pub(crate) fn describe_update(state: &UpdateUiState) -> String {
    let status = match &state.status {
        UpdateStatus::Unavailable { reason } => format!("此构建不提供在线更新：{reason}"),
        UpdateStatus::Idle => "尚未检查".into(),
        UpdateStatus::Checking => "正在检查…".into(),
        UpdateStatus::UpToDate => "已是最新版本".into(),
        UpdateStatus::Downloading { version } => format!("正在后台下载 {version}"),
        UpdateStatus::Ready { version } => {
            format!("{version} 已下载并验签，可安装（安装前自动备份）")
        }
        UpdateStatus::BusyConfirm { version, detail } => {
            format!("{detail}：{version} 暂不安装，等任务结束后再试，或明确选择停止任务并更新")
        }
        UpdateStatus::Applying { version } => format!("正在备份并启动 {version} 安装器"),
        UpdateStatus::Failed { code, message } => format!("更新未完成（{code}）：{message}"),
    };
    format!(
        "当前版本 {} · 来源 {} · 上次检查 {} · {status}",
        state.current_version,
        if state.source.is_empty() {
            "—"
        } else {
            &state.source
        },
        state.last_check_at.as_deref().unwrap_or("从未")
    )
}

pub(crate) fn set_update_status(status: UpdateStatus) {
    set_status(status)
}

pub(crate) fn set_update_failed(code: &str, message: &str) {
    set_status(UpdateStatus::Failed {
        code: code.into(),
        message: message.into(),
    })
}

fn set_status(status: UpdateStatus) {
    if let Some(state) = UPDATE.lock().unwrap().as_mut() {
        state.status = status.clone();
    }
    emit_event(
        "update-status",
        serde_json::to_value(&status).unwrap_or_default(),
    );
}

fn policy() -> Result<UpdatePolicy, delta_app::update::UpdateError> {
    let identity = InstallIdentity::current().ok_or_else(|| {
        delta_app::update::UpdateError::NotConfigured("source builds do not update online".into())
    })?;
    UpdatePolicy::for_identity(identity)
}

/// Initialize update state and run a due background check (at most every 24 h).
pub(crate) fn initialize_updates(data_dir: PathBuf, check_now: bool) {
    let preferences = UpdatePreferences::load(&data_dir);
    let (status, source) = match policy() {
        Ok(policy) => (UpdateStatus::Idle, policy.source_url.clone()),
        Err(error) => (
            UpdateStatus::Unavailable {
                reason: error.to_string(),
            },
            String::new(),
        ),
    };
    *UPDATE.lock().unwrap() = Some(UpdateUiState {
        status: status.clone(),
        current_version: delta_app::app_version().into(),
        source,
        auto_check: preferences.auto_check,
        last_check_at: preferences.last_check_at.clone(),
        offer: None,
        installer: None,
    });
    if status == UpdateStatus::Idle && (check_now || preferences.check_due(chrono::Utc::now())) {
        start_check(data_dir);
    }
}

pub(crate) fn set_auto_check(data_dir: PathBuf, enabled: bool) {
    let mut preferences = UpdatePreferences::load(&data_dir);
    preferences.auto_check = enabled;
    let _ = preferences.save(&data_dir);
    if let Some(state) = UPDATE.lock().unwrap().as_mut() {
        state.auto_check = enabled;
    }
}

/// Check and download on a background thread; the UI only reads the shared status.
pub(crate) fn start_check(data_dir: PathBuf) {
    if UPDATE_WORKER.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        delta_app::apply_process_quiet_guards();
        run_check(&data_dir);
        UPDATE_WORKER.store(false, Ordering::SeqCst);
    });
}

fn run_check(data_dir: &std::path::Path) {
    let fail = |error: delta_app::update::UpdateError| {
        set_status(UpdateStatus::Failed {
            code: error.code().into(),
            message: error.to_string(),
        })
    };
    let client = match policy().and_then(UpdateClient::new) {
        Ok(client) => client,
        Err(error) => return fail(error),
    };
    set_status(UpdateStatus::Checking);
    let mut preferences = UpdatePreferences::load(data_dir);
    preferences.last_check_at = Some(chrono::Utc::now().to_rfc3339());
    let _ = preferences.save(data_dir);
    if let Some(state) = UPDATE.lock().unwrap().as_mut() {
        state.last_check_at = preferences.last_check_at.clone();
    }
    let offer = match client.check(&delta_app::update::current_version()) {
        Ok(Some(offer)) => offer,
        Ok(None) => return set_status(UpdateStatus::UpToDate),
        Err(error) => return fail(error),
    };
    let version = offer.manifest.version.clone();
    set_status(UpdateStatus::Downloading {
        version: version.clone(),
    });
    let cancel = AtomicBool::new(false);
    let directory = data_dir.join("updates");
    // One automatic retry resumes an interrupted transfer with HTTP Range.
    let mut result = client.download(&offer, &directory, &cancel);
    if matches!(&result, Err(delta_app::update::UpdateError::Network(_))) {
        emit_event(
            "update-download-retry",
            serde_json::json!({"version": version}),
        );
        result = client.download(&offer, &directory, &cancel);
    }
    match result {
        Ok(installer) => {
            if let Some(state) = UPDATE.lock().unwrap().as_mut() {
                state.offer = Some(offer);
                state.installer = Some(installer);
            }
            set_status(UpdateStatus::Ready { version });
        }
        Err(error) => fail(error),
    }
}

/// Phases of the save-then-exit sequence. Every phase is re-evaluated on each UI tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitPhase {
    CheckBusy,
    SaveNote,
    SaveLayout,
    Finish,
}

pub(crate) enum ExitReason {
    Control(mpsc::Sender<ControlReply>),
    ApplyUpdate,
}

pub(crate) struct PendingExit {
    pub reason: ExitReason,
    pub phase: ExitPhase,
    pub started: Instant,
    pub dispatched: bool,
}

impl PendingExit {
    pub fn new(reason: ExitReason) -> Self {
        Self {
            reason,
            phase: ExitPhase::CheckBusy,
            started: Instant::now(),
            dispatched: false,
        }
    }

    pub fn timed_out(&self) -> bool {
        self.started.elapsed() > Duration::from_secs(45)
    }
}

/// Final step of an in-app update: consistent backup, re-verification, then the detached
/// silent installer. Any failure leaves the installed version untouched.
pub(crate) fn backup_and_launch_installer(
    host: &delta_app::AppHost,
) -> Result<(), delta_app::update::UpdateError> {
    use delta_app::update::UpdateError;
    let state = update_state().ok_or_else(|| UpdateError::NotConfigured("no update".into()))?;
    let (offer, installer) = state
        .offer
        .clone()
        .zip(state.installer.clone())
        .ok_or_else(|| UpdateError::NotConfigured("no downloaded update".into()))?;
    let policy = policy()?;
    delta_app::update::verify_installer(&policy, &offer, &installer)?;
    let options = options().ok_or_else(|| UpdateError::NotConfigured("lifecycle".into()))?;
    let backup = options.backup_root.join(format!(
        "pre-update-{}-{}",
        offer.manifest.version,
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
    ));
    let manifest = host
        .create_backup(backup.clone())
        .map_err(|e| UpdateError::BackupFailed(e.message))?;
    emit_event(
        "update-backup",
        serde_json::json!({"path": backup, "library_id": manifest.library_id}),
    );
    let log = installer.with_extension("install.log");
    let launch = delta_app::update::launch_installer(
        &policy,
        &offer,
        &installer,
        &log,
        &options.relaunch_args,
    )?;
    emit_event(
        "update-installer-started",
        serde_json::json!({"process_id": launch.process_id, "broke_away": launch.broke_away_from_job, "log": log}),
    );
    Ok(())
}
