//! Single instance and per-identity control pipe (`S-29`).
//!
//! The running desktop app holds the named mutex `<identity>-instance` (also the installer
//! `AppMutex`) and serves `\\.\pipe\<identity>-control`. Only clients in the same Windows
//! session running as the same user are served. The protocol has exactly two commands:
//! `focus` and `quit-for-update`; the latter is answered only after pending note saves and
//! layout persistence finish, or with `busy` while sync/import/AI work is running.

use crate::identity::InstallIdentity;
use serde::{Deserialize, Serialize};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ControlCommand {
    Focus,
    QuitForUpdate,
}

impl ControlCommand {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "focus" => Some(Self::Focus),
            "quit-for-update" => Some(Self::QuitForUpdate),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControlRequest {
    pub identity: String,
    pub command: ControlCommand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ControlStatus {
    /// `focus` handled, or `quit-for-update` saved everything and the app is exiting.
    Ok,
    /// Sync, import or AI work is running; the user decides. The app keeps running.
    Busy,
    /// A pending save failed; the app keeps running so no edit is lost.
    SaveFailed,
    /// Wrong identity, malformed request or a client outside this user session.
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControlReply {
    pub status: ControlStatus,
    pub reason: String,
    pub identity: String,
    pub version: String,
    /// Arguments that reopen the app in the same state (data root, hidden verification mode).
    pub relaunch_args: Vec<String>,
}

impl ControlReply {
    pub fn new(status: ControlStatus, reason: impl Into<String>) -> Self {
        Self {
            status,
            reason: reason.into(),
            identity: InstallIdentity::current()
                .map(|identity| identity.name().to_string())
                .unwrap_or_else(|| "source".into()),
            version: crate::identity::app_version().into(),
            relaunch_args: Vec::new(),
        }
    }
}

#[derive(Debug, Error)]
pub enum InstanceError {
    #[error("{0} is already running")]
    AlreadyRunning(String),
    #[error("{0} is not running")]
    NotRunning(String),
    #[error("control pipe timed out after {0:?}")]
    Timeout(Duration),
    #[error("control pipe error: {0}")]
    Io(String),
    #[error("single instance and control pipe require Windows")]
    Unsupported,
}

impl InstanceError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::AlreadyRunning(_) => "INSTANCE_RUNNING",
            Self::NotRunning(_) => "INSTANCE_NOT_RUNNING",
            Self::Timeout(_) => "CONTROL_TIMEOUT",
            Self::Io(_) => "CONTROL_IO",
            Self::Unsupported => "CONTROL_UNSUPPORTED",
        }
    }
}

#[cfg(windows)]
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn last_error() -> String {
    std::io::Error::last_os_error().to_string()
}

/// Named mutex owned for the lifetime of the running desktop app.
pub struct SingleInstance {
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
}

// SAFETY: the mutex handle is only closed on drop; the kernel object is thread-agnostic.
unsafe impl Send for SingleInstance {}

impl SingleInstance {
    pub fn acquire_name(mutex_name: &str) -> Result<Self, InstanceError> {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
            use windows_sys::Win32::System::Threading::CreateMutexW;
            let name = wide(mutex_name);
            let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
            if handle.is_null() {
                return Err(InstanceError::Io(last_error()));
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(handle);
                return Err(InstanceError::AlreadyRunning(mutex_name.into()));
            }
            Ok(Self { handle })
        }
        #[cfg(not(windows))]
        {
            let _ = mutex_name;
            Err(InstanceError::Unsupported)
        }
    }

    pub fn acquire(identity: InstallIdentity) -> Result<Self, InstanceError> {
        Self::acquire_name(&identity.mutex_name())
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

/// True while any process in this session holds the identity mutex.
pub fn mutex_exists(mutex_name: &str) -> bool {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};
        let name = wide(mutex_name);
        let handle = OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, name.as_ptr());
        if handle.is_null() {
            return false;
        }
        CloseHandle(handle);
        true
    }
    #[cfg(not(windows))]
    {
        let _ = mutex_name;
        false
    }
}

pub fn identity_is_running(identity: InstallIdentity) -> bool {
    mutex_exists(&identity.mutex_name())
}

/// Wait until the identity mutex disappears; never terminates the process.
pub fn wait_for_exit(mutex_name: &str, timeout: Duration) -> bool {
    let started = std::time::Instant::now();
    while started.elapsed() < timeout {
        if !mutex_exists(mutex_name) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    !mutex_exists(mutex_name)
}

pub type ControlHandler = Arc<dyn Fn(ControlCommand) -> ControlReply + Send + Sync>;

/// Background pipe server. Dropping it stops accepting connections.
pub struct ControlServer {
    stop: Arc<AtomicBool>,
    pipe_name: String,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ControlServer {
    pub fn start(
        identity: InstallIdentity,
        handler: ControlHandler,
    ) -> Result<Self, InstanceError> {
        Self::start_named(identity.pipe_name(), identity.name().into(), handler)
    }

    /// Serve `pipe_name`, accepting only requests that name `identity`.
    pub fn start_named(
        pipe_name: String,
        identity: String,
        handler: ControlHandler,
    ) -> Result<Self, InstanceError> {
        #[cfg(windows)]
        {
            let stop = Arc::new(AtomicBool::new(false));
            // Create the first instance synchronously so a squatter or a duplicate server
            // makes startup fail instead of silently serving nobody.
            let first = windows_pipe::create_instance(&pipe_name)?;
            let thread_stop = Arc::clone(&stop);
            let thread = std::thread::Builder::new()
                .name("delta-control-pipe".into())
                .spawn(move || windows_pipe::serve(first, &identity, &handler, &thread_stop))
                .map_err(|e| InstanceError::Io(e.to_string()))?;
            Ok(Self {
                stop,
                pipe_name,
                thread: Some(thread),
            })
        }
        #[cfg(not(windows))]
        {
            let _ = (pipe_name, identity, handler);
            Err(InstanceError::Unsupported)
        }
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            // Wake the blocking ConnectNamedPipe with throwaway connections. A single attempt
            // can miss while the one instance is between Disconnect and Connect, so retry until
            // the thread ends; never block the caller's exit indefinitely.
            let started = std::time::Instant::now();
            while !thread.is_finished() && started.elapsed() < Duration::from_secs(5) {
                #[cfg(windows)]
                windows_pipe::poke(&self.pipe_name);
                std::thread::sleep(Duration::from_millis(20));
            }
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
    }
}

/// Send one command to the running identity and wait for its reply.
pub fn send_control_command(
    identity: InstallIdentity,
    command: ControlCommand,
    timeout: Duration,
) -> Result<ControlReply, InstanceError> {
    send_control_request(
        &identity.pipe_name(),
        &ControlRequest {
            identity: identity.name().into(),
            command,
        },
        timeout,
    )
}

pub fn send_control_request(
    pipe_name: &str,
    request: &ControlRequest,
    timeout: Duration,
) -> Result<ControlReply, InstanceError> {
    #[cfg(windows)]
    {
        let bytes = {
            let payload =
                serde_json::to_vec(request).map_err(|e| InstanceError::Io(e.to_string()))?;
            let name = pipe_name.to_string();
            let (sender, receiver) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = sender.send(windows_pipe::exchange(&name, &payload, timeout));
            });
            receiver
                .recv_timeout(timeout + Duration::from_millis(250))
                .map_err(|_| InstanceError::Timeout(timeout))??
        };
        serde_json::from_slice(&bytes).map_err(|e| InstanceError::Io(e.to_string()))
    }
    #[cfg(not(windows))]
    {
        let _ = (pipe_name, request, timeout);
        Err(InstanceError::Unsupported)
    }
}

#[cfg(windows)]
mod windows_pipe {
    use super::*;
    use std::time::Instant;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER,
        ERROR_MORE_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE,
        HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FlushFileBuffers, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE,
        OPEN_EXISTING, PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
        SetNamedPipeHandleState, WaitNamedPipeW, PIPE_READMODE_MESSAGE, PIPE_REJECT_REMOTE_CLIENTS,
        PIPE_TYPE_MESSAGE, PIPE_WAIT,
    };

    const BUFFER: u32 = 8192;
    const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

    pub(super) struct PipeHandle(pub HANDLE);
    unsafe impl Send for PipeHandle {}
    impl Drop for PipeHandle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    pub(super) fn create_instance(name: &str) -> Result<PipeHandle, InstanceError> {
        let wide = wide(name);
        let handle = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                BUFFER,
                BUFFER,
                0,
                std::ptr::null(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(InstanceError::Io(format!(
                "cannot create control pipe: {}",
                last_error()
            )));
        }
        Ok(PipeHandle(handle))
    }

    fn session_of(pid: u32) -> Option<u32> {
        use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
        let mut session = 0;
        (unsafe { ProcessIdToSessionId(pid, &mut session) } != 0).then_some(session)
    }

    fn user_sid_of(pid: u32) -> Option<Vec<u8>> {
        use windows_sys::Win32::Security::{
            GetLengthSid, GetTokenInformation, IsValidSid, TokenUser, TOKEN_QUERY, TOKEN_USER,
        };
        use windows_sys::Win32::System::Threading::{
            OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return None;
            }
            let mut token: HANDLE = std::ptr::null_mut();
            let opened = OpenProcessToken(process, TOKEN_QUERY, &mut token);
            CloseHandle(process);
            if opened == 0 || token.is_null() {
                return None;
            }
            // Allocate `usize`-aligned words (`align_of::<usize>() >= align_of::<TOKEN_USER>()`)
            // and grow on `ERROR_INSUFFICIENT_BUFFER` (`XCR-U01`).
            let word_size = std::mem::size_of::<usize>();
            let mut words = vec![0usize; 32];
            let mut needed = 0u32;
            let mut ok = GetTokenInformation(
                token,
                TokenUser,
                words.as_mut_ptr().cast(),
                (words.len() * word_size) as u32,
                &mut needed,
            );
            if ok == 0 && GetLastError() == ERROR_INSUFFICIENT_BUFFER && needed > 0 {
                let required_words = (needed as usize).div_ceil(word_size);
                words.resize(required_words, 0usize);
                ok = GetTokenInformation(
                    token,
                    TokenUser,
                    words.as_mut_ptr().cast(),
                    (words.len() * word_size) as u32,
                    &mut needed,
                );
            }
            CloseHandle(token);
            if ok == 0 || (needed as usize) < std::mem::size_of::<TOKEN_USER>() {
                return None;
            }
            let user = std::ptr::read_unaligned(words.as_ptr() as *const TOKEN_USER);
            if user.User.Sid.is_null() || IsValidSid(user.User.Sid) == 0 {
                return None;
            }
            let length = GetLengthSid(user.User.Sid) as usize;
            if length == 0 {
                return None;
            }
            Some(std::slice::from_raw_parts(user.User.Sid as *const u8, length).to_vec())
        }
    }

    fn client_is_same_user_session(pipe: HANDLE) -> bool {
        let mut client = 0u32;
        if unsafe { GetNamedPipeClientProcessId(pipe, &mut client) } == 0 {
            return false;
        }
        let me = std::process::id();
        session_of(client).is_some() && session_of(client) == session_of(me) && {
            let mine = user_sid_of(me);
            mine.is_some() && user_sid_of(client) == mine
        }
    }

    fn read_message(pipe: HANDLE) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        let mut chunk = vec![0u8; BUFFER as usize];
        loop {
            let mut read = 0u32;
            let ok = unsafe {
                ReadFile(
                    pipe,
                    chunk.as_mut_ptr(),
                    BUFFER,
                    &mut read,
                    std::ptr::null_mut(),
                )
            };
            let more = ok == 0 && unsafe { GetLastError() } == ERROR_MORE_DATA;
            if ok == 0 && !more {
                return None;
            }
            let read_len = read as usize;
            if out.len().saturating_add(read_len) > MAX_MESSAGE_BYTES {
                return None;
            }
            out.extend_from_slice(&chunk[..read_len]);
            if !more {
                return Some(out);
            }
        }
    }

    fn write_message(pipe: HANDLE, bytes: &[u8]) -> bool {
        let Ok(len) = u32::try_from(bytes.len()) else {
            return false;
        };
        let mut written = 0u32;
        let ok = unsafe {
            WriteFile(
                pipe,
                bytes.as_ptr(),
                len,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        ok != 0 && written == len && unsafe { FlushFileBuffers(pipe) } != 0
    }

    fn rejected_reply(identity: &str, reason: impl Into<String>) -> ControlReply {
        let mut reply = ControlReply::new(ControlStatus::Rejected, reason);
        reply.identity = identity.to_string();
        reply
    }

    /// Serve one pipe instance for the whole app lifetime: reusing the handle avoids a window
    /// in which another process could create the pipe name between connections.
    pub(super) fn serve(
        pipe: PipeHandle,
        identity: &str,
        handler: &ControlHandler,
        stop: &AtomicBool,
    ) {
        loop {
            let connected = unsafe { ConnectNamedPipe(pipe.0, std::ptr::null_mut()) } != 0
                || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
            if stop.load(Ordering::SeqCst) {
                return;
            }
            if !connected {
                // A client that closed before we connected leaves the instance unusable
                // until it is disconnected.
                unsafe {
                    DisconnectNamedPipe(pipe.0);
                }
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            let reply = if !client_is_same_user_session(pipe.0) {
                rejected_reply(identity, "client is outside this user session")
            } else {
                match read_message(pipe.0)
                    .and_then(|bytes| serde_json::from_slice::<ControlRequest>(&bytes).ok())
                {
                    None => rejected_reply(identity, "malformed control request"),
                    Some(request) if request.identity != identity => rejected_reply(
                        identity,
                        format!(
                            "request for {} sent to the {identity} control pipe",
                            request.identity
                        ),
                    ),
                    Some(request) => handler(request.command),
                }
            };
            if let Ok(bytes) = serde_json::to_vec(&reply) {
                let _ = write_message(pipe.0, &bytes);
            }
            unsafe {
                DisconnectNamedPipe(pipe.0);
            }
        }
    }

    fn open_client(name: &str, timeout: Duration) -> Result<PipeHandle, InstanceError> {
        let wide = wide(name);
        let deadline = Instant::now() + timeout;
        loop {
            let handle = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    std::ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                let mode = PIPE_READMODE_MESSAGE;
                unsafe {
                    SetNamedPipeHandleState(handle, &mode, std::ptr::null(), std::ptr::null());
                }
                return Ok(PipeHandle(handle));
            }
            let code = unsafe { GetLastError() };
            if code == ERROR_FILE_NOT_FOUND {
                return Err(InstanceError::NotRunning(name.into()));
            }
            if code != ERROR_PIPE_BUSY {
                return Err(InstanceError::Io(format!("{name}: {}", last_error())));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(InstanceError::Timeout(timeout));
            }
            let wait_ms = (remaining.as_millis() as u32).clamp(1, 500);
            if unsafe { WaitNamedPipeW(wide.as_ptr(), wait_ms) } == 0
                && unsafe { GetLastError() } == ERROR_FILE_NOT_FOUND
            {
                return Err(InstanceError::NotRunning(name.into()));
            }
        }
    }

    pub(super) fn exchange(
        name: &str,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>, InstanceError> {
        let pipe = open_client(name, timeout)?;
        if !write_message(pipe.0, payload) {
            return Err(InstanceError::Io(last_error()));
        }
        read_message(pipe.0).ok_or_else(|| InstanceError::Io(last_error()))
    }

    pub(super) fn poke(name: &str) {
        let wide = wide(name);
        unsafe {
            WaitNamedPipeW(wide.as_ptr(), 50);
        }
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                std::ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            drop(PipeHandle(handle));
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn m3_a02_single_instance_and_control_pipe_reject_cross_identity_requests() {
        let unique = uuid::Uuid::new_v4().simple().to_string();
        let mutex = format!("delta-unit-{unique}-instance");
        let first = SingleInstance::acquire_name(&mutex).unwrap();
        assert!(mutex_exists(&mutex));
        assert!(matches!(
            SingleInstance::acquire_name(&mutex),
            Err(InstanceError::AlreadyRunning(_))
        ));
        drop(first);
        assert!(!mutex_exists(&mutex));
        assert!(wait_for_exit(&mutex, Duration::from_millis(10)));

        let pipe = format!(r"\\.\pipe\delta-unit-{unique}-control");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&seen);
        let busy = Arc::new(AtomicBool::new(true));
        let busy_flag = Arc::clone(&busy);
        let handler: ControlHandler = Arc::new(move |command| {
            observed.lock().unwrap().push(command);
            if command == ControlCommand::QuitForUpdate && busy_flag.load(Ordering::SeqCst) {
                ControlReply::new(ControlStatus::Busy, "sync running")
            } else {
                ControlReply::new(ControlStatus::Ok, "")
            }
        });
        let server =
            ControlServer::start_named(pipe.clone(), "delta-test".into(), handler).unwrap();
        // A second server for the same pipe cannot squat or duplicate it.
        assert!(ControlServer::start_named(
            pipe.clone(),
            "delta-test".into(),
            Arc::new(|_| ControlReply::new(ControlStatus::Ok, ""))
        )
        .is_err());
        let request = |identity: &str, command| {
            send_control_request(
                &pipe,
                &ControlRequest {
                    identity: identity.into(),
                    command,
                },
                Duration::from_secs(5),
            )
            .unwrap()
        };
        assert_eq!(
            request("delta-test", ControlCommand::Focus).status,
            ControlStatus::Ok
        );
        assert_eq!(
            request("delta-test", ControlCommand::QuitForUpdate).status,
            ControlStatus::Busy
        );
        let rejected = request("delta-dev-test", ControlCommand::QuitForUpdate);
        assert_eq!(rejected.status, ControlStatus::Rejected);
        assert_eq!(rejected.identity, "delta-test");
        busy.store(false, Ordering::SeqCst);
        assert_eq!(
            request("delta-test", ControlCommand::QuitForUpdate).status,
            ControlStatus::Ok
        );
        // The rejected cross-identity command never reached the handler.
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                ControlCommand::Focus,
                ControlCommand::QuitForUpdate,
                ControlCommand::QuitForUpdate
            ]
        );
        drop(server);
        assert!(matches!(
            send_control_request(
                &pipe,
                &ControlRequest {
                    identity: "delta-test".into(),
                    command: ControlCommand::Focus
                },
                Duration::from_secs(8)
            ),
            Err(InstanceError::NotRunning(_))
        ));
    }

    #[test]
    fn control_pipe_handles_multi_chunk_replies_and_nonexistent_not_running() {
        let unique = uuid::Uuid::new_v4().simple().to_string();
        let pipe = format!(r"\\.\pipe\delta-unit-{unique}-large");
        let large_detail = "x".repeat(16_384);
        let detail_clone = large_detail.clone();
        let handler: ControlHandler = Arc::new(move |_| {
            let mut reply = ControlReply::new(ControlStatus::Ok, detail_clone.clone());
            reply.identity = "delta-test".into();
            reply
        });
        let server =
            ControlServer::start_named(pipe.clone(), "delta-test".into(), handler).unwrap();
        let reply = send_control_request(
            &pipe,
            &ControlRequest {
                identity: "delta-test".into(),
                command: ControlCommand::Focus,
            },
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(reply.status, ControlStatus::Ok);
        assert_eq!(reply.reason.len(), large_detail.len());
        assert_eq!(reply.reason, large_detail);
        drop(server);

        let missing = format!(r"\\.\pipe\delta-unit-{unique}-missing");
        assert!(matches!(
            send_control_request(
                &missing,
                &ControlRequest {
                    identity: "delta-test".into(),
                    command: ControlCommand::Focus,
                },
                Duration::from_millis(50),
            ),
            Err(InstanceError::NotRunning(_))
        ));
    }
}
