//! Windows system tray icon (`Shell_NotifyIconW`), close-to-tray policy, and background/update
//! window-state restoration (`D-36`, `D-37`).
//!
//! Pure-Rust Win32 implementation using the workspace's existing `windows-sys` dependency (zero
//! extra crates, zero sensitive PE imports). All interactive tray work (message sink, native
//! right-click popup menu, `TaskbarCreated` re-registration) runs on a dedicated lightweight
//! thread so opening the tray menu never blocks GPUI's UI loop. Automated verification modes
//! (`--ui-smoke`, `--ui-bench`, `--verification-resident`, headless tests) never create a tray
//! icon (`D-25`).

use std::sync::{
    atomic::{AtomicBool, AtomicIsize, Ordering},
    mpsc, Mutex,
};

use serde::{Deserialize, Serialize};

use crate::lock_or_recover;

/// Action performed when the user clicks the main window close button (`X` / `Alt+F4`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseAction {
    /// Hide the main window to the system tray and keep DELTA running in the background (default).
    #[default]
    MinimizeToTray,
    /// Save pending state and exit DELTA cleanly.
    Quit,
}

/// Resolution of a window close request (`on_window_should_close`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseDecision {
    /// Cancel window destruction and hide the main window into the tray.
    HideToTray,
    /// Start DELTA's graceful save-then-exit sequence (cancels immediate destruction until saved).
    GracefulQuit,
    /// Exit is already authorized (or forced by OS shutdown / repeated close); allow close now.
    AllowImmediateClose,
}

/// Initial main-window visibility when launching interactive desktop mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundStartupMode {
    /// Normal interactive launch: show and focus the main window.
    NormalWindow,
    /// `--background` with an active tray icon: tray icon only, no window shown (`D-36`).
    TrayOnly,
    /// `--background` fallback when the OS shell refused tray icon creation: minimized on taskbar,
    /// never stealing focus, so the user is never stranded without a way to open the window.
    MinimizedTaskbar,
}

/// Commands emitted from the tray thread to GPUI's foreground lifecycle tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    ShowWindow,
    QuitApp,
}

/// Decide how the main window handles a close request (`WM_CLOSE`).
///
/// Explicit quits (`force_exit`), update quits, and OS shutdown always close immediately. When no
/// tray icon is active, closing quits instead of hiding the window with no way back.
pub fn decide_window_close(
    close_action: CloseAction,
    tray_active: bool,
    force_exit: bool,
) -> CloseDecision {
    if force_exit {
        return CloseDecision::AllowImmediateClose;
    }
    match (close_action, tray_active) {
        (CloseAction::MinimizeToTray, true) => CloseDecision::HideToTray,
        _ => CloseDecision::GracefulQuit,
    }
}

/// Decide how interactive startup shows the main window for `--background` (`D-36`).
pub fn decide_startup_mode(background: bool, tray_active: bool) -> BackgroundStartupMode {
    if !background {
        BackgroundStartupMode::NormalWindow
    } else if tray_active {
        BackgroundStartupMode::TrayOnly
    } else {
        BackgroundStartupMode::MinimizedTaskbar
    }
}

/// Compute relaunch arguments for an in-app update (`D-37`):
/// - Verification residents (`preserve_raw`) keep their exact arguments unchanged (`M3-A-08`).
/// - Interactive releases return to their previous state: if the main window was open, strip
///   `--background` so the window opens again; if it was hidden in the tray, pass `--background`
///   so it stays in the tray after restarting.
pub fn relaunch_args_for_update(
    base_args: &[String],
    window_was_open: bool,
    preserve_raw: bool,
) -> Vec<String> {
    if preserve_raw {
        return base_args.to_vec();
    }
    let mut args: Vec<String> = base_args
        .iter()
        .filter(|arg| arg.as_str() != "--background")
        .cloned()
        .collect();
    if !window_was_open {
        args.push("--background".to_string());
    }
    args
}

/// Format the notification-area tooltip, safely bounded to Win32 `NOTIFYICONDATAW::szTip` (127
/// UTF-16 code units plus trailing NUL).
pub fn tray_tooltip_text(display_name: &str, active_work: Option<&str>) -> String {
    let raw = match active_work.map(str::trim).filter(|s| !s.is_empty()) {
        Some(work) => format!("{display_name} · {work}"),
        None => display_name.to_string(),
    };
    truncate_utf16_units(&raw, 127)
}

/// Menu labels for the native tray popup menu: open main window (default) and quit app.
pub fn tray_menu_labels(display_name: &str) -> (String, String) {
    (
        format!("打开 {display_name}"),
        format!("退出 {display_name}"),
    )
}

fn truncate_utf16_units(text: &str, max_units: usize) -> String {
    let mut out = String::new();
    let mut units = 0usize;
    for ch in text.chars() {
        let needed = ch.len_utf16();
        if units + needed > max_units {
            break;
        }
        out.push(ch);
        units += needed;
    }
    out
}

// ============================================================================
// Process-wide interactive tray state (never populated during verification runs)
// ============================================================================

static TRAY_ACTIVE: AtomicBool = AtomicBool::new(false);
static HIDDEN_IN_TRAY: AtomicBool = AtomicBool::new(false);
static EVER_SHOWN: AtomicBool = AtomicBool::new(false);
static FORCE_EXIT: AtomicBool = AtomicBool::new(false);
static USER_QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);
static MAIN_WINDOW_HWND: AtomicIsize = AtomicIsize::new(0);
static TRAY_WINDOW_HWND: AtomicIsize = AtomicIsize::new(0);

static TRAY_RECEIVER: Mutex<Option<mpsc::Receiver<TrayCommand>>> = Mutex::new(None);
static TRAY_TOOLTIP: Mutex<String> = Mutex::new(String::new());
static TRAY_DISPLAY_NAME: Mutex<String> = Mutex::new(String::new());
static TRAY_THREAD: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

/// RAII guard that removes the tray icon and joins the tray thread when `run_desktop_entrypoint`
/// exits.
pub(crate) struct InteractiveTrayGuard;

impl Drop for InteractiveTrayGuard {
    fn drop(&mut self) {
        dispose_active_tray();
    }
}

pub(crate) fn is_active() -> bool {
    TRAY_ACTIVE.load(Ordering::SeqCst)
}

pub(crate) fn is_hidden_in_tray() -> bool {
    HIDDEN_IN_TRAY.load(Ordering::SeqCst)
}

pub(crate) fn is_force_exiting() -> bool {
    FORCE_EXIT.load(Ordering::SeqCst)
}

pub(crate) fn mark_force_exit() {
    FORCE_EXIT.store(true, Ordering::SeqCst);
}

pub(crate) fn request_user_quit() {
    USER_QUIT_REQUESTED.store(true, Ordering::SeqCst);
}

pub(crate) fn take_user_quit_requested() -> bool {
    USER_QUIT_REQUESTED.swap(false, Ordering::SeqCst)
}

pub(crate) fn next_tray_command() -> Option<TrayCommand> {
    lock_or_recover(&TRAY_RECEIVER)
        .as_ref()
        .and_then(|rx| rx.try_recv().ok())
}

/// Update the tray icon tooltip if the text changed since the last update.
pub(crate) fn sync_tooltip(active_work: Option<&str>) {
    if !is_active() {
        return;
    }
    let display_name = lock_or_recover(&TRAY_DISPLAY_NAME).clone();
    if display_name.is_empty() {
        return;
    }
    let next = tray_tooltip_text(&display_name, active_work);
    let mut current = lock_or_recover(&TRAY_TOOLTIP);
    if *current == next {
        return;
    }
    *current = next;
    drop(current);
    #[cfg(windows)]
    win32::post_update_tip();
}

/// Start the Windows notification-area tray icon for an interactive desktop session.
pub(crate) fn start_interactive_tray(display_name: &str) -> InteractiveTrayGuard {
    FORCE_EXIT.store(false, Ordering::SeqCst);
    USER_QUIT_REQUESTED.store(false, Ordering::SeqCst);
    HIDDEN_IN_TRAY.store(false, Ordering::SeqCst);
    EVER_SHOWN.store(false, Ordering::SeqCst);
    MAIN_WINDOW_HWND.store(0, Ordering::SeqCst);
    *lock_or_recover(&TRAY_DISPLAY_NAME) = display_name.to_string();
    *lock_or_recover(&TRAY_TOOLTIP) = tray_tooltip_text(display_name, None);

    let (cmd_tx, cmd_rx) = mpsc::channel();
    *lock_or_recover(&TRAY_RECEIVER) = Some(cmd_rx);

    #[cfg(windows)]
    {
        let (ready_tx, ready_rx) = mpsc::sync_channel::<(bool, isize)>(1);
        let name = display_name.to_string();
        if let Ok(handle) = std::thread::Builder::new()
            .name("delta-tray".into())
            .spawn(move || win32::run_tray_thread(name, cmd_tx, ready_tx))
        {
            *lock_or_recover(&TRAY_THREAD) = Some(handle);
            if let Ok((active, hwnd)) = ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
                TRAY_WINDOW_HWND.store(hwnd, Ordering::SeqCst);
                TRAY_ACTIVE.store(active, Ordering::SeqCst);
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = cmd_tx;
        TRAY_ACTIVE.store(false, Ordering::SeqCst);
    }

    InteractiveTrayGuard
}

/// Record the GPUI main window HWND and initial visibility right after `cx.open_window`.
pub(crate) fn register_main_window(initially_shown: bool) {
    EVER_SHOWN.store(initially_shown, Ordering::SeqCst);
    HIDDEN_IN_TRAY.store(!initially_shown, Ordering::SeqCst);
    #[cfg(windows)]
    {
        let hwnd = win32::find_main_gpui_hwnd(TRAY_WINDOW_HWND.load(Ordering::SeqCst));
        if hwnd != 0 {
            MAIN_WINDOW_HWND.store(hwnd, Ordering::SeqCst);
        }
    }
}

/// Hide the main GPUI window into the system tray (`SW_HIDE`, removes taskbar entry).
pub(crate) fn hide_to_tray() -> bool {
    if !is_active() {
        return false;
    }
    #[cfg(windows)]
    {
        let hwnd = resolve_main_hwnd();
        if hwnd == 0 {
            return false;
        }
        win32::hide_window(hwnd);
        HIDDEN_IN_TRAY.store(true, Ordering::SeqCst);
        true
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Show (if hidden in tray or minimized on taskbar), activate, and refresh the main GPUI window.
pub(crate) fn restore_main_window(window: &mut gpui_kit::Window) {
    HIDDEN_IN_TRAY.store(false, Ordering::SeqCst);
    let was_ever_shown = EVER_SHOWN.swap(true, Ordering::SeqCst);
    #[cfg(windows)]
    if was_ever_shown {
        let hwnd = resolve_main_hwnd();
        if hwnd != 0 {
            win32::show_or_restore_window(hwnd);
        }
    }
    #[cfg(not(windows))]
    let _ = was_ever_shown;
    // When `!was_ever_shown` (first reveal from `--background` `show: false`), GPUI's
    // `activate_window()` applies `initial_placement` (centered 1440x900 + `SW_SHOWNORMAL`)
    // and activates in one step without flashing an unpositioned window.
    window.activate_window();
    window.refresh();
}

/// Remove the notification-area icon and stop the tray message thread. Idempotent.
pub(crate) fn dispose_active_tray() {
    TRAY_ACTIVE.store(false, Ordering::SeqCst);
    #[cfg(windows)]
    win32::post_shutdown();
    if let Some(handle) = lock_or_recover(&TRAY_THREAD).take() {
        // Avoid self-join if somehow dropped on the tray thread.
        if handle.thread().id() != std::thread::current().id() {
            let _ = handle.join();
        }
    }
    TRAY_WINDOW_HWND.store(0, Ordering::SeqCst);
    *lock_or_recover(&TRAY_RECEIVER) = None;
}

#[cfg(windows)]
fn resolve_main_hwnd() -> isize {
    let cached = MAIN_WINDOW_HWND.load(Ordering::SeqCst);
    if cached != 0 && win32::is_window(cached) {
        return cached;
    }
    let found = win32::find_main_gpui_hwnd(TRAY_WINDOW_HWND.load(Ordering::SeqCst));
    if found != 0 {
        MAIN_WINDOW_HWND.store(found, Ordering::SeqCst);
    }
    found
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowClassRole {
    MainWindow,
    Ignore,
    FallbackCandidate,
}

fn classify_window_class(name: &str) -> WindowClassRole {
    if matches!(name, "GpuiFast::Window" | "Zed::Window") {
        WindowClassRole::MainWindow
    } else if matches!(name, "GpuiFast::PlatformWindow" | "Zed::PlatformWindow")
        || name.starts_with("DeltaTraySink_")
    {
        WindowClassRole::Ignore
    } else {
        WindowClassRole::FallbackCandidate
    }
}

#[cfg(windows)]
mod win32 {
    use std::sync::{
        atomic::Ordering,
        mpsc::{Sender, SyncSender},
    };

    use windows_sys::Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentProcessId},
        UI::{
            Shell::{
                Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
                NOTIFYICONDATAW,
            },
            WindowsAndMessaging::{
                AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
                DestroyWindow, DispatchMessageW, EnumWindows, GetClassNameW, GetClientRect,
                GetCursorPos, GetMessageW, GetSystemMetrics, GetWindowLongPtrW,
                GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, LoadImageW,
                PostMessageW, PostQuitMessage, RegisterClassExW, RegisterWindowMessageW,
                SetForegroundWindow, SetMenuDefaultItem, SetWindowLongPtrW, ShowWindow,
                TrackPopupMenuEx, TranslateMessage, UnregisterClassW, GWLP_USERDATA, HICON,
                IMAGE_ICON, LR_DEFAULTSIZE, LR_SHARED, MF_SEPARATOR, MF_STRING, MSG, SM_CXSMICON,
                SM_CYSMICON, SW_HIDE, SW_RESTORE, SW_SHOW, TPM_BOTTOMALIGN, TPM_RETURNCMD,
                TPM_RIGHTBUTTON, WM_APP, WM_CONTEXTMENU, WM_DESTROY, WM_ENDSESSION,
                WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NCDESTROY, WM_NULL, WM_RBUTTONUP, WNDCLASSEXW,
                WS_EX_TOOLWINDOW,
            },
        },
    };

    use super::{
        tray_menu_labels, TrayCommand, FORCE_EXIT, TRAY_ACTIVE, TRAY_TOOLTIP, TRAY_WINDOW_HWND,
    };
    use crate::lock_or_recover;

    const WM_APP_TRAY: u32 = WM_APP + 0x42;
    const WM_APP_UPDATE_TIP: u32 = WM_APP + 0x43;
    const WM_APP_SHUTDOWN: u32 = WM_APP + 0x44;
    const TRAY_ICON_ID: u32 = 1;
    const CMD_OPEN_WINDOW: usize = 1001;
    const CMD_QUIT_APP: usize = 1002;

    struct TraySinkState {
        display_name: String,
        hicon: HICON,
        taskbar_created_msg: u32,
        icon_added: bool,
        cmd_tx: Sender<TrayCommand>,
    }

    fn wide_nul(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn load_embedded_tray_icon() -> HICON {
        // `MAKEINTRESOURCEW(1)`: `winresource` embeds `icon.ico` / `icon-dev.ico` as icon ID 1.
        let resource_id: *const u16 = std::ptr::without_provenance(1);
        unsafe {
            let module = GetModuleHandleW(std::ptr::null());
            let cx = GetSystemMetrics(SM_CXSMICON);
            let cy = GetSystemMetrics(SM_CYSMICON);
            let icon = LoadImageW(module, resource_id, IMAGE_ICON, cx, cy, LR_SHARED);
            if !icon.is_null() {
                return icon as HICON;
            }
            LoadImageW(
                module,
                resource_id,
                IMAGE_ICON,
                0,
                0,
                LR_DEFAULTSIZE | LR_SHARED,
            ) as HICON
        }
    }

    fn build_notify_data(hwnd: HWND, hicon: HICON, tooltip: &str) -> NOTIFYICONDATAW {
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = TRAY_ICON_ID;
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.uCallbackMessage = WM_APP_TRAY;
        data.hIcon = hicon;
        let units: Vec<u16> = tooltip.encode_utf16().take(127).collect();
        for (slot, unit) in data.szTip.iter_mut().zip(units.iter()) {
            *slot = *unit;
        }
        data.szTip[units.len()] = 0;
        data
    }

    fn remove_icon(hwnd: HWND, state: &mut TraySinkState) {
        if state.icon_added {
            let data = build_notify_data(hwnd, state.hicon, "");
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &data);
            }
            state.icon_added = false;
            TRAY_ACTIVE.store(false, Ordering::SeqCst);
        }
    }

    fn show_context_menu(hwnd: HWND, state: &TraySinkState) {
        unsafe {
            let mut pt = POINT { x: 0, y: 0 };
            if GetCursorPos(&mut pt) == 0 {
                return;
            }
            let hmenu = CreatePopupMenu();
            if hmenu.is_null() {
                return;
            }
            let (open_label, quit_label) = tray_menu_labels(&state.display_name);
            let open_w = wide_nul(&open_label);
            let quit_w = wide_nul(&quit_label);
            let _ = AppendMenuW(hmenu, MF_STRING, CMD_OPEN_WINDOW, open_w.as_ptr());
            let _ = SetMenuDefaultItem(hmenu, CMD_OPEN_WINDOW as u32, 0);
            let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, std::ptr::null());
            let _ = AppendMenuW(hmenu, MF_STRING, CMD_QUIT_APP, quit_w.as_ptr());

            let _ = SetForegroundWindow(hwnd);
            let chosen = TrackPopupMenuEx(
                hmenu,
                TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_RETURNCMD,
                pt.x,
                pt.y,
                hwnd,
                std::ptr::null(),
            );
            let _ = PostMessageW(hwnd, WM_NULL, 0, 0);
            let _ = DestroyMenu(hmenu);

            match chosen as usize {
                CMD_OPEN_WINDOW => {
                    let _ = state.cmd_tx.send(TrayCommand::ShowWindow);
                }
                CMD_QUIT_APP => {
                    let _ = state.cmd_tx.send(TrayCommand::QuitApp);
                }
                _ => {}
            }
        }
    }

    unsafe extern "system" fn tray_wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // Never unwind across a Win32 callback boundary (`XCR-U04`).
        let result = std::panic::catch_unwind(|| {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TraySinkState;
            if msg == WM_NCDESTROY {
                if !ptr.is_null() {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    drop(Box::from_raw(ptr));
                }
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            if !ptr.is_null() {
                let state = &mut *ptr;
                if msg == WM_APP_TRAY {
                    let mouse_msg = (lparam as usize & 0xFFFF) as u32;
                    if matches!(mouse_msg, WM_LBUTTONUP | WM_LBUTTONDBLCLK) {
                        let _ = state.cmd_tx.send(TrayCommand::ShowWindow);
                        return 0;
                    }
                    if matches!(mouse_msg, WM_RBUTTONUP | WM_CONTEXTMENU) {
                        show_context_menu(hwnd, state);
                        return 0;
                    }
                } else if state.taskbar_created_msg != 0 && msg == state.taskbar_created_msg {
                    // Explorer restarted: re-register the notification-area icon.
                    let tip = lock_or_recover(&TRAY_TOOLTIP).clone();
                    let data = build_notify_data(hwnd, state.hicon, &tip);
                    if Shell_NotifyIconW(NIM_ADD, &data) != 0 {
                        state.icon_added = true;
                        TRAY_ACTIVE.store(true, Ordering::SeqCst);
                    }
                    return 0;
                } else if msg == WM_APP_UPDATE_TIP {
                    if state.icon_added {
                        let tip = lock_or_recover(&TRAY_TOOLTIP).clone();
                        let data = build_notify_data(hwnd, state.hicon, &tip);
                        let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
                    }
                    return 0;
                } else if msg == WM_APP_SHUTDOWN {
                    remove_icon(hwnd, state);
                    let _ = DestroyWindow(hwnd);
                    return 0;
                } else if msg == WM_ENDSESSION {
                    if wparam != 0 {
                        FORCE_EXIT.store(true, Ordering::SeqCst);
                        remove_icon(hwnd, state);
                    }
                    return 0;
                } else if msg == WM_DESTROY {
                    remove_icon(hwnd, state);
                    PostQuitMessage(0);
                    return 0;
                }
            } else if msg == WM_DESTROY {
                PostQuitMessage(0);
                return 0;
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        });
        result.unwrap_or(0)
    }

    pub(super) fn run_tray_thread(
        display_name: String,
        cmd_tx: Sender<TrayCommand>,
        ready_tx: SyncSender<(bool, isize)>,
    ) {
        delta_app::apply_process_quiet_guards();
        let class_name = format!("DeltaTraySink_{}", std::process::id());
        let class_w = wide_nul(&class_name);
        let title_w = wide_nul(&display_name);
        let taskbar_w = wide_nul("TaskbarCreated");

        unsafe {
            let module = GetModuleHandleW(std::ptr::null());
            let mut wc: WNDCLASSEXW = std::mem::zeroed();
            wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
            wc.lpfnWndProc = Some(tray_wnd_proc);
            wc.hInstance = module;
            wc.lpszClassName = class_w.as_ptr();
            if RegisterClassExW(&wc) == 0 {
                let _ = ready_tx.send((false, 0));
                return;
            }
            // Hidden top-level tool window (not HWND_MESSAGE) so Windows delivers broadcast
            // messages such as `TaskbarCreated` and `WM_ENDSESSION`.
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                class_w.as_ptr(),
                title_w.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                module,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                let _ = UnregisterClassW(class_w.as_ptr(), module);
                let _ = ready_tx.send((false, 0));
                return;
            }

            let hicon = load_embedded_tray_icon();
            let taskbar_created_msg = RegisterWindowMessageW(taskbar_w.as_ptr());
            let tip = lock_or_recover(&TRAY_TOOLTIP).clone();
            let icon_added = if !hicon.is_null() {
                let data = build_notify_data(hwnd, hicon, &tip);
                Shell_NotifyIconW(NIM_ADD, &data) != 0
            } else {
                false
            };

            let state = Box::new(TraySinkState {
                display_name,
                hicon,
                taskbar_created_msg,
                icon_added,
                cmd_tx,
            });
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);

            let _ = ready_tx.send((icon_added, hwnd as isize));
            if !icon_added {
                let _ = DestroyWindow(hwnd);
                let _ = UnregisterClassW(class_w.as_ptr(), module);
                return;
            }

            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let _ = UnregisterClassW(class_w.as_ptr(), module);
        }
    }

    pub(super) fn post_update_tip() {
        let hwnd = TRAY_WINDOW_HWND.load(Ordering::SeqCst);
        if hwnd != 0 {
            unsafe {
                let _ = PostMessageW(hwnd as HWND, WM_APP_UPDATE_TIP, 0, 0);
            }
        }
    }

    pub(super) fn post_shutdown() {
        let hwnd = TRAY_WINDOW_HWND.load(Ordering::SeqCst);
        if hwnd != 0 {
            unsafe {
                let _ = PostMessageW(hwnd as HWND, WM_APP_SHUTDOWN, 0, 0);
            }
        }
    }

    pub(super) fn is_window(hwnd: isize) -> bool {
        hwnd != 0 && unsafe { IsWindow(hwnd as HWND) != 0 }
    }

    pub(super) fn hide_window(hwnd: isize) {
        unsafe {
            let _ = ShowWindow(hwnd as HWND, SW_HIDE);
        }
    }

    pub(super) fn show_or_restore_window(hwnd: isize) {
        unsafe {
            if IsIconic(hwnd as HWND) != 0 {
                let _ = ShowWindow(hwnd as HWND, SW_RESTORE);
            } else if IsWindowVisible(hwnd as HWND) == 0 {
                let _ = ShowWindow(hwnd as HWND, SW_SHOW);
            }
        }
    }

    pub(super) fn find_main_gpui_hwnd(exclude_hwnd: isize) -> isize {
        struct Search {
            pid: u32,
            exclude: isize,
            preferred: isize,
            fallback: isize,
        }
        unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> i32 {
            let _ = std::panic::catch_unwind(|| {
                let search = &mut *(lparam as *mut Search);
                let raw = hwnd as isize;
                if raw == search.exclude {
                    return;
                }
                let mut pid = 0u32;
                GetWindowThreadProcessId(hwnd, &mut pid);
                if pid != search.pid {
                    return;
                }
                let mut cls = [0u16; 64];
                let len = GetClassNameW(hwnd, cls.as_mut_ptr(), cls.len() as i32).max(0) as usize;
                let name = String::from_utf16_lossy(&cls[..len]);
                match super::classify_window_class(&name) {
                    super::WindowClassRole::MainWindow => {
                        search.preferred = raw;
                        return;
                    }
                    super::WindowClassRole::Ignore => return,
                    super::WindowClassRole::FallbackCandidate => {}
                }
                if search.fallback == 0 {
                    let mut bounds: RECT = std::mem::zeroed();
                    if GetClientRect(hwnd, &mut bounds) != 0
                        && bounds.right.saturating_sub(bounds.left) >= 600
                        && bounds.bottom.saturating_sub(bounds.top) >= 400
                    {
                        search.fallback = raw;
                    }
                }
            });
            1
        }
        let mut search = Search {
            pid: unsafe { GetCurrentProcessId() },
            exclude: exclude_hwnd,
            preferred: 0,
            fallback: 0,
        };
        unsafe {
            let _ = EnumWindows(Some(enum_proc), &mut search as *mut Search as LPARAM);
        }
        if search.preferred != 0 {
            search.preferred
        } else {
            search.fallback
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_button_defaults_to_tray_and_respects_quit_and_fallback() {
        assert_eq!(CloseAction::default(), CloseAction::MinimizeToTray);
        assert_eq!(
            decide_window_close(CloseAction::MinimizeToTray, true, false),
            CloseDecision::HideToTray
        );
        // Without a tray icon, never hide the window where the user cannot reach it.
        assert_eq!(
            decide_window_close(CloseAction::MinimizeToTray, false, false),
            CloseDecision::GracefulQuit
        );
        assert_eq!(
            decide_window_close(CloseAction::Quit, true, false),
            CloseDecision::GracefulQuit
        );
        // Explicit quit / update / OS shutdown always close immediately.
        assert_eq!(
            decide_window_close(CloseAction::MinimizeToTray, true, true),
            CloseDecision::AllowImmediateClose
        );
        assert_eq!(
            decide_window_close(CloseAction::Quit, false, true),
            CloseDecision::AllowImmediateClose
        );
    }

    #[test]
    fn background_startup_parks_in_tray_when_available_and_falls_back_to_minimized() {
        assert_eq!(
            decide_startup_mode(false, true),
            BackgroundStartupMode::NormalWindow
        );
        assert_eq!(
            decide_startup_mode(false, false),
            BackgroundStartupMode::NormalWindow
        );
        assert_eq!(
            decide_startup_mode(true, true),
            BackgroundStartupMode::TrayOnly
        );
        assert_eq!(
            decide_startup_mode(true, false),
            BackgroundStartupMode::MinimizedTaskbar
        );
    }

    #[test]
    fn update_relaunch_args_match_previous_window_state() {
        let base = vec!["--data-dir".into(), "profile-data".into()];
        // Window was open -> relaunch without `--background` so the window opens again.
        assert_eq!(
            relaunch_args_for_update(&base, true, false),
            vec!["--data-dir", "profile-data"]
        );
        // Window was hidden in tray -> relaunch with `--background` so it stays in tray.
        assert_eq!(
            relaunch_args_for_update(&base, false, false),
            vec!["--data-dir", "profile-data", "--background"]
        );
        // Existing `--background` is stripped when the window was later opened before update.
        let with_bg = vec![
            "--data-dir".into(),
            "profile-data".into(),
            "--background".into(),
        ];
        assert_eq!(
            relaunch_args_for_update(&with_bg, true, false),
            vec!["--data-dir", "profile-data"]
        );
        // Verification resident preserves raw args untouched.
        let resident = vec!["--verification-resident".into()];
        assert_eq!(
            relaunch_args_for_update(&resident, false, true),
            vec!["--verification-resident"]
        );
    }

    #[test]
    fn tray_tooltip_and_menu_labels_format_and_bound_utf16_length() {
        assert_eq!(tray_tooltip_text("DELTA", None), "DELTA");
        assert_eq!(
            tray_tooltip_text("DELTA Dev", Some("同步进行中")),
            "DELTA Dev · 同步进行中"
        );
        let long = "长".repeat(200);
        let bounded = tray_tooltip_text("DELTA", Some(&long));
        assert!(bounded.encode_utf16().count() <= 127);

        assert_eq!(
            tray_menu_labels("DELTA"),
            ("打开 DELTA".to_string(), "退出 DELTA".to_string())
        );
        assert_eq!(
            tray_menu_labels("DELTA Dev"),
            ("打开 DELTA Dev".to_string(), "退出 DELTA Dev".to_string())
        );
    }

    #[test]
    fn window_class_classifier_matches_gpui_fast_and_ignores_sinks() {
        assert_eq!(
            classify_window_class("GpuiFast::Window"),
            WindowClassRole::MainWindow
        );
        assert_eq!(
            classify_window_class("Zed::Window"),
            WindowClassRole::MainWindow
        );
        assert_eq!(
            classify_window_class("GpuiFast::PlatformWindow"),
            WindowClassRole::Ignore
        );
        assert_eq!(
            classify_window_class("Zed::PlatformWindow"),
            WindowClassRole::Ignore
        );
        assert_eq!(
            classify_window_class("DeltaTraySink_1234"),
            WindowClassRole::Ignore
        );
        assert_eq!(
            classify_window_class("CustomWindow"),
            WindowClassRole::FallbackCandidate
        );
    }
}
