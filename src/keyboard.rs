use crate::{
    app::{
        WM_USER_SWITCH_APPS, WM_USER_SWITCH_APPS_CANCEL, WM_USER_SWITCH_APPS_DONE,
        WM_USER_SWITCH_WINDOWS, WM_USER_SWITCH_WINDOWS_DONE,
    },
    config::{Hotkey, SWITCH_APPS_HOTKEY_ID, SWITCH_WINDOWS_HOTKEY_ID},
    foreground::IS_FOREGROUND_IN_BLACKLIST,
};

use anyhow::{anyhow, bail, Result};
use parking_lot::Mutex;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, RecvTimeoutError},
    LazyLock,
};
use std::thread::JoinHandle;
use std::time::Duration;
use windows::Win32::{
    Foundation::{GetLastError, HWND, LPARAM, LRESULT, WPARAM},
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::{
        Input::KeyboardAndMouse::{
            SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
            SCANCODE_LSHIFT, SCANCODE_RSHIFT, VIRTUAL_KEY,
        },
        WindowsAndMessaging::{
            CallNextHookEx, DispatchMessageW, GetMessageW, PeekMessageW, PostMessageW,
            PostThreadMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
            HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_UP, MSG, PM_NOREMOVE, WH_KEYBOARD_LL, WM_QUIT,
        },
    },
};

/// Unassigned virtual key injected while a modifier is held so the foreground
/// app never sees a bare modifier press/release (Alt alone activates menu bars,
/// Win alone opens Start). Same technique as AutoHotkey's menu mask.
const VK_MASK: u16 = 0xE8;

/// `dwExtraInfo` tag on input injected by this module, so the hook skips it.
const OWN_INJECTION_MARKER: usize = 0x574F_524B;

/// How long `KeyboardListener::drop` waits for the hook thread to exit.
const HOOK_THREAD_STOP_TIMEOUT: Duration = Duration::from_secs(1);

/// Hook state. Only `keyboard_proc` (on the hook thread) touches it after
/// `KeyboardListener::init` has installed it.
static KEYBOARD_STATE: LazyLock<Mutex<KeyboardState>> =
    LazyLock::new(|| Mutex::new(KeyboardState::new(HWND::default(), &[])));

/// At most one listener may own `KEYBOARD_STATE` and the hook at a time.
static LISTENER_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Owns the dedicated thread that installs `WH_KEYBOARD_LL` and pumps its
/// messages. Low-level hook callbacks run on the installing thread, so keeping
/// them off the UI thread means overlay painting can never stall global input.
///
/// The hook never waits on the UI thread: actions are posted, and ordering of
/// the modifier release no longer matters because the hook masks the modifier
/// as soon as it consumes a hotkey press (see `VK_MASK`).
#[derive(Debug)]
pub struct KeyboardListener {
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
    /// Disconnects when the hook thread finishes (its sender is dropped).
    exited: Receiver<()>,
}

impl KeyboardListener {
    pub fn init(hwnd: HWND, hotkeys: &[&Hotkey]) -> Result<Self> {
        if LISTENER_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            bail!("Keyboard listener is already running");
        }
        match Self::start(hwnd, hotkeys) {
            Ok(listener) => Ok(listener),
            Err(err) => {
                LISTENER_ACTIVE.store(false, Ordering::Release);
                Err(err)
            }
        }
    }

    fn start(hwnd: HWND, hotkeys: &[&Hotkey]) -> Result<Self> {
        // Fresh state on every start, so nothing leaks across restarts.
        *KEYBOARD_STATE.lock() = KeyboardState::new(hwnd, hotkeys);

        let (ready_tx, ready_rx) = mpsc::channel::<Result<u32>>();
        let (exited_tx, exited_rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("keyboard-hook".into())
            .spawn(move || {
                let _exited = exited_tx;
                run_hook_thread(ready_tx);
            })
            .map_err(|err| anyhow!("Failed to spawn keyboard hook thread, {err}"))?;

        let thread_id = match ready_rx.recv() {
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
            exited: exited_rx,
        })
    }
}

impl Drop for KeyboardListener {
    fn drop(&mut self) {
        debug!("keyboard listener destroyed");
        if let Err(err) =
            unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) }
        {
            // The thread may already have exited; the bounded wait covers both.
            error!("failed to signal keyboard hook thread to stop, {err}");
        }
        if wait_for_thread_exit(&self.exited, HOOK_THREAD_STOP_TIMEOUT) {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            LISTENER_ACTIVE.store(false, Ordering::Release);
        } else {
            // Never block shutdown indefinitely. The detached thread still owns
            // the hook, so the listener slot stays taken.
            error!("keyboard hook thread did not stop in time; detaching it");
        }
    }
}

/// Returns true once the hook thread has finished, waiting at most `timeout`.
fn wait_for_thread_exit(exited: &Receiver<()>, timeout: Duration) -> bool {
    match exited.recv_timeout(timeout) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => true,
        Err(RecvTimeoutError::Timeout) => false,
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
    loop {
        match unsafe { GetMessageW(&mut message, None, 0, 0) }.0 {
            0 => break,
            -1 => {
                error!("keyboard hook message loop failed, {:?}", unsafe {
                    GetLastError()
                });
                break;
            }
            _ => unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            },
        }
    }

    // Unhook on the thread that installed the hook.
    if let Err(err) = unsafe { UnhookWindowsHookEx(hook) } {
        error!("failed to remove keyboard hook, {err}");
    }
    debug!("keyboard hook thread exited");
}

/// One physical key event, as seen by the hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KeyEvent {
    scan_code: u32,
    is_pressed: bool,
}

impl KeyEvent {
    /// Returns `None` for input this module injected itself.
    fn from_hook_data(data: &KBDLLHOOKSTRUCT) -> Option<Self> {
        if data.dwExtraInfo == OWN_INJECTION_MARKER {
            return None;
        }
        Some(Self {
            scan_code: data.scanCode,
            is_pressed: data.flags.0 & LLKHF_UP.0 == 0,
        })
    }
}

/// What the hook does with the current event once state has been updated.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct HookOutcome {
    /// Swallow the event instead of passing it to the next hook.
    consume: bool,
    /// Inject the modifier mask after releasing the state lock.
    inject_mask: bool,
}

/// Delivers actions to the UI thread. Returns false if delivery failed.
trait ActionSink {
    fn post(&mut self, action: HotKeyAction) -> bool;
}

struct WindowSink(HWND);

impl ActionSink for WindowSink {
    fn post(&mut self, action: HotKeyAction) -> bool {
        let Some((message, lparam)) = action_message(action) else {
            return true;
        };
        match unsafe { PostMessageW(Some(self.0), message, WPARAM(0), lparam) } {
            Ok(()) => true,
            Err(err) => {
                error!("failed to post keyboard action {action:?}, {err}");
                false
            }
        }
    }
}

#[derive(Debug)]
struct KeyboardState {
    /// App window that receives posted switch messages, stored as an integer
    /// because `HWND` is not `Send`.
    window: isize,
    is_shift_pressed: bool,
    hotkeys: Vec<HotKeyState>,
}

impl KeyboardState {
    fn new(hwnd: HWND, hotkeys: &[&Hotkey]) -> Self {
        Self {
            window: hwnd.0 as isize,
            is_shift_pressed: false,
            hotkeys: hotkeys
                .iter()
                .map(|hotkey| HotKeyState::new((*hotkey).clone()))
                .collect(),
        }
    }

    /// Updates hotkey state for one event and delivers resulting actions.
    ///
    /// Fail-safe rule: if a switch or cancel cannot be delivered, the hotkey
    /// state is rolled back and the key is passed through, so the hook never
    /// swallows input or expects a commit for work the UI never received.
    fn handle_key(
        &mut self,
        event: KeyEvent,
        is_foreground_in_blacklist: bool,
        sink: &mut impl ActionSink,
    ) -> HookOutcome {
        let mut outcome = HookOutcome::default();
        if [SCANCODE_LSHIFT, SCANCODE_RSHIFT].contains(&event.scan_code) {
            self.is_shift_pressed = event.is_pressed;
        }
        let reverse = self.is_shift_pressed;

        let mut is_modifier = false;
        for state in self.hotkeys.iter_mut() {
            if let Some(action) = state.handle_modifier_event(event.scan_code, event.is_pressed) {
                is_modifier = true;
                // Commit state is already cleared; a failed commit has nothing
                // to roll back and the modifier always passes through.
                sink.post(action);
            } else if state.hotkey.modifier.contains(&event.scan_code) {
                is_modifier = true;
            }
        }
        if is_modifier {
            return outcome;
        }

        for state in self.hotkeys.iter_mut() {
            let saved = (
                state.is_hotkey_pressed,
                state.should_commit_on_modifier_release,
            );
            let allow_switch =
                state.hotkey.id != SWITCH_WINDOWS_HOTKEY_ID || !is_foreground_in_blacklist;
            let action = state
                .handle_hotkey_event(event.scan_code, event.is_pressed, reverse, allow_switch)
                .or_else(|| state.handle_cancel_event(event.scan_code, event.is_pressed));
            let Some(action) = action else {
                continue;
            };
            if !sink.post(action) {
                // Undo this press so a key repeat retries instead of being
                // swallowed as a repeat of a switch that never happened.
                (
                    state.is_hotkey_pressed,
                    state.should_commit_on_modifier_release,
                ) = saved;
                return outcome;
            }
            outcome.consume = true;
            if action.is_switch() && !state.is_modifier_masked {
                state.is_modifier_masked = true;
                outcome.inject_mask = true;
            }
            return outcome;
        }
        outcome
    }
}

#[derive(Debug)]
struct HotKeyState {
    hotkey: Hotkey,
    is_modifier_pressed: bool,
    is_hotkey_pressed: bool,
    should_commit_on_modifier_release: bool,
    /// Whether the mask key was already injected during this modifier hold.
    is_modifier_masked: bool,
}

impl HotKeyState {
    fn new(hotkey: Hotkey) -> Self {
        Self {
            hotkey,
            is_modifier_pressed: false,
            is_hotkey_pressed: false,
            should_commit_on_modifier_release: false,
            is_modifier_masked: false,
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
            if !self.is_modifier_pressed {
                self.is_modifier_masked = false;
            }
            self.is_modifier_pressed = true;
            return None;
        }

        self.is_modifier_pressed = false;
        self.is_hotkey_pressed = false;
        self.is_modifier_masked = false;
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

impl HotKeyAction {
    fn is_switch(self) -> bool {
        matches!(self, Self::SwitchApps { .. } | Self::SwitchWindows { .. })
    }
}

unsafe extern "system" fn keyboard_proc(code: i32, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
    if code != HC_ACTION as i32 {
        return CallNextHookEx(None, code, w_param, l_param);
    }
    let kbd_data: &KBDLLHOOKSTRUCT = &*(l_param.0 as *const _);
    debug!("keyboard {kbd_data:?}");
    let Some(event) = KeyEvent::from_hook_data(kbd_data) else {
        return CallNextHookEx(None, code, w_param, l_param);
    };
    let is_foreground_in_blacklist = IS_FOREGROUND_IN_BLACKLIST.load(Ordering::Relaxed);
    let outcome = {
        let mut keyboard_state = KEYBOARD_STATE.lock();
        let mut sink = WindowSink(HWND(keyboard_state.window as _));
        keyboard_state.handle_key(event, is_foreground_in_blacklist, &mut sink)
    };
    // Injected outside the lock. The mask lands in the input stream ahead of
    // the later physical modifier release.
    if outcome.inject_mask {
        inject_mask_key();
    }
    if outcome.consume {
        return LRESULT(1);
    }
    CallNextHookEx(None, code, w_param, l_param)
}

fn inject_mask_key() {
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(VK_MASK),
                dwFlags: flags,
                dwExtraInfo: OWN_INJECTION_MARKER,
                ..Default::default()
            },
        },
    };
    let inputs = [key(Default::default()), key(KEYEVENTF_KEYUP)];
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent != inputs.len() as u32 {
        warn!("failed to inject modifier mask key, sent {sent} of 2");
    }
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

#[cfg(test)]
mod tests {
    use super::{
        action_message, wait_for_thread_exit, ActionSink, HookOutcome, HotKeyAction, HotKeyState,
        KeyEvent, KeyboardState, OWN_INJECTION_MARKER,
    };
    use crate::app::{
        WM_USER_SWITCH_APPS, WM_USER_SWITCH_APPS_CANCEL, WM_USER_SWITCH_APPS_DONE,
        WM_USER_SWITCH_WINDOWS, WM_USER_SWITCH_WINDOWS_DONE,
    };
    use crate::config::{Hotkey, SWITCH_APPS_HOTKEY_ID, SWITCH_WINDOWS_HOTKEY_ID};
    use std::sync::mpsc;
    use std::time::Duration;
    use windows::Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{KBDLLHOOKSTRUCT, KBDLLHOOKSTRUCT_FLAGS, LLKHF_UP},
    };

    const ALT: u32 = 0x38;
    const SHIFT: u32 = 0x2a;
    const TAB: u32 = 0x0f;
    const BACKTICK: u32 = 0x29;
    const ESC: u32 = 0x01;
    const A: u32 = 0x1e;

    /// Records posted actions; fails delivery for actions matching `fail`.
    #[derive(Default)]
    struct FakeSink {
        posted: Vec<HotKeyAction>,
        fail: Option<fn(HotKeyAction) -> bool>,
    }

    impl ActionSink for FakeSink {
        fn post(&mut self, action: HotKeyAction) -> bool {
            if self.fail.is_some_and(|fail| fail(action)) {
                return false;
            }
            self.posted.push(action);
            true
        }
    }

    fn keyboard_state() -> KeyboardState {
        let apps = Hotkey::create(SWITCH_APPS_HOTKEY_ID, "switch apps", "alt+tab")
            .expect("switch-apps hotkey should parse");
        let windows = Hotkey::create(SWITCH_WINDOWS_HOTKEY_ID, "switch windows", "alt+`")
            .expect("switch-windows hotkey should parse");
        KeyboardState::new(HWND::default(), &[&apps, &windows])
    }

    fn down(scan_code: u32) -> KeyEvent {
        KeyEvent {
            scan_code,
            is_pressed: true,
        }
    }

    fn up(scan_code: u32) -> KeyEvent {
        KeyEvent {
            scan_code,
            is_pressed: false,
        }
    }

    const PASS: HookOutcome = HookOutcome {
        consume: false,
        inject_mask: false,
    };
    const CONSUME: HookOutcome = HookOutcome {
        consume: true,
        inject_mask: false,
    };
    const CONSUME_AND_MASK: HookOutcome = HookOutcome {
        consume: true,
        inject_mask: true,
    };

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

    #[test]
    fn alt_tab_posts_switch_masks_once_and_commits_on_release() {
        let mut state = keyboard_state();
        let mut sink = FakeSink::default();

        assert_eq!(state.handle_key(down(ALT), false, &mut sink), PASS);
        assert_eq!(
            state.handle_key(down(TAB), false, &mut sink),
            CONSUME_AND_MASK
        );
        assert_eq!(state.handle_key(up(TAB), false, &mut sink), PASS);
        // Further presses in the same hold do not mask again.
        assert_eq!(state.handle_key(down(TAB), false, &mut sink), CONSUME);
        assert_eq!(state.handle_key(up(ALT), false, &mut sink), PASS);
        assert_eq!(
            sink.posted,
            vec![
                HotKeyAction::SwitchApps { reverse: false },
                HotKeyAction::SwitchApps { reverse: false },
                HotKeyAction::SwitchAppsDone,
            ]
        );
    }

    #[test]
    fn mask_is_injected_again_on_the_next_modifier_hold() {
        let mut state = keyboard_state();
        let mut sink = FakeSink::default();

        state.handle_key(down(ALT), false, &mut sink);
        assert_eq!(
            state.handle_key(down(TAB), false, &mut sink),
            CONSUME_AND_MASK
        );
        state.handle_key(up(TAB), false, &mut sink);
        state.handle_key(up(ALT), false, &mut sink);

        state.handle_key(down(ALT), false, &mut sink);
        assert_eq!(
            state.handle_key(down(TAB), false, &mut sink),
            CONSUME_AND_MASK
        );
    }

    #[test]
    fn held_tab_repeat_is_consumed_without_posting() {
        let mut state = keyboard_state();
        let mut sink = FakeSink::default();

        state.handle_key(down(ALT), false, &mut sink);
        state.handle_key(down(TAB), false, &mut sink);
        assert_eq!(state.handle_key(down(TAB), false, &mut sink), CONSUME);
        assert_eq!(state.handle_key(down(TAB), false, &mut sink), CONSUME);
        assert_eq!(
            sink.posted,
            vec![HotKeyAction::SwitchApps { reverse: false }]
        );
    }

    #[test]
    fn shift_reverses_switches() {
        let mut state = keyboard_state();
        let mut sink = FakeSink::default();

        state.handle_key(down(ALT), false, &mut sink);
        state.handle_key(down(SHIFT), false, &mut sink);
        state.handle_key(down(TAB), false, &mut sink);
        state.handle_key(up(TAB), false, &mut sink);
        state.handle_key(down(BACKTICK), false, &mut sink);
        assert_eq!(
            sink.posted,
            vec![
                HotKeyAction::SwitchApps { reverse: true },
                HotKeyAction::SwitchWindows { reverse: true },
            ]
        );
    }

    #[test]
    fn escape_cancels_and_release_then_does_not_commit() {
        let mut state = keyboard_state();
        let mut sink = FakeSink::default();

        state.handle_key(down(ALT), false, &mut sink);
        state.handle_key(down(TAB), false, &mut sink);
        assert_eq!(state.handle_key(down(ESC), false, &mut sink), CONSUME);
        assert_eq!(state.handle_key(up(ALT), false, &mut sink), PASS);
        assert_eq!(
            sink.posted,
            vec![
                HotKeyAction::SwitchApps { reverse: false },
                HotKeyAction::SwitchAppsCancel,
            ]
        );
    }

    #[test]
    fn unrelated_keys_and_bare_modifier_pass_through() {
        let mut state = keyboard_state();
        let mut sink = FakeSink::default();

        assert_eq!(state.handle_key(down(A), false, &mut sink), PASS);
        assert_eq!(state.handle_key(down(ALT), false, &mut sink), PASS);
        assert_eq!(state.handle_key(down(A), false, &mut sink), PASS);
        assert_eq!(state.handle_key(up(ALT), false, &mut sink), PASS);
        assert!(sink.posted.is_empty());
    }

    #[test]
    fn blacklisted_foreground_passes_switch_windows_through() {
        let mut state = keyboard_state();
        let mut sink = FakeSink::default();

        state.handle_key(down(ALT), true, &mut sink);
        assert_eq!(state.handle_key(down(BACKTICK), true, &mut sink), PASS);
        assert_eq!(state.handle_key(up(ALT), true, &mut sink), PASS);
        assert!(sink.posted.is_empty());
    }

    #[test]
    fn failed_switch_post_passes_key_through_and_leaves_no_pending_commit() {
        let mut state = keyboard_state();
        let mut sink = FakeSink {
            fail: Some(|action| action.is_switch()),
            ..Default::default()
        };

        state.handle_key(down(ALT), false, &mut sink);
        assert_eq!(state.handle_key(down(TAB), false, &mut sink), PASS);
        // A key repeat is not swallowed as a repeat of the failed switch.
        assert_eq!(state.handle_key(down(TAB), false, &mut sink), PASS);
        assert_eq!(state.handle_key(up(ALT), false, &mut sink), PASS);
        assert!(sink.posted.is_empty());
    }

    #[test]
    fn key_repeat_retries_a_switch_whose_post_failed() {
        let mut state = keyboard_state();
        let mut sink = FakeSink {
            fail: Some(|action| action.is_switch()),
            ..Default::default()
        };

        state.handle_key(down(ALT), false, &mut sink);
        assert_eq!(state.handle_key(down(TAB), false, &mut sink), PASS);
        sink.fail = None;
        assert_eq!(
            state.handle_key(down(TAB), false, &mut sink),
            CONSUME_AND_MASK
        );
        state.handle_key(up(ALT), false, &mut sink);
        assert_eq!(
            sink.posted,
            vec![
                HotKeyAction::SwitchApps { reverse: false },
                HotKeyAction::SwitchAppsDone,
            ]
        );
    }

    #[test]
    fn failed_later_switch_post_keeps_the_earlier_pending_commit() {
        let mut state = keyboard_state();
        let mut sink = FakeSink::default();

        state.handle_key(down(ALT), false, &mut sink);
        state.handle_key(down(TAB), false, &mut sink);
        state.handle_key(up(TAB), false, &mut sink);
        sink.fail = Some(|action| action.is_switch());
        assert_eq!(state.handle_key(down(TAB), false, &mut sink), PASS);
        sink.fail = None;
        state.handle_key(up(ALT), false, &mut sink);
        assert_eq!(
            sink.posted,
            vec![
                HotKeyAction::SwitchApps { reverse: false },
                HotKeyAction::SwitchAppsDone,
            ]
        );
    }

    #[test]
    fn failed_cancel_post_passes_escape_through_and_keeps_the_commit() {
        let mut state = keyboard_state();
        let mut sink = FakeSink::default();

        state.handle_key(down(ALT), false, &mut sink);
        state.handle_key(down(TAB), false, &mut sink);
        sink.fail = Some(|action| action == HotKeyAction::SwitchAppsCancel);
        assert_eq!(state.handle_key(down(ESC), false, &mut sink), PASS);
        state.handle_key(up(ALT), false, &mut sink);
        assert_eq!(
            sink.posted,
            vec![
                HotKeyAction::SwitchApps { reverse: false },
                HotKeyAction::SwitchAppsDone,
            ]
        );
    }

    #[test]
    fn failed_commit_post_still_resets_state_for_the_next_hold() {
        let mut state = keyboard_state();
        let mut sink = FakeSink {
            fail: Some(|action| action == HotKeyAction::SwitchAppsDone),
            ..Default::default()
        };

        state.handle_key(down(ALT), false, &mut sink);
        state.handle_key(down(TAB), false, &mut sink);
        assert_eq!(state.handle_key(up(ALT), false, &mut sink), PASS);
        sink.fail = None;
        sink.posted.clear();

        // A bare Alt press/release afterwards must not replay the lost commit.
        state.handle_key(down(ALT), false, &mut sink);
        state.handle_key(up(ALT), false, &mut sink);
        assert!(sink.posted.is_empty());
    }

    #[test]
    fn restarted_state_has_no_pending_commit_or_modifier() {
        let mut state = keyboard_state();
        let mut sink = FakeSink::default();
        state.handle_key(down(ALT), false, &mut sink);
        state.handle_key(down(SHIFT), false, &mut sink);
        state.handle_key(down(TAB), false, &mut sink);

        // `KeyboardListener::start` replaces the state wholesale.
        let mut state = keyboard_state();
        let mut sink = FakeSink::default();
        assert_eq!(state.handle_key(up(ALT), false, &mut sink), PASS);
        assert_eq!(state.handle_key(down(TAB), false, &mut sink), PASS);
        assert!(sink.posted.is_empty());
    }

    #[test]
    fn own_injected_input_is_ignored_but_other_input_is_processed() {
        let mut data = KBDLLHOOKSTRUCT {
            scanCode: TAB,
            dwExtraInfo: OWN_INJECTION_MARKER,
            ..Default::default()
        };
        assert_eq!(KeyEvent::from_hook_data(&data), None);

        data.dwExtraInfo = 0;
        assert_eq!(KeyEvent::from_hook_data(&data), Some(down(TAB)));

        data.flags = KBDLLHOOKSTRUCT_FLAGS(LLKHF_UP.0);
        assert_eq!(KeyEvent::from_hook_data(&data), Some(up(TAB)));
    }

    #[test]
    fn thread_exit_wait_reports_exited_threads() {
        let (exited_tx, exited_rx) = mpsc::channel::<()>();
        let thread = std::thread::spawn(move || drop(exited_tx));
        thread.join().expect("test thread should finish");
        assert!(wait_for_thread_exit(&exited_rx, Duration::ZERO));
    }

    #[test]
    fn thread_exit_wait_is_bounded_when_the_thread_never_stops() {
        let (_exited_tx, exited_rx) = mpsc::channel::<()>();
        assert!(!wait_for_thread_exit(&exited_rx, Duration::from_millis(10)));
    }
}
