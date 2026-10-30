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
//!
//! ## Which signal covers which transition
//!
//! Bare Win taps are owned only while every transition that can hide key-ups
//! produces a reset. Lock/unlock as seen in validation logs (TASK-S06D):
//!
//! | Transition                  | Reset signal                              |
//! |-----------------------------|-------------------------------------------|
//! | Win+L lock, unlock          | WTS lock/unlock *and* desktop switch      |
//! | UAC prompt                  | desktop switch (WTS has no event for it)  |
//! | Ctrl+Alt+Del                | desktop switch (WTS has no event for it)  |
//! | RDP / user switch           | WTS connect/disconnect (not validated)    |
//! | elevated window focused     | foreground watcher, when not elevated     |
//!
//! Only lock/unlock is covered twice, so each observer is needed on its own:
//! the desktop-switch hook for UAC and Ctrl+Alt+Del, WTS for remote and
//! user-switch sessions, the foreground watcher for elevated windows (and
//! for knowing whether Start is open). `decide_super_tap` leaves the Win key
//! entirely to Windows when any of them could not be installed.

use crate::app::WM_USER_INPUT_RESET;
use crate::config::SuperTapConfig;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::{Duration, Instant};
use windows::core::HRESULT;
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

/// The observers bare Win tap ownership depends on, as installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SafetyObservers {
    /// WTS session notifications.
    pub session: bool,
    /// `EVENT_SYSTEM_DESKTOPSWITCH`.
    pub desktop_switch: bool,
    /// `foreground::ForegroundWatcher`'s foreground hook.
    pub foreground: bool,
}

impl SafetyObservers {
    fn missing(self) -> Vec<&'static str> {
        [
            (self.session, "session notifications (RDP, user switch)"),
            (self.desktop_switch, "desktop switches (UAC, Ctrl+Alt+Del)"),
            (
                self.foreground,
                "foreground changes (elevated windows, Start)",
            ),
        ]
        .into_iter()
        .filter_map(|(installed, what)| (!installed).then_some(what))
        .collect()
    }
}

/// Whether bare Win taps are owned, decided once at startup before the
/// keyboard hook starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuperTapDecision {
    /// `[overview]` is disabled or invalid.
    NotRequested,
    Enabled(SuperTapConfig),
    /// Requested, but these observers are missing: Win stays native.
    Unsafe(Vec<&'static str>),
}

impl SuperTapDecision {
    /// The configuration to start the keyboard hook with.
    pub fn config(&self) -> Option<SuperTapConfig> {
        match self {
            Self::Enabled(config) => Some(*config),
            Self::NotRequested | Self::Unsafe(_) => None,
        }
    }
}

/// Owns bare Win taps only when every reset signal can be observed (see the
/// table in the module docs): unknown key state must fail open.
pub fn decide_super_tap(
    requested: Option<SuperTapConfig>,
    observers: SafetyObservers,
) -> SuperTapDecision {
    let Some(config) = requested else {
        return SuperTapDecision::NotRequested;
    };
    match observers.missing() {
        missing if missing.is_empty() => SuperTapDecision::Enabled(config),
        missing => SuperTapDecision::Unsafe(missing),
    }
}

/// `RPC_S_INVALID_BINDING` (1702) as an `HRESULT`.
const RPC_S_INVALID_BINDING: HRESULT = HRESULT(0x8007_06A6_u32 as i32);

/// How long to keep retrying session notifications at startup.
const SESSION_RETRY_FOR: Duration = Duration::from_secs(10);

/// Registers WTS session notifications. Started at logon, WinOrbit can run
/// before Remote Desktop Services is ready, which fails with
/// `RPC_S_INVALID_BINDING` (per the `WTSRegisterSessionNotification` docs);
/// that one error is retried for a bounded time.
fn register_session_notifications(hwnd: HWND) -> bool {
    let started = Instant::now();
    loop {
        match unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) } {
            Ok(()) => return true,
            Err(err)
                if err.code() == RPC_S_INVALID_BINDING && started.elapsed() < SESSION_RETRY_FOR =>
            {
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(err) => {
                warn!("session notifications unavailable, {err}");
                return false;
            }
        }
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
    /// Never fails: what could not be installed is reported by `observers`,
    /// and `decide_super_tap` then leaves the Win key to Windows.
    pub fn init(hwnd: HWND) -> Self {
        RESET_TARGET.store(hwnd.0 as isize, Ordering::Release);
        let session_notifications = register_session_notifications(hwnd);
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

    /// Which of its signals were installed; `foreground` is filled in by the
    /// caller from `ForegroundWatcher`.
    pub fn observers(&self, foreground: bool) -> SafetyObservers {
        SafetyObservers {
            session: self.session_notifications,
            desktop_switch: !self.desktop_switch_hook.is_invalid(),
            foreground,
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
    use super::{
        decide_super_tap, request_reset_with, ResetReason, SafetyObservers, SuperTapDecision,
    };
    use crate::config::SuperTapConfig;
    use std::cell::RefCell;

    const ALL: SafetyObservers = SafetyObservers {
        session: true,
        desktop_switch: true,
        foreground: true,
    };

    #[test]
    fn bare_win_taps_are_owned_only_with_every_reset_observer() {
        let config = SuperTapConfig { double_tap_ms: 250 };
        assert_eq!(
            decide_super_tap(Some(config), ALL),
            SuperTapDecision::Enabled(config)
        );
        let without = [
            SafetyObservers {
                session: false,
                ..ALL
            },
            SafetyObservers {
                desktop_switch: false,
                ..ALL
            },
            SafetyObservers {
                foreground: false,
                ..ALL
            },
            SafetyObservers {
                session: false,
                desktop_switch: false,
                foreground: false,
            },
        ];
        for observers in without {
            let decision = decide_super_tap(Some(config), observers);
            assert!(
                matches!(&decision, SuperTapDecision::Unsafe(missing) if !missing.is_empty()),
                "{observers:?}: {decision:?}"
            );
            assert_eq!(decision.config(), None, "{observers:?}: still owned");
        }
        // Each missing observer is named.
        let SuperTapDecision::Unsafe(missing) = decide_super_tap(Some(config), without[3]) else {
            unreachable!()
        };
        assert_eq!(missing.len(), 3);
    }

    #[test]
    fn disabled_or_invalid_overview_is_native_whatever_the_observers() {
        for observers in [
            ALL,
            SafetyObservers {
                desktop_switch: false,
                ..ALL
            },
        ] {
            let decision = decide_super_tap(None, observers);
            assert_eq!(decision, SuperTapDecision::NotRequested);
            assert_eq!(decision.config(), None);
        }
    }

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
