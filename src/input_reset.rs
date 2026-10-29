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
//!
//! Both are handled by the app window (`App::handle_message`), which resets
//! the hook and cancels any open switcher, since the release that would have
//! committed it can no longer arrive. Neither runs inside the hook callback.

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

/// App window that desktop-switch events are forwarded to (as an integer
/// because `HWND` is not `Send`); 0 when no watcher is installed.
static RESET_TARGET: AtomicIsize = AtomicIsize::new(0);

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
    let target = RESET_TARGET.load(Ordering::Acquire);
    if target == 0 {
        return;
    }
    if let Err(err) = unsafe {
        PostMessageW(
            Some(HWND(target as _)),
            WM_USER_INPUT_RESET,
            WPARAM(0),
            LPARAM(0),
        )
    } {
        // Still reset the hook; only the switcher cancel is lost.
        crate::keyboard::reset_input_state("input desktop switch (post failed)");
        error!("failed to forward desktop switch, {err}");
    }
}
