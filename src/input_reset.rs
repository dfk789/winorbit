//! Signals that the keyboard hook may have missed key-ups (TASK-S06D).
//!
//! While the secure desktop is active (Win+L lock screen, UAC prompt,
//! Ctrl+Alt+Del) the hook sees no input, so key-ups made there never reach
//! it. Each signal below makes the hook drop its physical key state through
//! `keyboard::reset_input_state`:
//!
//! - `WM_WTSSESSION_CHANGE` lock/unlock/connect, registered here.
//! - `EVENT_SYSTEM_DESKTOPSWITCH`, raised whenever the input desktop changes,
//!   forwarded to the app window as `WM_USER_INPUT_RESET`.
//! - Focus moving to an elevated window while WinOrbit is not elevated
//!   (reported by `foreground::ForegroundWatcher`): Windows then sends the
//!   hook nothing, so keys held at that moment (Ctrl+Shift+Esc opening Task
//!   Manager, Win+1 activating an elevated app) lose their key-ups.
//!
//! A reset has two independent parts:
//!
//! 1. Hook safety: `keyboard::reset_input_state` bumps an atomic epoch at
//!    once, from the thread that received the signal. The hook drops its key
//!    state and enters probation on its next event, whatever the UI is doing.
//! 2. UI cleanup: the app window cancels any open switcher, because the
//!    release that would have committed it can no longer arrive. This is
//!    posted (`WM_USER_INPUT_RESET`) and never bumps the epoch again.
//!
//! WTS notifications arrive as window messages, so their epoch bump happens
//! first thing in the handler, after any messages queued before them.
//! None of this runs inside the hook callback.

use crate::app::WM_USER_INPUT_RESET;
use std::sync::atomic::{AtomicIsize, Ordering};
use windows::Win32::{
    Foundation::{HWND, LPARAM, WPARAM},
    System::RemoteDesktop::{
        WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
    },
    UI::{
        Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK},
        WindowsAndMessaging::{PostMessageW, EVENT_SYSTEM_DESKTOPSWITCH, WINEVENT_OUTOFCONTEXT},
    },
};

/// App window that reset requests are forwarded to (as an integer because
/// `HWND` is not `Send`); 0 when no watcher is installed.
static RESET_TARGET: AtomicIsize = AtomicIsize::new(0);

/// Why key-ups may have been lost; sent as the `WPARAM` of
/// `WM_USER_INPUT_RESET`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetReason {
    DesktopSwitch,
    ElevatedForeground,
}

impl ResetReason {
    const ALL: [Self; 2] = [Self::DesktopSwitch, Self::ElevatedForeground];

    pub fn describe(self) -> &'static str {
        match self {
            Self::DesktopSwitch => "input desktop switch",
            Self::ElevatedForeground => "elevated window focused",
        }
    }

    pub fn from_wparam(wparam: usize) -> Option<Self> {
        Self::ALL.get(wparam).copied()
    }

    fn to_wparam(self) -> usize {
        self as usize
    }
}

/// Resets the hook now, then asks the app window to cancel any open switcher.
pub fn request_reset(reason: ResetReason) {
    request_reset_with(reason, crate::keyboard::reset_input_state, |reason| {
        let target = RESET_TARGET.load(Ordering::Acquire);
        target != 0
            && unsafe {
                PostMessageW(
                    Some(HWND(target as _)),
                    WM_USER_INPUT_RESET,
                    WPARAM(reason.to_wparam()),
                    LPARAM(0),
                )
            }
            .is_ok()
    });
}

/// `request_reset` with its two effects injectable, for tests. The hook reset
/// always happens, first and exactly once; a failed post only loses the
/// switcher cancel.
fn request_reset_with(
    reason: ResetReason,
    reset_hook: impl FnOnce(&str),
    post_cleanup: impl FnOnce(ResetReason) -> bool,
) {
    reset_hook(reason.describe());
    if !post_cleanup(reason) {
        warn!(
            "could not ask the UI to cancel the switcher after: {}",
            reason.describe()
        );
    }
}

/// Owns both registrations and undoes them on drop. Must be dropped on the
/// thread that created it (the UI thread), before its window is destroyed.
#[derive(Debug)]
pub struct InputResetWatcher {
    hwnd: HWND,
    session_notifications: bool,
    desktop_switch_hook: HWINEVENTHOOK,
}

impl InputResetWatcher {
    /// Never fails: without a signal the hook still works, it just cannot
    /// recover from key-ups lost on the secure desktop (see `super_key`).
    pub fn init(hwnd: HWND) -> Self {
        RESET_TARGET.store(hwnd.0 as isize, Ordering::Release);
        let session_notifications =
            match unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) } {
                Ok(()) => true,
                Err(err) => {
                    warn!("session lock notifications unavailable, {err}");
                    false
                }
            };
        let desktop_switch_hook = unsafe {
            SetWinEventHook(
                EVENT_SYSTEM_DESKTOPSWITCH,
                EVENT_SYSTEM_DESKTOPSWITCH,
                None,
                Some(desktop_switch_proc),
                0,
                0,
                WINEVENT_OUTOFCONTEXT,
            )
        };
        if desktop_switch_hook.is_invalid() {
            warn!("desktop switch notifications unavailable");
        }
        Self {
            hwnd,
            session_notifications,
            desktop_switch_hook,
        }
    }
}

impl Drop for InputResetWatcher {
    fn drop(&mut self) {
        RESET_TARGET.store(0, Ordering::Release);
        if !self.desktop_switch_hook.is_invalid() {
            let _ = unsafe { UnhookWinEvent(self.desktop_switch_hook) };
        }
        if self.session_notifications {
            let _ = unsafe { WTSUnRegisterSessionNotification(self.hwnd) };
        }
    }
}

unsafe extern "system" fn desktop_switch_proc(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _event_thread: u32,
    _event_time: u32,
) {
    request_reset(ResetReason::DesktopSwitch);
}

#[cfg(test)]
mod tests {
    use super::{request_reset_with, ResetReason};
    use std::cell::RefCell;

    #[test]
    fn reset_reasons_round_trip_through_wparam() {
        for reason in ResetReason::ALL {
            assert_eq!(ResetReason::from_wparam(reason.to_wparam()), Some(reason));
        }
        assert_eq!(ResetReason::from_wparam(ResetReason::ALL.len()), None);
    }

    #[test]
    fn hook_reset_happens_first_and_once_whether_or_not_the_ui_is_reachable() {
        for posted in [true, false] {
            for reason in ResetReason::ALL {
                let calls = RefCell::new(vec![]);
                request_reset_with(
                    reason,
                    |why| calls.borrow_mut().push(format!("reset {why}")),
                    |sent| {
                        calls.borrow_mut().push(format!("post {sent:?}"));
                        posted
                    },
                );
                assert_eq!(
                    calls.into_inner(),
                    vec![
                        format!("reset {}", reason.describe()),
                        format!("post {reason:?}")
                    ],
                    "posted={posted}"
                );
            }
        }
    }
}
