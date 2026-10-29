use crate::utils::get_window_exe;
use anyhow::{bail, Result};
use once_cell::sync::OnceCell;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::{
    Foundation::HWND,
    UI::{
        Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK},
        WindowsAndMessaging::{
            EVENT_SYSTEM_FOREGROUND, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
        },
    },
};

/// Written by the foreground WinEvent hook on the UI thread and read by the
/// keyboard hook thread.
pub static IS_FOREGROUND_IN_BLACKLIST: AtomicBool = AtomicBool::new(false);

/// Start (or Search) is the foreground window. Written like
/// `IS_FOREGROUND_IN_BLACKLIST`, only while bare Win taps are owned
/// (`[overview] enable`): a tap then closes Start natively.
pub static IS_FOREGROUND_START: AtomicBool = AtomicBool::new(false);

/// Processes hosting the Start menu: `SearchHost.exe` on Windows 11 23H2
/// (seen by the S06D probe), `StartMenuExperienceHost.exe` on other builds.
const START_EXES: [&str; 2] = ["searchhost.exe", "startmenuexperiencehost.exe"];

static BLACKLIST: OnceCell<HashSet<String>> = OnceCell::new();
static TRACK_START: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
pub struct ForegroundWatcher {
    hook: HWINEVENTHOOK,
}

impl ForegroundWatcher {
    pub fn init(blacklist: &HashSet<String>, track_start: bool) -> Result<Self> {
        IS_FOREGROUND_START.store(false, Ordering::Relaxed);
        if blacklist.is_empty() && !track_start {
            return Ok(Self {
                hook: HWINEVENTHOOK::default(),
            });
        }

        let _ = BLACKLIST.set(blacklist.iter().map(|v| v.to_lowercase()).collect());
        TRACK_START.store(track_start, Ordering::Relaxed);

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
        if hook.is_invalid() {
            bail!("Failed to watch foreground");
        }

        info!("foreground watcher start");

        Ok(Self { hook })
    }
}

impl Drop for ForegroundWatcher {
    fn drop(&mut self) {
        debug!("foreground watcher destroyed");
        if !self.hook.is_invalid() {
            unsafe {
                let _ = UnhookWinEvent(self.hook);
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
    let exe = get_window_exe(hwnd).map(|v| v.to_lowercase());
    if TRACK_START.load(Ordering::Relaxed) {
        // Unknown owner: not Start, so taps stay owned.
        let is_start = exe.as_deref().is_some_and(|exe| START_EXES.contains(&exe));
        IS_FOREGROUND_START.store(is_start, Ordering::Relaxed);
    }
    let Some(exe) = exe else {
        return;
    };
    let is_in_blacklist = BLACKLIST.get().unwrap().contains(&exe);
    IS_FOREGROUND_IN_BLACKLIST.store(is_in_blacklist, Ordering::Relaxed);
    debug!("foreground {exe} {is_in_blacklist}");
}
