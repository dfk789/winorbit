use crate::input_reset::{request_reset, ResetReason};
use crate::utils::{get_window_cloak_type, get_window_exe, get_window_pid, is_process_elevated};
use anyhow::{bail, Result};
use once_cell::sync::OnceCell;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::{
    Foundation::HWND,
    UI::{
        Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK},
        WindowsAndMessaging::{
            GetForegroundWindow, IsWindowVisible, EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED,
            EVENT_SYSTEM_FOREGROUND, OBJID_WINDOW, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
        },
    },
};

/// Written by the foreground WinEvent hook on the UI thread and read by the
/// keyboard hook thread.
pub static IS_FOREGROUND_IN_BLACKLIST: AtomicBool = AtomicBool::new(false);

/// Start (or Search) may be open in the foreground (`is_start`). Written like
/// `IS_FOREGROUND_IN_BLACKLIST`, only while bare Win taps are owned
/// (`[overview] enable`): a tap then closes Start natively.
pub static IS_FOREGROUND_START: AtomicBool = AtomicBool::new(false);

/// How Start is recognized, fixed when the watcher starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartTracking {
    /// Cloak changes are watched, so a Start host counts only while visibly
    /// open (`is_visible_start`), and is re-checked when it uncloaks without
    /// a new foreground event.
    CloakEvents,
    /// Cloak changes cannot be watched: a Start host in the foreground counts
    /// as Start even if hidden or cloaked, so a tap is left to Windows rather
    /// than owned while Start may be visible. Costs only extra native taps.
    HostOnly,
}

impl StartTracking {
    pub fn describe(self) -> &'static str {
        match self {
            Self::CloakEvents => "visible Start, cloak changes watched",
            Self::HostOnly => "any Start host foreground (cloak changes not watched)",
        }
    }
}

/// Processes hosting the Start menu: `SearchHost.exe` on Windows 11 23H2
/// (seen by the S06D probe), `StartMenuExperienceHost.exe` on other builds.
const START_EXES: [&str; 2] = ["searchhost.exe", "startmenuexperiencehost.exe"];

static BLACKLIST: OnceCell<HashSet<String>> = OnceCell::new();
static TRACK_START: AtomicBool = AtomicBool::new(false);
/// `StartTracking::HostOnly` is in effect.
static START_HOST_ONLY: AtomicBool = AtomicBool::new(false);
/// WinOrbit is not elevated, so focus moving to an elevated window hides
/// the following key-ups from the keyboard hook.
static TRACK_ELEVATED: AtomicBool = AtomicBool::new(false);

/// Whether a foreground window is a visibly open Start menu. The Start hosts
/// also become foreground while hidden or DWM-cloaked (validation saw
/// SearchHost do so during Win+Left), which must not count. An unknown owner
/// is not Start, so taps stay owned. `super-validate.ps1` uses the same rule.
pub fn is_visible_start(exe: Option<&str>, is_visible: bool, cloak_type: u32) -> bool {
    is_start_host(exe) && is_visible && cloak_type == 0
}

fn is_start_host(exe: Option<&str>) -> bool {
    exe.is_some_and(|exe| START_EXES.contains(&exe))
}

/// Whether a foreground window counts as an open Start under `tracking`.
pub fn is_start(
    tracking: StartTracking,
    exe: Option<&str>,
    is_visible: bool,
    cloak_type: u32,
) -> bool {
    match tracking {
        StartTracking::CloakEvents => is_visible_start(exe, is_visible, cloak_type),
        StartTracking::HostOnly => is_start_host(exe),
    }
}

/// Re-classifies `hwnd` (the foreground window) as Start or not.
fn update_start_flag(hwnd: HWND, exe: Option<&str>) {
    let is_visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
    let cloak_type = get_window_cloak_type(hwnd);
    let tracking = match START_HOST_ONLY.load(Ordering::Relaxed) {
        true => StartTracking::HostOnly,
        false => StartTracking::CloakEvents,
    };
    let is_start = is_start(tracking, exe, is_visible, cloak_type);
    let was_start = IS_FOREGROUND_START.swap(is_start, Ordering::Relaxed);
    if is_start != was_start || is_start_host(exe) {
        debug!(
            "start open={is_start} ({}, visible={is_visible}, cloak={cloak_type:#x})",
            exe.unwrap_or("?")
        );
    }
}

#[derive(Debug)]
pub struct ForegroundWatcher {
    hook: HWINEVENTHOOK,
    /// Cloak changes of the foreground window, only while tracking Start: a
    /// Start host can uncloak without a new foreground event.
    cloak_hook: HWINEVENTHOOK,
}

impl ForegroundWatcher {
    /// Fails only when the blacklist needs the foreground hook and it cannot
    /// be installed. Without a blacklist, a failed hook is reported by
    /// `is_watching` instead, and bare Win taps are then left to Windows.
    pub fn init(
        blacklist: &HashSet<String>,
        track_start: bool,
        track_elevated: bool,
    ) -> Result<Self> {
        IS_FOREGROUND_START.store(false, Ordering::Relaxed);
        if blacklist.is_empty() && !track_start && !track_elevated {
            return Ok(Self {
                hook: HWINEVENTHOOK::default(),
                cloak_hook: HWINEVENTHOOK::default(),
            });
        }

        let _ = BLACKLIST.set(blacklist.iter().map(|v| v.to_lowercase()).collect());
        TRACK_START.store(track_start, Ordering::Relaxed);
        TRACK_ELEVATED.store(track_elevated, Ordering::Relaxed);

        let hook = unsafe {
            SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                None,
                Some(win_event_proc),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        };
        let mut watcher = Self {
            hook,
            cloak_hook: HWINEVENTHOOK::default(),
        };
        if hook.is_invalid() {
            if !blacklist.is_empty() {
                bail!("Failed to watch foreground");
            }
            warn!("cannot watch the foreground window");
            return Ok(watcher);
        }

        if track_start {
            watcher.cloak_hook = unsafe {
                SetWinEventHook(
                    EVENT_OBJECT_CLOAKED,
                    EVENT_OBJECT_UNCLOAKED,
                    None,
                    Some(cloak_event_proc),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                )
            };
            let tracking = match watcher.cloak_hook.is_invalid() {
                true => StartTracking::HostOnly,
                false => StartTracking::CloakEvents,
            };
            START_HOST_ONLY.store(tracking == StartTracking::HostOnly, Ordering::Relaxed);
            if tracking == StartTracking::HostOnly {
                warn!("cannot watch Start cloaking: {}", tracking.describe());
            }
            // Start may already be open: classify the current foreground now
            // instead of waiting for the next focus change.
            let hwnd = unsafe { GetForegroundWindow() };
            let exe = get_window_exe(hwnd).map(|v| v.to_lowercase());
            update_start_flag(hwnd, exe.as_deref());
        }

        info!("foreground watcher start");

        Ok(watcher)
    }

    /// The foreground hook is installed.
    pub fn is_watching(&self) -> bool {
        !self.hook.is_invalid()
    }

    /// How Start is recognized; only meaningful while tracking Start.
    pub fn start_tracking(&self) -> StartTracking {
        match self.cloak_hook.is_invalid() {
            true => StartTracking::HostOnly,
            false => StartTracking::CloakEvents,
        }
    }
}

impl Drop for ForegroundWatcher {
    fn drop(&mut self) {
        debug!("foreground watcher destroyed");
        for hook in [self.hook, self.cloak_hook] {
            if !hook.is_invalid() {
                unsafe {
                    let _ = UnhookWinEvent(hook);
                }
            }
        }
    }
}

unsafe extern "system" fn win_event_proc(
    _h_win_event_hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _dw_event_thread: u32,
    _dwms_event_time: u32,
) {
    if TRACK_ELEVATED.load(Ordering::Relaxed) {
        let pid = get_window_pid(hwnd);
        // An unreadable token is treated as elevated: a needless reset only
        // drops key state, a missed one leaves keys stuck down for the hook.
        if pid != 0 && is_process_elevated(pid) != Some(false) {
            request_reset(ResetReason::ElevatedForeground);
        }
    }
    let exe = get_window_exe(hwnd).map(|v| v.to_lowercase());
    if TRACK_START.load(Ordering::Relaxed) {
        update_start_flag(hwnd, exe.as_deref());
    }
    let Some(exe) = exe else {
        return;
    };
    let is_in_blacklist = BLACKLIST.get().unwrap().contains(&exe);
    IS_FOREGROUND_IN_BLACKLIST.store(is_in_blacklist, Ordering::Relaxed);
    debug!("foreground {exe} {is_in_blacklist}");
}

unsafe extern "system" fn cloak_event_proc(
    _h_win_event_hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _dw_event_thread: u32,
    _dwms_event_time: u32,
) {
    // Only the foreground window itself matters; other windows cloak and
    // uncloak all the time (virtual desktop switches).
    if id_object != OBJID_WINDOW.0 || hwnd != GetForegroundWindow() {
        return;
    }
    let exe = get_window_exe(hwnd).map(|v| v.to_lowercase());
    update_start_flag(hwnd, exe.as_deref());
}

#[cfg(test)]
mod tests {
    use super::{is_start, is_visible_start, StartTracking};

    #[test]
    fn without_cloak_events_any_start_host_counts_as_start() {
        // (exe, visible, cloak, with cloak events, host only)
        let cases = [
            ("notepad.exe", true, 0, false, false),
            ("searchhost.exe", true, 0, true, true),
            // Hidden or cloaked: possibly about to uncloak unseen.
            ("searchhost.exe", false, 0, false, true),
            ("searchhost.exe", true, 0x2, false, true),
            ("startmenuexperiencehost.exe", true, 0x1, false, true),
            ("explorer.exe", false, 0x2, false, false),
        ];
        for (exe, visible, cloak, cloak_events, host_only) in cases {
            let what = format!("{exe} visible={visible} cloak={cloak:#x}");
            assert_eq!(
                is_start(StartTracking::CloakEvents, Some(exe), visible, cloak),
                cloak_events,
                "{what}"
            );
            assert_eq!(
                is_start(StartTracking::HostOnly, Some(exe), visible, cloak),
                host_only,
                "{what}"
            );
        }
        for tracking in [StartTracking::CloakEvents, StartTracking::HostOnly] {
            assert!(!is_start(tracking, None, true, 0), "unreadable process");
        }
    }

    #[test]
    fn only_a_visible_uncloaked_start_host_is_start() {
        let cases = [
            ("normal app", Some("notepad.exe"), true, 0, false),
            (
                "visible StartMenuExperienceHost",
                Some("startmenuexperiencehost.exe"),
                true,
                0,
                true,
            ),
            (
                "visible SearchHost (Start on 23H2)",
                Some("searchhost.exe"),
                true,
                0,
                true,
            ),
            (
                "invisible SearchHost",
                Some("searchhost.exe"),
                false,
                0,
                false,
            ),
            (
                "cloaked SearchHost",
                Some("searchhost.exe"),
                true,
                0x2,
                false,
            ),
            (
                "shell-cloaked StartMenuExperienceHost",
                Some("startmenuexperiencehost.exe"),
                true,
                0x1,
                false,
            ),
            ("unreadable process", None, true, 0, false),
        ];
        for (name, exe, visible, cloak, expected) in cases {
            assert_eq!(is_visible_start(exe, visible, cloak), expected, "{name}");
        }
    }
}
