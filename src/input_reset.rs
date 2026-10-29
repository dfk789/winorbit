//! Signals that the keyboard hook may have missed key-ups (TASK-S06D).
//!
//! While the secure desktop is active (Win+L lock screen, UAC prompt,
//! Ctrl+Alt+Del) the hook sees no input, so key-ups made there never reach
//! it. Each signal below makes the hook drop its physical key state through
//! `keyboard::reset_input_state`:
//!
//! - `WM_WTSSESSION_CHANGE` lock/unlock/connect, registered here and handled
//!   by the app window (`App::handle_message`).
//! - `EVENT_SYSTEM_DESKTOPSWITCH`, raised whenever the input desktop changes.
//!
//! Both arrive on the UI thread through its message loop; neither runs inside
//! the hook callback.

use crate::keyboard::reset_input_state;
use windows::Win32::{
    Foundation::HWND,
    System::RemoteDesktop::{
        WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
    },
    UI::{
        Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK},
        WindowsAndMessaging::{EVENT_SYSTEM_DESKTOPSWITCH, WINEVENT_OUTOFCONTEXT},
    },
};

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
    reset_input_state("input desktop switch");
}
