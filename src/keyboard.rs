use crate::{
    app::{
        WM_USER_SWITCH_APPS, WM_USER_SWITCH_APPS_CANCEL, WM_USER_SWITCH_APPS_DONE,
        WM_USER_SWITCH_WINDOWS, WM_USER_SWITCH_WINDOWS_DONE,
    },
    config::{Hotkey, SWITCH_APPS_HOTKEY_ID, SWITCH_WINDOWS_HOTKEY_ID},
    foreground::IS_FOREGROUND_IN_BLACKLIST,
};

use anyhow::{anyhow, Result};
use parking_lot::{Condvar, Mutex};
use std::sync::{atomic::Ordering, mpsc, LazyLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::{
        Input::KeyboardAndMouse::{SCANCODE_LSHIFT, SCANCODE_RSHIFT},
        WindowsAndMessaging::{
            CallNextHookEx, DispatchMessageW, GetMessageW, PeekMessageW, PostMessageW,
            PostThreadMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
            KBDLLHOOKSTRUCT, LLKHF_UP, MSG, PM_NOREMOVE, WH_KEYBOARD_LL, WM_QUIT,
        },
    },
};

/// State shared with `keyboard_proc`, which only ever runs on the hook thread.
static KEYBOARD_STATE: LazyLock<Mutex<KeyboardState>> =
    LazyLock::new(|| Mutex::new(KeyboardState::default()));

#[derive(Debug, Default)]
struct KeyboardState {
    /// App window that receives posted switch messages, stored as an integer
    /// because `HWND` is not `Send`.
    window: isize,
    is_shift_pressed: bool,
    hotkeys: Vec<HotKeyState>,
}

/// Upper bound on how long the hook waits for the UI thread to finish a commit.
const COMMIT_ACK_TIMEOUT: Duration = Duration::from_millis(250);

/// Counts commit messages posted by the hook and acknowledged by the UI thread.
///
/// The hook waits for each commit to be handled before passing the modifier
/// release on, so the key-up reaches the newly activated window, exactly as it
/// did when the hook ran on the UI thread and used `SendMessageW`.
static COMMIT_SYNC: LazyLock<(Mutex<CommitCounter>, Condvar)> =
    LazyLock::new(|| (Mutex::new(CommitCounter::default()), Condvar::new()));

#[derive(Debug, Default)]
struct CommitCounter {
    posted: u64,
    acknowledged: u64,
}

/// Called by the UI thread once it has handled a `*_DONE` commit message.
pub fn acknowledge_commit() {
    let (counter, condvar) = &*COMMIT_SYNC;
    counter.lock().acknowledged += 1;
    condvar.notify_all();
}

/// Owns the dedicated thread that installs `WH_KEYBOARD_LL` and pumps its
/// messages. Low-level hook callbacks run on the installing thread, so keeping
/// them off the UI thread means overlay painting can never stall global input.
#[derive(Debug)]
pub struct KeyboardListener {
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
}

impl KeyboardListener {
    pub fn init(hwnd: HWND, hotkeys: &[&Hotkey]) -> Result<Self> {
        {
            let mut state = KEYBOARD_STATE.lock();
            state.window = hwnd.0 as isize;
            state.is_shift_pressed = false;
            state.hotkeys = hotkeys
                .iter()
                .map(|hotkey| HotKeyState::new((*hotkey).clone()))
                .collect();
        }

        let (tx, rx) = mpsc::channel::<Result<u32>>();
        let thread = std::thread::Builder::new()
            .name("keyboard-hook".into())
            .spawn(move || run_hook_thread(tx))
            .map_err(|err| anyhow!("Failed to spawn keyboard hook thread, {err}"))?;

        let thread_id = match rx.recv() {
            Ok(Ok(thread_id)) => thread_id,
            Ok(Err(err)) => {
                let _ = thread.join();
                return Err(err);
            }
            Err(_) => {
                let _ = thread.join();
                return Err(anyhow!("Keyboard hook thread exited during startup"));
            }
        };
        info!("keyboard listener start");

        Ok(Self {
            thread_id,
            thread: Some(thread),
        })
    }
}

impl Drop for KeyboardListener {
    fn drop(&mut self) {
        debug!("keyboard listener destroyed");
        let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_hook_thread(ready: mpsc::Sender<Result<u32>>) {
    let mut message = MSG::default();
    // Force creation of this thread's message queue before reporting the
    // thread id, so a `WM_QUIT` posted by `Drop` can never be lost.
    let _ = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE) };
    let thread_id = unsafe { GetCurrentThreadId() };

    let hook = unsafe {
        GetModuleHandleW(None)
            .map_err(|err| anyhow!("Failed to get module handle, {err}"))
            .and_then(|hinstance| {
                SetWindowsHookExW(
                    WH_KEYBOARD_LL,
                    Some(keyboard_proc),
                    Some(hinstance.into()),
                    0,
                )
                .map_err(|err| anyhow!("Failed to set windows hook, {err}"))
            })
    };
    let hook = match hook {
        Ok(hook) => hook,
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };
    let _ = ready.send(Ok(thread_id));

    // The hook callback is dispatched from inside this message loop.
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }

    // Unhook on the thread that installed the hook.
    let _ = unsafe { UnhookWindowsHookEx(hook) };
    debug!("keyboard hook thread exited");
}

#[derive(Debug)]
struct HotKeyState {
    hotkey: Hotkey,
    is_modifier_pressed: bool,
    is_hotkey_pressed: bool,
    should_commit_on_modifier_release: bool,
}

impl HotKeyState {
    fn new(hotkey: Hotkey) -> Self {
        Self {
            hotkey,
            is_modifier_pressed: false,
            is_hotkey_pressed: false,
            should_commit_on_modifier_release: false,
        }
    }

    fn handle_modifier_event(
        &mut self,
        scan_code: u32,
        is_key_pressed: bool,
    ) -> Option<HotKeyAction> {
        if !self.hotkey.modifier.contains(&scan_code) {
            return None;
        }

        if is_key_pressed {
            self.is_modifier_pressed = true;
            return None;
        }

        self.is_modifier_pressed = false;
        self.is_hotkey_pressed = false;
        let should_commit = self.should_commit_on_modifier_release;
        self.should_commit_on_modifier_release = false;

        if !should_commit {
            return None;
        }

        match self.hotkey.id {
            SWITCH_APPS_HOTKEY_ID => Some(HotKeyAction::SwitchAppsDone),
            SWITCH_WINDOWS_HOTKEY_ID => Some(HotKeyAction::SwitchWindowsDone),
            _ => None,
        }
    }

    fn handle_hotkey_event(
        &mut self,
        scan_code: u32,
        is_key_pressed: bool,
        reverse: bool,
        allow_switch: bool,
    ) -> Option<HotKeyAction> {
        if scan_code != self.hotkey.code {
            return None;
        }

        if !is_key_pressed {
            self.is_hotkey_pressed = false;
            return None;
        }

        let is_repeat = self.is_hotkey_pressed;
        self.is_hotkey_pressed = true;

        if !self.is_modifier_pressed {
            return None;
        }

        if self.hotkey.id == SWITCH_APPS_HOTKEY_ID && is_repeat {
            // Swallow repeated Alt+Tab keydowns so Windows does not see the native
            // shortcut, but only advance once per physical Tab press.
            return Some(HotKeyAction::Consume);
        }

        if !allow_switch {
            return None;
        }

        self.should_commit_on_modifier_release = true;
        match self.hotkey.id {
            SWITCH_APPS_HOTKEY_ID => Some(HotKeyAction::SwitchApps { reverse }),
            SWITCH_WINDOWS_HOTKEY_ID => Some(HotKeyAction::SwitchWindows { reverse }),
            _ => None,
        }
    }

    fn handle_cancel_event(
        &mut self,
        scan_code: u32,
        is_key_pressed: bool,
    ) -> Option<HotKeyAction> {
        if self.hotkey.id != SWITCH_APPS_HOTKEY_ID
            || scan_code != 0x01
            || !is_key_pressed
            || !self.is_modifier_pressed
        {
            return None;
        }

        self.should_commit_on_modifier_release = false;
        Some(HotKeyAction::SwitchAppsCancel)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HotKeyAction {
    Consume,
    SwitchApps { reverse: bool },
    SwitchAppsDone,
    SwitchAppsCancel,
    SwitchWindows { reverse: bool },
    SwitchWindowsDone,
}

unsafe extern "system" fn keyboard_proc(code: i32, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
    let kbd_data: &KBDLLHOOKSTRUCT = &*(l_param.0 as *const _);
    debug!("keyboard {kbd_data:?}");
    let mut is_modifier = false;
    let scan_code = kbd_data.scanCode;
    let is_key_pressed = || kbd_data.flags.0 & LLKHF_UP.0 == 0;
    let mut keyboard_state = KEYBOARD_STATE.lock();
    if [SCANCODE_LSHIFT, SCANCODE_RSHIFT].contains(&scan_code) {
        keyboard_state.is_shift_pressed = is_key_pressed();
    }
    let reverse = keyboard_state.is_shift_pressed;
    let window = HWND(keyboard_state.window as _);
    let is_foreground_in_blacklist = IS_FOREGROUND_IN_BLACKLIST.load(Ordering::Relaxed);
    for state in keyboard_state.hotkeys.iter_mut() {
        if let Some(action) = state.handle_modifier_event(scan_code, is_key_pressed()) {
            is_modifier = true;
            dispatch_action(window, action);
        } else if state.hotkey.modifier.contains(&scan_code) {
            is_modifier = true;
        }
    }
    if !is_modifier {
        for state in keyboard_state.hotkeys.iter_mut() {
            if let Some(action) = state.handle_hotkey_event(
                scan_code,
                is_key_pressed(),
                reverse,
                state.hotkey.id != SWITCH_WINDOWS_HOTKEY_ID || !is_foreground_in_blacklist,
            ) {
                dispatch_action(window, action);
                return LRESULT(1);
            }
            if let Some(action) = state.handle_cancel_event(scan_code, is_key_pressed()) {
                dispatch_action(window, action);
                return LRESULT(1);
            }
        }
    }
    CallNextHookEx(None, code, w_param, l_param)
}

/// Maps a hook action to the app-window message it posts, if any.
fn action_message(action: HotKeyAction) -> Option<(u32, LPARAM)> {
    match action {
        HotKeyAction::Consume => None,
        HotKeyAction::SwitchApps { reverse } => {
            Some((WM_USER_SWITCH_APPS, LPARAM(reverse as isize)))
        }
        HotKeyAction::SwitchAppsDone => Some((WM_USER_SWITCH_APPS_DONE, LPARAM(0))),
        HotKeyAction::SwitchAppsCancel => Some((WM_USER_SWITCH_APPS_CANCEL, LPARAM(0))),
        HotKeyAction::SwitchWindows { reverse } => {
            Some((WM_USER_SWITCH_WINDOWS, LPARAM(reverse as isize)))
        }
        HotKeyAction::SwitchWindowsDone => Some((WM_USER_SWITCH_WINDOWS_DONE, LPARAM(0))),
    }
}

/// Posts rather than sends so the hook callback never runs app code; the UI
/// thread handles switching and painting from its own message loop. Commit
/// messages additionally wait (bounded) for the UI thread to finish.
fn dispatch_action(window: HWND, action: HotKeyAction) {
    let Some((message, lparam)) = action_message(action) else {
        return;
    };
    let is_commit = matches!(
        action,
        HotKeyAction::SwitchAppsDone | HotKeyAction::SwitchWindowsDone
    );
    let (counter, condvar) = &*COMMIT_SYNC;
    let mut counter = counter.lock();
    if let Err(err) = unsafe { PostMessageW(Some(window), message, WPARAM(0), lparam) } {
        error!("failed to post keyboard action {action:?}, {err}");
        return;
    }
    if !is_commit {
        return;
    }
    counter.posted += 1;
    let expected = counter.posted;
    let deadline = Instant::now() + COMMIT_ACK_TIMEOUT;
    while counter.acknowledged < expected {
        if condvar.wait_until(&mut counter, deadline).timed_out() {
            warn!("timed out waiting for keyboard commit {action:?}");
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{action_message, HotKeyAction, HotKeyState};
    use crate::app::{
        WM_USER_SWITCH_APPS, WM_USER_SWITCH_APPS_CANCEL, WM_USER_SWITCH_APPS_DONE,
        WM_USER_SWITCH_WINDOWS, WM_USER_SWITCH_WINDOWS_DONE,
    };
    use crate::config::{Hotkey, SWITCH_APPS_HOTKEY_ID, SWITCH_WINDOWS_HOTKEY_ID};

    fn switch_apps_state() -> HotKeyState {
        HotKeyState::new(
            Hotkey::create(SWITCH_APPS_HOTKEY_ID, "switch apps", "alt+tab")
                .expect("switch-apps hotkey should parse"),
        )
    }

    fn switch_windows_state() -> HotKeyState {
        HotKeyState::new(
            Hotkey::create(SWITCH_WINDOWS_HOTKEY_ID, "switch windows", "alt+`")
                .expect("switch-windows hotkey should parse"),
        )
    }

    #[test]
    fn app_switch_repeat_is_consumed_without_advancing_again() {
        let mut state = switch_apps_state();

        assert_eq!(state.handle_modifier_event(0x38, true), None);
        assert_eq!(
            state.handle_hotkey_event(0x0f, true, false, true),
            Some(HotKeyAction::SwitchApps { reverse: false })
        );
        assert_eq!(
            state.handle_hotkey_event(0x0f, true, false, true),
            Some(HotKeyAction::Consume)
        );
        assert_eq!(
            state.handle_modifier_event(0x38, false),
            Some(HotKeyAction::SwitchAppsDone)
        );
    }

    #[test]
    fn app_switch_allows_a_new_press_after_key_release() {
        let mut state = switch_apps_state();

        assert_eq!(state.handle_modifier_event(0x38, true), None);
        assert_eq!(
            state.handle_hotkey_event(0x0f, true, false, true),
            Some(HotKeyAction::SwitchApps { reverse: false })
        );
        assert_eq!(state.handle_hotkey_event(0x0f, false, false, true), None);
        assert_eq!(
            state.handle_hotkey_event(0x0f, true, true, true),
            Some(HotKeyAction::SwitchApps { reverse: true })
        );
    }

    #[test]
    fn app_switch_cancel_clears_pending_commit() {
        let mut state = switch_apps_state();

        assert_eq!(state.handle_modifier_event(0x38, true), None);
        assert_eq!(
            state.handle_hotkey_event(0x0f, true, false, true),
            Some(HotKeyAction::SwitchApps { reverse: false })
        );
        assert_eq!(
            state.handle_cancel_event(0x01, true),
            Some(HotKeyAction::SwitchAppsCancel)
        );
        assert_eq!(state.handle_modifier_event(0x38, false), None);
    }

    #[test]
    fn switch_windows_only_commits_when_the_switch_was_allowed() {
        let mut state = switch_windows_state();

        assert_eq!(state.handle_modifier_event(0x38, true), None);
        assert_eq!(state.handle_hotkey_event(0x29, true, false, false), None);
        assert_eq!(state.handle_modifier_event(0x38, false), None);
    }

    #[test]
    fn switch_windows_still_commits_on_modifier_release_when_allowed() {
        let mut state = switch_windows_state();

        assert_eq!(state.handle_modifier_event(0x38, true), None);
        assert_eq!(
            state.handle_hotkey_event(0x29, true, false, true),
            Some(HotKeyAction::SwitchWindows { reverse: false })
        );
        assert_eq!(
            state.handle_modifier_event(0x38, false),
            Some(HotKeyAction::SwitchWindowsDone)
        );
    }

    #[test]
    fn actions_map_to_the_existing_app_messages() {
        let message = |action| action_message(action).map(|(msg, lparam)| (msg, lparam.0));

        assert_eq!(message(HotKeyAction::Consume), None);
        assert_eq!(
            message(HotKeyAction::SwitchApps { reverse: false }),
            Some((WM_USER_SWITCH_APPS, 0))
        );
        assert_eq!(
            message(HotKeyAction::SwitchApps { reverse: true }),
            Some((WM_USER_SWITCH_APPS, 1))
        );
        assert_eq!(
            message(HotKeyAction::SwitchAppsDone),
            Some((WM_USER_SWITCH_APPS_DONE, 0))
        );
        assert_eq!(
            message(HotKeyAction::SwitchAppsCancel),
            Some((WM_USER_SWITCH_APPS_CANCEL, 0))
        );
        assert_eq!(
            message(HotKeyAction::SwitchWindows { reverse: true }),
            Some((WM_USER_SWITCH_WINDOWS, 1))
        );
        assert_eq!(
            message(HotKeyAction::SwitchWindowsDone),
            Some((WM_USER_SWITCH_WINDOWS_DONE, 0))
        );
    }
}
