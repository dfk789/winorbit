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
    atomic::{AtomicBool, AtomicIsize, Ordering},
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
            SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
            KEYEVENTF_KEYUP, SCANCODE_LSHIFT, SCANCODE_RSHIFT, VIRTUAL_KEY,
        },
        WindowsAndMessaging::{
            CallNextHookEx, DispatchMessageW, GetMessageW, PeekMessageW, PostMessageW,
            PostThreadMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
            HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, LLKHF_LOWER_IL_INJECTED,
            LLKHF_UP, MSG, PM_NOREMOVE, WH_KEYBOARD_LL, WM_QUIT,
        },
    },
};

/// Unassigned virtual key injected while a modifier is held so the foreground
/// app never sees a bare modifier press/release (Alt alone activates menu bars,
/// Win alone opens Start). Same technique as AutoHotkey's menu mask.
const VK_MASK: u16 = 0xE8;

/// `dwExtraInfo` tag on the mask events this module injects. Provenance only:
/// the hook ignores those events because Windows flags them as injected, the
/// same as any other injected input. Nothing checks this value.
const OWN_INJECTION_MARKER: usize = 0x574F_524B;

/// `KBDLLHOOKSTRUCT` flags Windows sets on synthesized input (`SendInput`,
/// `keybd_event`). `LLKHF_LOWER_IL_INJECTED` (sent from a lower integrity
/// level) is documented to come with `LLKHF_INJECTED`; both are checked so
/// neither alone can get through.
const INJECTED_FLAGS: u32 = LLKHF_INJECTED.0 | LLKHF_LOWER_IL_INJECTED.0;

/// How long `KeyboardListener::drop` waits for the hook thread to exit.
const HOOK_THREAD_STOP_TIMEOUT: Duration = Duration::from_secs(1);

/// Everything the hook thread can reach.
///
/// Ownership invariant: the hook callback touches only this `'static` value
/// (never freed) and the app window's HWND *value*, which it passes to
/// `PostMessageW`. It never dereferences `App`. `App` is reachable only through
/// the window's user data on the UI thread, and `app::take_app` clears that
/// pointer before freeing the `Box<App>`. So a hook thread that is stuck,
/// late, or detached can never reach freed memory: a message it posts is
/// either handled while `App` is alive or dropped/rejected afterwards.
///
/// Quiescing (`target` = 0) is a "stop producing" signal, not the safety gate
/// above. It never waits, so shutdown never depends on the hook finishing.
struct HookShared {
    /// App window the hook posts to, as an integer because `HWND` is not
    /// `Send`. 0 once quiesced.
    target: AtomicIsize,
    state: Mutex<KeyboardState>,
}

impl HookShared {
    fn new() -> Self {
        Self {
            target: AtomicIsize::new(0),
            state: Mutex::new(KeyboardState::new(&[])),
        }
    }

    /// Stops the hook from starting new work for the app. Never blocks. A hook
    /// event that already read the target may still post once; see the
    /// ownership invariant on `HookShared` for why that is safe.
    fn quiesce(&self) {
        self.target.store(0, Ordering::Release);
    }

    /// Hook-thread side of one key event. Never blocks: a contended state lock
    /// passes the key through. Returns true to consume the event.
    fn on_key<S: ActionSink>(
        &self,
        event: KeyEvent,
        is_foreground_in_blacklist: bool,
        sink_for: impl FnOnce(isize) -> S,
        injector: &mut impl KeyInjector,
    ) -> bool {
        let Some(mut state) = self.state.try_lock() else {
            return false;
        };
        // Retried even after quiesce, so the mask key gets every chance to go up.
        state.flush_pending_mask_up(injector);
        let target = self.target.load(Ordering::Acquire);
        if target == 0 {
            return false;
        }
        state.handle_key(
            event,
            is_foreground_in_blacklist,
            &mut sink_for(target),
            injector,
        )
    }
}

static HOOK: LazyLock<HookShared> = LazyLock::new(HookShared::new);

/// At most one listener (or detached hook thread) may own `HOOK` at a time.
static LISTENER_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Owns the dedicated thread that installs `WH_KEYBOARD_LL` and pumps its
/// messages. Low-level hook callbacks run on the installing thread, so keeping
/// them off the UI thread means overlay painting can never stall global input.
///
/// The hook never waits on the UI thread; it only posts. Ordering of the
/// modifier release does not matter because a hold's mask key is in the input
/// stream before the hook consumes any key in that hold (see `VK_MASK`).
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
                HOOK.quiesce();
                LISTENER_ACTIVE.store(false, Ordering::Release);
                Err(err)
            }
        }
    }

    fn start(hwnd: HWND, hotkeys: &[&Hotkey]) -> Result<Self> {
        // Fresh state on every start, so nothing leaks across restarts. This
        // lock is uncontended: holding `LISTENER_ACTIVE` means no hook thread
        // exists, because a detached one keeps the slot taken forever.
        *HOOK.state.lock() = KeyboardState::new(hotkeys);
        HOOK.target.store(hwnd.0 as isize, Ordering::Release);

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

    /// Stops all new keyboard messages to the app window. Never blocks, even
    /// if the hook thread is stuck holding its state. Idempotent.
    pub fn quiesce(&self) {
        HOOK.quiesce();
    }
}

impl Drop for KeyboardListener {
    fn drop(&mut self) {
        debug!("keyboard listener destroyed");
        HOOK.quiesce();
        if let Err(err) =
            unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) }
        {
            // The thread may already have exited; the bounded wait covers both.
            error!("failed to signal keyboard hook thread to stop, {err}");
        }
        let thread_exited = wait_for_thread_exit(&self.exited, HOOK_THREAD_STOP_TIMEOUT);
        if thread_exited {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
        report_mask_cleanup(finish_shutdown(
            &HOOK,
            thread_exited,
            &mut SendInputInjector,
        ));
        if thread_exited {
            LISTENER_ACTIVE.store(false, Ordering::Release);
        } else {
            // Never block shutdown indefinitely. The detached thread is
            // quiesced and only ever touches `HOOK`, which is never freed. It
            // keeps the listener slot so no restart can share its state.
            error!("keyboard hook thread did not stop in time; detaching it");
        }
    }
}

/// Outcome of releasing a mask key whose key-up never went in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaskCleanup {
    /// No mask key-up was pending.
    NotNeeded,
    /// The pending key-up was injected.
    Released,
    /// Injection failed again; vkE8 may stay logically down. Best effort only:
    /// `SendInput` can keep failing (or be blocked by UIPI) and nothing else
    /// can release a key this process pressed.
    StillDown,
    /// The hook thread still holds the state, so cleanup was not attempted.
    Skipped,
}

/// Final mask cleanup once the hook has been quiesced. Bounded: it takes the
/// state lock normally only after the hook thread has exited (nothing else
/// can hold it then) and otherwise only tries it once.
fn finish_shutdown(
    shared: &HookShared,
    thread_exited: bool,
    injector: &mut impl KeyInjector,
) -> MaskCleanup {
    let state = if thread_exited {
        Some(shared.state.lock())
    } else {
        shared.state.try_lock()
    };
    match state {
        Some(mut state) => state.release_pending_mask(injector),
        None => MaskCleanup::Skipped,
    }
}

fn report_mask_cleanup(cleanup: MaskCleanup) {
    match cleanup {
        MaskCleanup::NotNeeded | MaskCleanup::Released => {}
        MaskCleanup::StillDown => {
            error!("mask key vkE8 may still be logically down: its key-up could not be injected")
        }
        MaskCleanup::Skipped => {
            error!("keyboard hook still holds its state; skipped releasing a pending mask key")
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
    /// Only used to tell apart modifier sides that share a scan code.
    is_extended: bool,
    is_pressed: bool,
}

impl KeyEvent {
    /// Returns `None` for any event carrying Windows' injected-input flags,
    /// including this module's own mask key. Every other event is treated as
    /// ordinary keyboard input: the hook only knows what Windows reports.
    fn from_hook_data(data: &KBDLLHOOKSTRUCT) -> Option<Self> {
        if data.flags.0 & INJECTED_FLAGS != 0 {
            return None;
        }
        Some(Self {
            scan_code: data.scanCode,
            is_extended: data.flags.0 & LLKHF_EXTENDED.0 != 0,
            is_pressed: data.flags.0 & LLKHF_UP.0 == 0,
        })
    }
}

/// Bit identifying one physical side of a two-sided key, or `None` if the
/// event is not one of `sides`. Sides with their own scan code (LWin/RWin,
/// LShift/RShift) are told apart by scan code; sides that share one
/// (LAlt/RAlt, LCtrl/RCtrl) by the extended-key flag.
fn side_bit(sides: [u32; 2], event: KeyEvent) -> Option<u8> {
    let index = sides.iter().position(|&code| code == event.scan_code)?;
    Some(1 << (index * 2 + event.is_extended as usize))
}

/// Physical sides of a two-sided key that are currently down, one bit each.
/// Setting or clearing a bit is idempotent, so a repeated key-down or a stray
/// key-up can never corrupt the state the way a counter could.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct SidesDown(u8);

impl SidesDown {
    fn is_down(self) -> bool {
        self.0 != 0
    }

    fn apply(&mut self, bit: u8, is_pressed: bool) {
        if is_pressed {
            self.0 |= bit;
        } else {
            self.0 &= !bit;
        }
    }
}

/// Delivers actions to the UI thread. Returns false if delivery failed.
trait ActionSink {
    fn post(&mut self, action: HotKeyAction) -> bool;
}

impl<T: ActionSink> ActionSink for &mut T {
    fn post(&mut self, action: HotKeyAction) -> bool {
        (**self).post(action)
    }
}

struct WindowSink(HWND);

impl ActionSink for WindowSink {
    fn post(&mut self, action: HotKeyAction) -> bool {
        let Some((message, lparam)) = action_message(action) else {
            // Actions without a UI message have nothing to deliver.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaskKey {
    Down,
    Up,
}

/// Injects mask key events. Returns how many of `keys` were inserted.
///
/// `SendInput` inserts events serially, so a partial result always means a
/// prefix of `keys` went in.
trait KeyInjector {
    fn send(&mut self, keys: &[MaskKey]) -> usize;
}

struct SendInputInjector;

impl KeyInjector for SendInputInjector {
    fn send(&mut self, keys: &[MaskKey]) -> usize {
        let inputs: Vec<INPUT> = keys
            .iter()
            .map(|key| INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(VK_MASK),
                        dwFlags: match key {
                            MaskKey::Down => KEYBD_EVENT_FLAGS(0),
                            MaskKey::Up => KEYEVENTF_KEYUP,
                        },
                        dwExtraInfo: OWN_INJECTION_MARKER,
                        ..Default::default()
                    },
                },
            })
            .collect();
        let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) } as usize;
        if sent != inputs.len() {
            warn!("injected {sent} of {} modifier mask events", inputs.len());
        }
        sent
    }
}

/// State of one physical modifier hold, shared by every hotkey using that
/// modifier so masking and ownership are per hold, not per hotkey.
///
/// A hold starts when the first side goes down, lasts while either side is
/// down, and ends only when the last side goes up. Releasing one side while
/// the other is still down changes nothing else.
#[derive(Debug)]
struct ModifierHold {
    modifier: [u32; 2],
    sides: SidesDown,
    /// The mask key is in the input stream for this hold. Required before
    /// the hook consumes any key while the modifier is held.
    is_masked: bool,
    /// A WinOrbit switch was delivered during this hold, so the hold must not
    /// leak native shortcuts (Alt+Tab, Alt+Esc) into Windows any more.
    is_owned: bool,
}

/// What the UI should be told when the modifier is released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReleaseIntent {
    None,
    Commit,
    Cancel,
}

#[derive(Debug)]
struct KeyboardState {
    shift: SidesDown,
    /// A mask key-down went in without its key-up; retried on every event.
    is_mask_up_pending: bool,
    holds: Vec<ModifierHold>,
    hotkeys: Vec<HotKeyState>,
}

impl KeyboardState {
    fn new(hotkeys: &[&Hotkey]) -> Self {
        let mut holds: Vec<ModifierHold> = vec![];
        let hotkeys = hotkeys
            .iter()
            .map(|hotkey| {
                let hold = match holds.iter().position(|h| h.modifier == hotkey.modifier) {
                    Some(index) => index,
                    None => {
                        holds.push(ModifierHold {
                            modifier: hotkey.modifier,
                            sides: SidesDown::default(),
                            is_masked: false,
                            is_owned: false,
                        });
                        holds.len() - 1
                    }
                };
                HotKeyState::new((*hotkey).clone(), hold)
            })
            .collect();
        Self {
            shift: SidesDown::default(),
            is_mask_up_pending: false,
            holds,
            hotkeys,
        }
    }

    fn flush_pending_mask_up(&mut self, injector: &mut impl KeyInjector) {
        if self.is_mask_up_pending && injector.send(&[MaskKey::Up]) == 1 {
            self.is_mask_up_pending = false;
        }
    }

    /// Last attempt at a pending mask key-up, reporting what happened.
    fn release_pending_mask(&mut self, injector: &mut impl KeyInjector) -> MaskCleanup {
        if !self.is_mask_up_pending {
            return MaskCleanup::NotNeeded;
        }
        self.flush_pending_mask_up(injector);
        if self.is_mask_up_pending {
            MaskCleanup::StillDown
        } else {
            MaskCleanup::Released
        }
    }

    /// Makes sure the hold's mask is in the input stream. Returns false if no
    /// mask event could be inserted, in which case the caller must not consume.
    fn ensure_masked(&mut self, hold: usize, injector: &mut impl KeyInjector) -> bool {
        if self.holds[hold].is_masked {
            return true;
        }
        let inserted = injector.send(&[MaskKey::Down, MaskKey::Up]);
        if inserted == 0 {
            return false;
        }
        // With only the key-down inserted the app already saw modifier+mask, so
        // the hold is masked. The key-up is retried (best effort) until it goes
        // in; see `MaskCleanup::StillDown`.
        if inserted == 1 && injector.send(&[MaskKey::Up]) != 1 {
            self.is_mask_up_pending = true;
        }
        self.holds[hold].is_masked = true;
        true
    }

    /// Updates modifier holds for a modifier event and posts the release
    /// action of every hotkey whose hold just ended. Returns false if the
    /// event is not a configured modifier.
    fn handle_modifier_key(&mut self, event: KeyEvent, sink: &mut impl ActionSink) -> bool {
        let mut is_modifier = false;
        let mut ended_holds = 0u32;
        for (index, hold) in self.holds.iter_mut().enumerate() {
            let Some(bit) = side_bit(hold.modifier, event) else {
                continue;
            };
            is_modifier = true;
            let was_held = hold.sides.is_down();
            hold.sides.apply(bit, event.is_pressed);
            if event.is_pressed && !was_held {
                // First side down: a new hold.
                hold.is_masked = false;
                hold.is_owned = false;
            } else if !event.is_pressed && !hold.sides.is_down() {
                // Last side up: the hold is over.
                hold.is_masked = false;
                hold.is_owned = false;
                ended_holds |= 1 << index;
            }
        }
        for state in self.hotkeys.iter_mut() {
            if ended_holds & (1 << state.hold) == 0 {
                continue;
            }
            if let Some(action) = state.end_hold() {
                // Release state is already cleared; a failed post has nothing
                // to roll back (the UI is unreachable) and is logged by the sink.
                sink.post(action);
            }
        }
        is_modifier
    }

    /// Updates hotkey state for one event, delivering actions and injecting the
    /// mask as needed. Returns true to consume the event.
    ///
    /// Invariants:
    /// - Modifier events are never consumed or injected.
    /// - A key is consumed only after its hold's mask is in the input stream.
    /// - Before a hold is owned, a failed delivery rolls back and passes the
    ///   key through, so Windows handles the shortcut natively.
    /// - Once a hold is owned, failed deliveries still consume the key so
    ///   native and WinOrbit switching never mix within one hold.
    fn handle_key(
        &mut self,
        event: KeyEvent,
        is_foreground_in_blacklist: bool,
        sink: &mut impl ActionSink,
        injector: &mut impl KeyInjector,
    ) -> bool {
        if let Some(bit) = side_bit([SCANCODE_LSHIFT, SCANCODE_RSHIFT], event) {
            self.shift.apply(bit, event.is_pressed);
        }
        let reverse = self.shift.is_down();

        if self.handle_modifier_key(event, sink) {
            return false;
        }

        for index in 0..self.hotkeys.len() {
            let allow_switch = self.hotkeys[index].hotkey.id != SWITCH_WINDOWS_HOTKEY_ID
                || !is_foreground_in_blacklist;
            let hold = self.hotkeys[index].hold;
            let is_held = self.holds[hold].sides.is_down();
            let state = &mut self.hotkeys[index];
            let was_hotkey_pressed = state.is_hotkey_pressed;
            let action = state
                .handle_hotkey_event(event, is_held, reverse, allow_switch)
                .or_else(|| state.handle_cancel_event(event, is_held));
            let Some(action) = action else {
                continue;
            };

            if !self.ensure_masked(hold, injector) {
                // Not masked, so not consuming: Windows gets the key natively.
                self.hotkeys[index].is_hotkey_pressed = was_hotkey_pressed;
                return false;
            }
            if action == HotKeyAction::Consume {
                return true;
            }

            let is_owned = self.holds[hold].is_owned;
            let state = &mut self.hotkeys[index];
            let delivered = sink.post(action);
            match (action, delivered) {
                (HotKeyAction::SwitchAppsCancel, true) => {
                    state.release_intent = ReleaseIntent::None;
                }
                (HotKeyAction::SwitchAppsCancel, false) => {
                    // Cancel requires a delivered switch, so the hold is owned.
                    // Retry the cancel on release instead of committing.
                    state.release_intent = ReleaseIntent::Cancel;
                }
                (_, true) => {
                    state.release_intent = ReleaseIntent::Commit;
                    self.holds[hold].is_owned = true;
                }
                (_, false) if is_owned => {
                    // Keep the WinOrbit session; this press just did not advance.
                }
                (_, false) => {
                    // Undo this press so a key repeat retries delivery instead
                    // of being swallowed as a repeat.
                    state.is_hotkey_pressed = was_hotkey_pressed;
                    return false;
                }
            }
            return true;
        }
        false
    }
}

#[derive(Debug)]
struct HotKeyState {
    hotkey: Hotkey,
    /// Index of this hotkey's modifier in `KeyboardState::holds`; whether the
    /// modifier is held is read from there, never tracked per hotkey.
    hold: usize,
    is_hotkey_pressed: bool,
    release_intent: ReleaseIntent,
}

impl HotKeyState {
    fn new(hotkey: Hotkey, hold: usize) -> Self {
        Self {
            hotkey,
            hold,
            is_hotkey_pressed: false,
            release_intent: ReleaseIntent::None,
        }
    }

    /// Called when the last side of this hotkey's modifier is released.
    fn end_hold(&mut self) -> Option<HotKeyAction> {
        self.is_hotkey_pressed = false;
        let release_intent = self.release_intent;
        self.release_intent = ReleaseIntent::None;

        match (release_intent, self.hotkey.id) {
            (ReleaseIntent::Commit, SWITCH_APPS_HOTKEY_ID) => Some(HotKeyAction::SwitchAppsDone),
            (ReleaseIntent::Commit, SWITCH_WINDOWS_HOTKEY_ID) => {
                Some(HotKeyAction::SwitchWindowsDone)
            }
            (ReleaseIntent::Cancel, SWITCH_APPS_HOTKEY_ID) => Some(HotKeyAction::SwitchAppsCancel),
            _ => None,
        }
    }

    /// Tracks the hotkey's physical state and returns the switch (or repeat
    /// consume) it triggers. Release intent is set by the caller on delivery.
    fn handle_hotkey_event(
        &mut self,
        event: KeyEvent,
        is_modifier_held: bool,
        reverse: bool,
        allow_switch: bool,
    ) -> Option<HotKeyAction> {
        if event.scan_code != self.hotkey.code {
            return None;
        }

        if !event.is_pressed {
            self.is_hotkey_pressed = false;
            return None;
        }

        let is_repeat = self.is_hotkey_pressed;
        self.is_hotkey_pressed = true;

        if !is_modifier_held {
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

        match self.hotkey.id {
            SWITCH_APPS_HOTKEY_ID => Some(HotKeyAction::SwitchApps { reverse }),
            SWITCH_WINDOWS_HOTKEY_ID => Some(HotKeyAction::SwitchWindows { reverse }),
            _ => None,
        }
    }

    /// Esc cancels only while this hold has a WinOrbit switch pending; without
    /// one, Alt+Esc stays a native Windows shortcut.
    fn handle_cancel_event(&self, event: KeyEvent, is_modifier_held: bool) -> Option<HotKeyAction> {
        if self.hotkey.id != SWITCH_APPS_HOTKEY_ID
            || event.scan_code != 0x01
            || !event.is_pressed
            || !is_modifier_held
            || self.release_intent == ReleaseIntent::None
        {
            return None;
        }
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

/// Hook-thread handling of one raw event. Injected input is rejected here,
/// before the state is even locked, so it can never change modifier, Shift or
/// hotkey state, start/advance/cancel/commit a switch, inject a mask, post a
/// UI action, or be consumed: it always passes on to the next hook.
fn handle_hook_data<S: ActionSink>(
    shared: &HookShared,
    data: &KBDLLHOOKSTRUCT,
    is_foreground_in_blacklist: bool,
    sink_for: impl FnOnce(isize) -> S,
    injector: &mut impl KeyInjector,
) -> bool {
    match KeyEvent::from_hook_data(data) {
        Some(event) => shared.on_key(event, is_foreground_in_blacklist, sink_for, injector),
        None => false,
    }
}

unsafe extern "system" fn keyboard_proc(code: i32, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
    if code != HC_ACTION as i32 {
        return CallNextHookEx(None, code, w_param, l_param);
    }
    // No routine logging here: this runs for every key in the system.
    let kbd_data: &KBDLLHOOKSTRUCT = &*(l_param.0 as *const _);
    let is_foreground_in_blacklist = IS_FOREGROUND_IN_BLACKLIST.load(Ordering::Relaxed);
    let consume = handle_hook_data(
        &HOOK,
        kbd_data,
        is_foreground_in_blacklist,
        |target| WindowSink(HWND(target as _)),
        &mut SendInputInjector,
    );
    if consume {
        return LRESULT(1);
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

#[cfg(test)]
mod tests {
    use super::{
        action_message, finish_shutdown, handle_hook_data, wait_for_thread_exit, ActionSink,
        HookShared, HotKeyAction, KeyEvent, KeyInjector, KeyboardListener, KeyboardState,
        MaskCleanup, MaskKey, LISTENER_ACTIVE, OWN_INJECTION_MARKER, VK_MASK,
    };
    use crate::app::{
        WM_USER_SWITCH_APPS, WM_USER_SWITCH_APPS_CANCEL, WM_USER_SWITCH_APPS_DONE,
        WM_USER_SWITCH_WINDOWS, WM_USER_SWITCH_WINDOWS_DONE,
    };
    use crate::config::{Hotkey, SWITCH_APPS_HOTKEY_ID, SWITCH_WINDOWS_HOTKEY_ID};
    use parking_lot::MutexGuard;
    use std::collections::VecDeque;
    use std::sync::{atomic::Ordering, mpsc};
    use std::time::Duration;
    use windows::Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{
            KBDLLHOOKSTRUCT, KBDLLHOOKSTRUCT_FLAGS, LLKHF_EXTENDED, LLKHF_INJECTED,
            LLKHF_LOWER_IL_INJECTED, LLKHF_UP,
        },
    };

    const ALT: u32 = 0x38;
    const SHIFT: u32 = 0x2a;
    const RSHIFT: u32 = 0x36;
    const LWIN: u32 = 0x5b;
    const RWIN: u32 = 0x5c;
    const TAB: u32 = 0x0f;
    const BACKTICK: u32 = 0x29;
    const ESC: u32 = 0x01;
    const A: u32 = 0x1e;

    use HotKeyAction::{
        SwitchApps, SwitchAppsCancel, SwitchAppsDone, SwitchWindows, SwitchWindowsDone,
    };
    use MaskKey::{Down, Up};

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

    /// Records every injection request and the keys actually inserted. Each
    /// call inserts the next scripted count, or everything once the script is
    /// exhausted. With `fail_up`, every mask key-up fails forever.
    #[derive(Default)]
    struct FakeInjector {
        script: VecDeque<usize>,
        fail_up: bool,
        requests: Vec<Vec<MaskKey>>,
        inserted: Vec<MaskKey>,
    }

    impl FakeInjector {
        fn scripted(counts: &[usize]) -> Self {
            Self {
                script: counts.iter().copied().collect(),
                ..Default::default()
            }
        }
    }

    impl KeyInjector for FakeInjector {
        fn send(&mut self, keys: &[MaskKey]) -> usize {
            self.requests.push(keys.to_vec());
            let count = if self.fail_up {
                keys.iter().take_while(|key| **key == Down).count()
            } else {
                self.script
                    .pop_front()
                    .unwrap_or(keys.len())
                    .min(keys.len())
            };
            self.inserted.extend_from_slice(&keys[..count]);
            count
        }
    }

    /// Flag combinations Windows can report for injected input. The last one
    /// is not expected from Windows, but must be rejected all the same.
    const INJECTED_VARIANTS: [u32; 3] = [
        LLKHF_INJECTED.0,
        LLKHF_INJECTED.0 | LLKHF_LOWER_IL_INJECTED.0,
        LLKHF_LOWER_IL_INJECTED.0,
    ];

    fn hook_data(
        scan_code: u32,
        is_extended: bool,
        is_pressed: bool,
        flags: u32,
    ) -> KBDLLHOOKSTRUCT {
        let mut flags = flags;
        if is_extended {
            flags |= LLKHF_EXTENDED.0;
        }
        if !is_pressed {
            flags |= LLKHF_UP.0;
        }
        KBDLLHOOKSTRUCT {
            scanCode: scan_code,
            flags: KBDLLHOOKSTRUCT_FLAGS(flags),
            ..Default::default()
        }
    }

    /// Shared hook state for one listener, driven through the same
    /// `HookShared::on_key` entry point the real hook uses.
    fn shared_with(hotkeys: [&str; 2]) -> HookShared {
        let windows = Hotkey::create(SWITCH_WINDOWS_HOTKEY_ID, "switch windows", hotkeys[0])
            .expect("switch-windows hotkey should parse");
        let apps = Hotkey::create(SWITCH_APPS_HOTKEY_ID, "switch apps", hotkeys[1])
            .expect("switch-apps hotkey should parse");
        let shared = HookShared::new();
        *shared.state.lock() = KeyboardState::new(&[&windows, &apps]);
        shared.target.store(1, Ordering::Release);
        shared
    }

    struct Harness {
        shared: HookShared,
        sink: FakeSink,
        injector: FakeInjector,
        blacklisted: bool,
    }

    impl Harness {
        fn new() -> Self {
            Self::with_hotkeys(["alt+`", "alt+tab"])
        }

        fn with_hotkeys(hotkeys: [&str; 2]) -> Self {
            Self {
                shared: shared_with(hotkeys),
                sink: FakeSink::default(),
                injector: FakeInjector::default(),
                blacklisted: false,
            }
        }

        fn state(&self) -> MutexGuard<'_, KeyboardState> {
            self.shared.state.lock()
        }

        /// Feeds one raw hook event through the same entry point as the real
        /// hook, including the injected-input filter.
        fn raw(&mut self, data: KBDLLHOOKSTRUCT) -> bool {
            let sink = &mut self.sink;
            handle_hook_data(
                &self.shared,
                &data,
                self.blacklisted,
                move |_| sink,
                &mut self.injector,
            )
        }

        /// A physical key event.
        fn event(&mut self, scan_code: u32, is_extended: bool, is_pressed: bool) -> bool {
            self.raw(hook_data(scan_code, is_extended, is_pressed, 0))
        }

        /// An event Windows flagged as injected (`LLKHF_INJECTED`).
        fn injected(&mut self, scan_code: u32, is_extended: bool, is_pressed: bool) -> bool {
            self.raw(hook_data(
                scan_code,
                is_extended,
                is_pressed,
                LLKHF_INJECTED.0,
            ))
        }

        /// Debug rendering of all hook state, to prove an event changed nothing.
        fn snapshot(&self) -> String {
            format!("{:?}", *self.state())
        }

        fn key(&mut self, scan_code: u32, is_pressed: bool) -> bool {
            self.event(scan_code, false, is_pressed)
        }

        fn down(&mut self, scan_code: u32) -> bool {
            self.key(scan_code, true)
        }

        fn up(&mut self, scan_code: u32) -> bool {
            self.key(scan_code, false)
        }

        /// Key-down of an extended key (RAlt, RCtrl, both Win keys).
        fn down_ext(&mut self, scan_code: u32) -> bool {
            self.event(scan_code, true, true)
        }

        fn up_ext(&mut self, scan_code: u32) -> bool {
            self.event(scan_code, true, false)
        }

        fn tap(&mut self, scan_code: u32) -> bool {
            let consumed = self.down(scan_code);
            self.up(scan_code);
            consumed
        }

        fn fail_posts(&mut self, fail: fn(HotKeyAction) -> bool) {
            self.sink.fail = Some(fail);
        }

        fn deliver_posts(&mut self) {
            self.sink.fail = None;
        }
    }

    // Normal switching

    #[test]
    fn alt_tab_masks_before_switching_and_commits_on_release() {
        let mut h = Harness::new();

        assert!(!h.down(ALT));
        assert!(h.tap(TAB));
        assert!(h.tap(TAB));
        assert!(!h.up(ALT));
        assert_eq!(
            h.sink.posted,
            vec![
                SwitchApps { reverse: false },
                SwitchApps { reverse: false },
                SwitchAppsDone,
            ]
        );
        assert_eq!(h.injector.requests, vec![vec![Down, Up]]);
    }

    #[test]
    fn mask_is_injected_once_per_hold_across_hotkeys() {
        let mut h = Harness::new();

        h.down(ALT);
        assert!(h.tap(TAB));
        assert!(h.tap(BACKTICK));
        assert!(h.tap(TAB));
        h.up(ALT);
        assert_eq!(h.injector.requests, vec![vec![Down, Up]]);
        assert_eq!(
            h.sink.posted,
            vec![
                SwitchApps { reverse: false },
                SwitchWindows { reverse: false },
                SwitchApps { reverse: false },
                SwitchWindowsDone,
                SwitchAppsDone,
            ]
        );
    }

    #[test]
    fn mask_is_injected_again_on_the_next_hold() {
        let mut h = Harness::new();

        h.down(ALT);
        h.tap(TAB);
        h.up(ALT);
        h.down(ALT);
        h.tap(TAB);
        h.up(ALT);
        assert_eq!(h.injector.requests, vec![vec![Down, Up], vec![Down, Up]]);
    }

    #[test]
    fn modifier_auto_repeat_does_not_start_a_new_hold() {
        let mut h = Harness::new();

        h.down(ALT);
        h.tap(TAB);
        h.down(ALT);
        h.down(ALT);
        h.tap(TAB);
        assert_eq!(h.injector.requests, vec![vec![Down, Up]]);
    }

    #[test]
    fn modifier_events_are_never_consumed() {
        let mut h = Harness::new();

        assert!(!h.down(ALT));
        h.tap(TAB);
        assert!(!h.down(ALT));
        assert!(!h.up(ALT));
    }

    #[test]
    fn held_tab_repeat_is_consumed_without_posting() {
        let mut h = Harness::new();

        h.down(ALT);
        assert!(h.down(TAB));
        assert!(h.down(TAB));
        assert!(h.down(TAB));
        assert_eq!(h.sink.posted, vec![SwitchApps { reverse: false }]);
    }

    #[test]
    fn app_switch_allows_a_new_press_after_key_release() {
        let mut h = Harness::new();

        h.down(ALT);
        h.tap(TAB);
        h.down(SHIFT);
        h.tap(TAB);
        assert_eq!(
            h.sink.posted,
            vec![SwitchApps { reverse: false }, SwitchApps { reverse: true }]
        );
    }

    #[test]
    fn shift_reverses_both_hotkeys() {
        let mut h = Harness::new();

        h.down(ALT);
        h.down(SHIFT);
        h.tap(TAB);
        h.tap(BACKTICK);
        assert_eq!(
            h.sink.posted,
            vec![
                SwitchApps { reverse: true },
                SwitchWindows { reverse: true }
            ]
        );
    }

    #[test]
    fn unrelated_keys_and_bare_modifier_pass_through_unmasked() {
        let mut h = Harness::new();

        assert!(!h.tap(A));
        h.down(ALT);
        assert!(!h.tap(A));
        h.up(ALT);
        assert!(h.sink.posted.is_empty());
        assert!(h.injector.requests.is_empty());
    }

    #[test]
    fn blacklisted_foreground_passes_switch_windows_through() {
        let mut h = Harness::new();
        h.blacklisted = true;

        h.down(ALT);
        assert!(!h.tap(BACKTICK));
        assert!(!h.up(ALT));
        assert!(h.sink.posted.is_empty());
        assert!(h.injector.requests.is_empty());
    }

    #[test]
    fn switch_windows_still_commits_on_release() {
        let mut h = Harness::new();

        h.down(ALT);
        assert!(h.tap(BACKTICK));
        h.up(ALT);
        assert_eq!(
            h.sink.posted,
            vec![SwitchWindows { reverse: false }, SwitchWindowsDone]
        );
    }

    // Cancel

    #[test]
    fn escape_cancels_and_release_then_does_not_commit() {
        let mut h = Harness::new();

        h.down(ALT);
        h.tap(TAB);
        assert!(h.tap(ESC));
        assert!(!h.up(ALT));
        assert_eq!(
            h.sink.posted,
            vec![SwitchApps { reverse: false }, SwitchAppsCancel]
        );
    }

    #[test]
    fn alt_escape_without_a_switch_stays_native() {
        let mut h = Harness::new();

        h.down(ALT);
        assert!(!h.tap(ESC));
        h.up(ALT);
        assert!(h.sink.posted.is_empty());
        assert!(h.injector.requests.is_empty());
    }

    #[test]
    fn escape_after_a_cancel_in_the_same_hold_stays_native() {
        let mut h = Harness::new();

        h.down(ALT);
        h.tap(TAB);
        assert!(h.tap(ESC));
        assert!(!h.tap(ESC));
    }

    #[test]
    fn tab_after_cancel_starts_a_new_switch_that_commits() {
        let mut h = Harness::new();

        h.down(ALT);
        h.tap(TAB);
        h.tap(ESC);
        assert!(h.tap(TAB));
        h.up(ALT);
        assert_eq!(
            h.sink.posted,
            vec![
                SwitchApps { reverse: false },
                SwitchAppsCancel,
                SwitchApps { reverse: false },
                SwitchAppsDone,
            ]
        );
    }

    // Delivery failures

    #[test]
    fn first_switch_post_failure_falls_back_to_native() {
        let mut h = Harness::new();
        h.fail_posts(|action| action == SwitchApps { reverse: false });

        h.down(ALT);
        assert!(!h.down(TAB));
        // The repeat retries delivery instead of being swallowed as a repeat.
        assert!(!h.down(TAB));
        h.up(TAB);
        assert!(!h.up(ALT));
        assert!(h.sink.posted.is_empty());
    }

    #[test]
    fn key_repeat_retries_a_first_switch_whose_post_failed() {
        let mut h = Harness::new();
        h.fail_posts(|action| action == SwitchApps { reverse: false });

        h.down(ALT);
        assert!(!h.down(TAB));
        h.deliver_posts();
        assert!(h.down(TAB));
        h.up(TAB);
        h.up(ALT);
        assert_eq!(
            h.sink.posted,
            vec![SwitchApps { reverse: false }, SwitchAppsDone]
        );
    }

    #[test]
    fn later_switch_post_failure_stays_inside_the_winorbit_session() {
        let mut h = Harness::new();

        h.down(ALT);
        h.tap(TAB);
        h.fail_posts(|action| action == SwitchApps { reverse: false });
        // Consumed: native Alt+Tab must not open during a WinOrbit session.
        assert!(h.down(TAB));
        // Further repeats of that physical press are plain repeats.
        assert!(h.down(TAB));
        h.up(TAB);
        h.deliver_posts();
        h.up(ALT);
        assert_eq!(
            h.sink.posted,
            vec![SwitchApps { reverse: false }, SwitchAppsDone]
        );
    }

    #[test]
    fn switch_windows_failure_inside_an_app_session_is_consumed() {
        let mut h = Harness::new();

        h.down(ALT);
        h.tap(TAB);
        h.fail_posts(|action| matches!(action, SwitchWindows { .. }));
        assert!(h.tap(BACKTICK));
        h.deliver_posts();
        h.up(ALT);
        assert_eq!(
            h.sink.posted,
            vec![SwitchApps { reverse: false }, SwitchAppsDone]
        );
    }

    #[test]
    fn cancel_failure_is_consumed_and_retried_on_release() {
        let mut h = Harness::new();

        h.down(ALT);
        h.tap(TAB);
        h.fail_posts(|action| action == SwitchAppsCancel);
        assert!(h.tap(ESC));
        h.deliver_posts();
        assert!(!h.up(ALT));
        assert_eq!(
            h.sink.posted,
            vec![SwitchApps { reverse: false }, SwitchAppsCancel]
        );
    }

    #[test]
    fn switch_after_failed_cancel_commits_again() {
        let mut h = Harness::new();

        h.down(ALT);
        h.tap(TAB);
        h.fail_posts(|action| action == SwitchAppsCancel);
        h.tap(ESC);
        h.deliver_posts();
        h.tap(TAB);
        h.up(ALT);
        assert_eq!(
            h.sink.posted,
            vec![
                SwitchApps { reverse: false },
                SwitchApps { reverse: false },
                SwitchAppsDone,
            ]
        );
    }

    #[test]
    fn commit_failure_leaves_no_pending_state() {
        let mut h = Harness::new();

        h.down(ALT);
        h.tap(TAB);
        h.fail_posts(|action| action == SwitchAppsDone);
        assert!(!h.up(ALT));
        h.deliver_posts();
        h.sink.posted.clear();

        // A later bare Alt press must not replay the lost commit.
        h.down(ALT);
        h.up(ALT);
        assert!(h.sink.posted.is_empty());
    }

    // Mask injection outcomes

    #[test]
    fn zero_mask_events_inserted_passes_the_key_through_without_posting() {
        let mut h = Harness::new();
        h.injector = FakeInjector::scripted(&[0]);

        h.down(ALT);
        assert!(!h.down(TAB));
        h.up(TAB);
        assert!(!h.up(ALT));
        assert!(h.sink.posted.is_empty());
    }

    #[test]
    fn zero_mask_events_inserted_then_retry_succeeds_on_the_next_press() {
        let mut h = Harness::new();
        h.injector = FakeInjector::scripted(&[0]);

        h.down(ALT);
        assert!(!h.tap(TAB));
        assert!(h.tap(TAB));
        h.up(ALT);
        assert_eq!(
            h.sink.posted,
            vec![SwitchApps { reverse: false }, SwitchAppsDone]
        );
        assert_eq!(h.injector.inserted, vec![Down, Up]);
    }

    #[test]
    fn zero_mask_events_on_a_repeat_passes_the_repeat_through() {
        let mut h = Harness::new();
        // Tab held before Alt: the first repeat under Alt is a consume, which
        // also needs the mask first.
        h.injector = FakeInjector::scripted(&[0]);

        h.down(TAB);
        h.down(ALT);
        assert!(!h.down(TAB));
        assert!(h.sink.posted.is_empty());
    }

    #[test]
    fn partial_mask_retries_the_key_up_immediately() {
        let mut h = Harness::new();
        h.injector = FakeInjector::scripted(&[1, 1]);

        h.down(ALT);
        assert!(h.tap(TAB));
        assert_eq!(h.injector.inserted, vec![Down, Up]);
        assert_eq!(h.injector.requests, vec![vec![Down, Up], vec![Up]]);
        assert!(!h.state().is_mask_up_pending);
    }

    #[test]
    fn partial_mask_keeps_retrying_the_key_up_until_it_goes_in() {
        let mut h = Harness::new();
        h.injector = FakeInjector::scripted(&[1, 0, 0, 1]);

        h.down(ALT);
        // The hold counts as masked: the app already saw Alt+mask.
        assert!(h.down(TAB));
        assert!(h.state().is_mask_up_pending);
        h.up(TAB);
        assert!(h.state().is_mask_up_pending);
        h.up(ALT);
        assert!(!h.state().is_mask_up_pending);
        assert_eq!(h.injector.inserted, vec![Down, Up]);
        assert_eq!(
            h.sink.posted,
            vec![SwitchApps { reverse: false }, SwitchAppsDone]
        );
    }

    #[test]
    fn mask_is_never_requested_for_a_switch_that_is_not_allowed() {
        let mut h = Harness::new();
        h.blacklisted = true;

        h.down(ALT);
        h.tap(BACKTICK);
        assert!(h.injector.requests.is_empty());
    }

    // Lifecycle

    #[test]
    fn quiesced_state_never_posts_or_consumes_even_mid_session() {
        let mut h = Harness::new();

        h.down(ALT);
        h.tap(TAB);
        h.shared.quiesce();
        h.sink.posted.clear();
        assert!(!h.tap(TAB));
        assert!(!h.tap(ESC));
        assert!(!h.up(ALT));
        assert!(h.sink.posted.is_empty());
    }

    #[test]
    fn shutdown_after_the_hook_thread_exits_releases_a_pending_mask_up() {
        let mut h = Harness::new();
        h.injector = FakeInjector::scripted(&[1, 0]);

        h.down(ALT);
        h.down(TAB);
        assert!(h.state().is_mask_up_pending);
        h.shared.quiesce();
        assert_eq!(
            finish_shutdown(&h.shared, true, &mut h.injector),
            MaskCleanup::Released
        );
        assert!(!h.state().is_mask_up_pending);
        assert_eq!(h.injector.inserted, vec![Down, Up]);
    }

    #[test]
    fn shutdown_without_a_pending_mask_up_injects_nothing() {
        let mut h = Harness::new();

        h.down(ALT);
        h.tap(TAB);
        h.shared.quiesce();
        let requests = h.injector.requests.len();
        assert_eq!(
            finish_shutdown(&h.shared, true, &mut h.injector),
            MaskCleanup::NotNeeded
        );
        assert_eq!(h.injector.requests.len(), requests);
    }

    #[test]
    fn persistent_mask_up_failure_is_reported_and_shutdown_still_completes() {
        let mut h = Harness::new();
        h.injector.fail_up = true;

        h.down(ALT);
        // Only the mask key-down went in; the hold still counts as masked.
        assert!(h.down(TAB));
        h.up(TAB);
        h.up(ALT);
        h.shared.quiesce();
        // The hook keeps retrying after quiesce without posting anything.
        h.sink.posted.clear();
        assert!(!h.tap(A));
        assert!(h.sink.posted.is_empty());

        assert_eq!(
            finish_shutdown(&h.shared, true, &mut h.injector),
            MaskCleanup::StillDown
        );
        // Every event and the shutdown retried the key-up; none went in, so
        // vkE8 may still be logically down. That is reported, not hidden.
        assert!(h.state().is_mask_up_pending);
        assert_eq!(h.injector.inserted, vec![Down]);
        assert_eq!(
            h.injector.requests,
            vec![
                vec![Down, Up],
                vec![Up], // immediate retry
                vec![Up], // Tab up
                vec![Up], // Alt up
                vec![Up], // A down, after quiesce
                vec![Up], // A up, after quiesce
                vec![Up], // shutdown
            ]
        );
    }

    #[test]
    fn quiesce_and_shutdown_never_wait_for_a_hook_stuck_holding_its_state() {
        // Leaked so the "hook" (this thread) and the shutdown thread share it
        // the way the real hook thread shares the never-freed `HOOK`.
        let shared: &'static HookShared = Box::leak(Box::new(shared_with(["alt+`", "alt+tab"])));
        let stuck_hook = shared.state.lock();

        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            shared.quiesce();
            let cleanup = finish_shutdown(shared, false, &mut FakeInjector::default());
            let _ = done_tx.send(cleanup);
        });
        // A hang guard only: shutdown never waits on the state lock, so it
        // finishes as soon as its thread runs.
        let cleanup = done_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("shutdown must not wait for the hook's state lock");
        assert_eq!(cleanup, MaskCleanup::Skipped);
        assert_eq!(shared.target.load(Ordering::Acquire), 0);

        // The stuck hook's state is still valid memory after shutdown
        // returned, and once it resumes it posts and consumes nothing.
        assert!(stuck_hook.hotkeys.len() == 2);
        drop(stuck_hook);
        let mut sink = FakeSink::default();
        let mut injector = FakeInjector::default();
        for (scan_code, is_pressed) in [(ALT, true), (TAB, true), (TAB, false), (ALT, false)] {
            let event = KeyEvent {
                scan_code,
                is_extended: false,
                is_pressed,
            };
            assert!(!shared.on_key(event, false, |_| &mut sink, &mut injector));
        }
        assert!(sink.posted.is_empty());
        assert!(injector.requests.is_empty());
    }

    #[test]
    fn contended_state_passes_the_key_through_without_posting() {
        let mut h = Harness::new();
        h.down(ALT);
        let held = h.shared.state.lock();
        let mut sink = FakeSink::default();
        let event = KeyEvent {
            scan_code: TAB,
            is_extended: false,
            is_pressed: true,
        };
        assert!(!h
            .shared
            .on_key(event, false, |_| &mut sink, &mut FakeInjector::default()));
        drop(held);
        assert!(sink.posted.is_empty());
    }

    #[test]
    fn listener_slot_stays_taken_while_a_listener_or_detached_hook_owns_it() {
        // A detached hook thread never releases the slot, so `start` (which
        // takes the state lock) can never run alongside it.
        LISTENER_ACTIVE.store(true, Ordering::Release);
        let result = KeyboardListener::init(HWND::default(), &[]);
        assert!(LISTENER_ACTIVE.load(Ordering::Acquire));
        LISTENER_ACTIVE.store(false, Ordering::Release);
        assert!(result.is_err());
    }

    #[test]
    fn restarted_state_has_no_pending_hold_or_commit() {
        let mut h = Harness::new();
        h.down(ALT);
        h.down(SHIFT);
        h.tap(TAB);

        // `KeyboardListener::start` replaces the state wholesale.
        let mut h = Harness::new();
        assert!(!h.up(ALT));
        assert!(!h.down(TAB));
        assert!(h.sink.posted.is_empty());
        assert!(h.injector.requests.is_empty());
    }

    #[test]
    fn hotkeys_with_the_same_modifier_share_one_hold() {
        let h = Harness::new();
        assert_eq!(h.state().holds.len(), 1);
    }

    // Left/right modifier sides

    #[test]
    fn win_hold_lasts_until_both_win_keys_are_released() {
        let mut h = Harness::with_hotkeys(["win+`", "win+tab"]);

        assert!(!h.down_ext(LWIN));
        assert!(!h.down_ext(RWIN));
        assert!(h.tap(TAB));
        // Releasing one side keeps the hold, its mask, ownership and intent.
        assert!(!h.up_ext(LWIN));
        assert_eq!(h.sink.posted, vec![SwitchApps { reverse: false }]);
        assert!(h.tap(TAB));
        assert!(h.tap(BACKTICK));
        assert!(!h.up_ext(RWIN));
        assert_eq!(
            h.sink.posted,
            vec![
                SwitchApps { reverse: false },
                SwitchApps { reverse: false },
                SwitchWindows { reverse: false },
                SwitchWindowsDone,
                SwitchAppsDone,
            ]
        );
        assert_eq!(h.injector.requests, vec![vec![Down, Up]]);

        // The hold really ended: Tab alone is not a hotkey any more.
        assert!(!h.tap(TAB));
    }

    #[test]
    fn win_hold_survives_pressing_the_other_side_again() {
        let mut h = Harness::with_hotkeys(["win+`", "win+tab"]);

        h.down_ext(LWIN);
        h.tap(TAB);
        h.down_ext(RWIN);
        h.up_ext(LWIN);
        h.down_ext(LWIN);
        h.up_ext(RWIN);
        assert!(h.tap(TAB));
        h.up_ext(LWIN);
        assert_eq!(h.injector.requests, vec![vec![Down, Up]]);
        assert_eq!(
            h.sink.posted,
            vec![
                SwitchApps { reverse: false },
                SwitchApps { reverse: false },
                SwitchAppsDone,
            ]
        );
    }

    #[test]
    fn left_and_right_alt_share_one_hold_and_auto_repeat_is_idempotent() {
        let mut h = Harness::new();

        h.down(ALT);
        h.down(ALT);
        h.down_ext(ALT);
        h.down_ext(ALT);
        assert!(h.tap(TAB));
        // Left Alt up once, although it auto-repeated: right Alt still holds.
        assert!(!h.up(ALT));
        assert!(h.tap(TAB));
        assert_eq!(h.sink.posted.len(), 2);
        assert!(!h.up_ext(ALT));
        assert_eq!(h.sink.posted.last(), Some(&SwitchAppsDone));
        assert_eq!(h.injector.requests, vec![vec![Down, Up]]);
    }

    #[test]
    fn a_stray_modifier_release_without_a_hold_does_nothing() {
        let mut h = Harness::new();

        assert!(!h.up_ext(ALT));
        assert!(!h.up(ALT));
        assert!(h.sink.posted.is_empty());
        assert!(!h.tap(TAB));
    }

    #[test]
    fn releasing_one_shift_keeps_reverse_switching() {
        let mut h = Harness::new();

        h.down(SHIFT);
        h.down(RSHIFT);
        h.down(ALT);
        h.tap(TAB);
        h.up(SHIFT);
        h.tap(TAB);
        h.up(RSHIFT);
        h.tap(TAB);
        assert_eq!(
            h.sink.posted,
            vec![
                SwitchApps { reverse: true },
                SwitchApps { reverse: true },
                SwitchApps { reverse: false },
            ]
        );
    }

    // Injected input (TASK-S06C)

    #[test]
    fn physical_hook_data_becomes_a_key_event() {
        let event = |scan_code, is_extended, is_pressed| {
            KeyEvent::from_hook_data(&hook_data(scan_code, is_extended, is_pressed, 0))
        };
        assert_eq!(
            event(TAB, false, true),
            Some(KeyEvent {
                scan_code: TAB,
                is_extended: false,
                is_pressed: true
            })
        );
        assert_eq!(
            event(TAB, false, false),
            Some(KeyEvent {
                scan_code: TAB,
                is_extended: false,
                is_pressed: false
            })
        );
        // Right Alt and both Win keys arrive extended.
        assert_eq!(
            event(ALT, true, true),
            Some(KeyEvent {
                scan_code: ALT,
                is_extended: true,
                is_pressed: true
            })
        );
        assert_eq!(
            event(RWIN, true, false),
            Some(KeyEvent {
                scan_code: RWIN,
                is_extended: true,
                is_pressed: false
            })
        );
    }

    #[test]
    fn injected_hook_data_never_becomes_a_key_event() {
        for flags in INJECTED_VARIANTS {
            for (scan_code, is_extended) in [(TAB, false), (ALT, false), (ALT, true), (LWIN, true)]
            {
                for is_pressed in [true, false] {
                    let data = hook_data(scan_code, is_extended, is_pressed, flags);
                    assert_eq!(KeyEvent::from_hook_data(&data), None, "{data:?}");
                }
            }
        }
        // WinOrbit's own mask key is ignored because Windows flags it as
        // injected, not because of its marker: the marker alone is not enough.
        let own_mask = KBDLLHOOKSTRUCT {
            vkCode: VK_MASK as u32,
            flags: KBDLLHOOKSTRUCT_FLAGS(LLKHF_INJECTED.0),
            dwExtraInfo: OWN_INJECTION_MARKER,
            ..Default::default()
        };
        assert_eq!(KeyEvent::from_hook_data(&own_mask), None);
        let marker_without_flag = KBDLLHOOKSTRUCT {
            flags: KBDLLHOOKSTRUCT_FLAGS(0),
            ..own_mask
        };
        assert!(KeyEvent::from_hook_data(&marker_without_flag).is_some());
    }

    #[test]
    fn injected_events_change_nothing_and_are_never_consumed() {
        // Every key WinOrbit cares about, both sides, down and up, with each
        // injected flag combination: from idle, from a physical hold, and
        // from a live switch session.
        let keys = [
            (ALT, false),
            (ALT, true),
            (LWIN, true),
            (RWIN, true),
            (SHIFT, false),
            (RSHIFT, false),
            (TAB, false),
            (BACKTICK, false),
            (ESC, false),
            (A, false),
        ];
        let setups: [fn(&mut Harness); 3] = [
            |_| {},
            |h| {
                h.down(ALT);
            },
            |h| {
                h.down(ALT);
                h.tap(TAB);
            },
        ];
        for setup in setups {
            for hotkeys in [["alt+`", "alt+tab"], ["win+`", "win+tab"]] {
                let mut h = Harness::with_hotkeys(hotkeys);
                setup(&mut h);
                let before = h.snapshot();
                let posted = h.sink.posted.len();
                let requests = h.injector.requests.len();
                for flags in INJECTED_VARIANTS {
                    for (scan_code, is_extended) in keys {
                        for is_pressed in [true, false] {
                            let data = hook_data(scan_code, is_extended, is_pressed, flags);
                            assert!(!h.raw(data), "consumed {data:?}");
                        }
                    }
                }
                assert_eq!(h.snapshot(), before);
                assert_eq!(h.sink.posted.len(), posted);
                assert_eq!(h.injector.requests.len(), requests);
            }
        }
    }

    #[test]
    fn injected_tab_under_a_physical_alt_does_not_switch() {
        let mut h = Harness::new();

        h.down(ALT);
        assert!(!h.injected(TAB, false, true));
        assert!(!h.injected(TAB, false, false));
        assert!(!h.up(ALT));
        assert!(h.sink.posted.is_empty());
        assert!(h.injector.requests.is_empty());
    }

    #[test]
    fn injected_alt_down_does_not_start_a_hold() {
        let mut h = Harness::new();

        h.injected(ALT, false, true);
        assert!(!h.tap(TAB));
        assert!(!h.tap(BACKTICK));
        h.injected(ALT, false, false);
        assert!(h.sink.posted.is_empty());
        assert!(h.injector.requests.is_empty());
    }

    #[test]
    fn injected_alt_up_does_not_end_a_physical_hold() {
        let mut h = Harness::new();

        h.down(ALT);
        assert!(h.tap(TAB));
        assert!(!h.injected(ALT, false, false));
        assert!(!h.injected(ALT, true, false));
        // Still the same session: Tab advances it without a new mask.
        assert!(h.tap(TAB));
        assert!(!h.up(ALT));
        assert_eq!(
            h.sink.posted,
            vec![
                SwitchApps { reverse: false },
                SwitchApps { reverse: false },
                SwitchAppsDone,
            ]
        );
        assert_eq!(h.injector.requests, vec![vec![Down, Up]]);
    }

    #[test]
    fn injected_escape_does_not_cancel_a_session() {
        let mut h = Harness::new();

        h.down(ALT);
        h.tap(TAB);
        assert!(!h.injected(ESC, false, true));
        assert!(!h.injected(ESC, false, false));
        h.up(ALT);
        assert_eq!(
            h.sink.posted,
            vec![SwitchApps { reverse: false }, SwitchAppsDone]
        );
    }

    #[test]
    fn injected_shift_does_not_change_reverse_switching() {
        let mut h = Harness::new();

        h.down(ALT);
        h.injected(SHIFT, false, true);
        h.tap(TAB);
        h.down(RSHIFT);
        h.injected(RSHIFT, false, false);
        h.tap(TAB);
        assert_eq!(
            h.sink.posted,
            vec![SwitchApps { reverse: false }, SwitchApps { reverse: true }]
        );
    }

    #[test]
    fn injected_win_events_do_not_change_the_win_hold() {
        let mut h = Harness::with_hotkeys(["win+`", "win+tab"]);

        h.down_ext(LWIN);
        assert!(h.tap(TAB));
        // An injected right Win down does not count as a second held side...
        h.injected(RWIN, true, true);
        // ...and an injected left Win up does not end the physical hold.
        h.injected(LWIN, true, false);
        assert!(h.tap(TAB));
        assert!(!h.up_ext(LWIN));
        assert_eq!(
            h.sink.posted,
            vec![
                SwitchApps { reverse: false },
                SwitchApps { reverse: false },
                SwitchAppsDone,
            ]
        );
        // The hold is over although the injected right Win never came up.
        assert!(!h.tap(TAB));
        assert_eq!(h.injector.requests, vec![vec![Down, Up]]);
    }

    #[test]
    fn injected_win_down_does_not_start_a_win_hold() {
        let mut h = Harness::with_hotkeys(["win+`", "win+tab"]);

        h.injected(LWIN, true, true);
        assert!(!h.tap(TAB));
        h.injected(LWIN, true, false);
        assert!(h.sink.posted.is_empty());
        assert!(h.injector.requests.is_empty());
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
        assert!(!wait_for_thread_exit(&exited_rx, Duration::ZERO));
    }

    #[test]
    fn actions_map_to_the_existing_app_messages() {
        let message = |action| action_message(action).map(|(msg, lparam)| (msg, lparam.0));

        assert_eq!(message(HotKeyAction::Consume), None);
        assert_eq!(
            message(SwitchApps { reverse: false }),
            Some((WM_USER_SWITCH_APPS, 0))
        );
        assert_eq!(
            message(SwitchApps { reverse: true }),
            Some((WM_USER_SWITCH_APPS, 1))
        );
        assert_eq!(message(SwitchAppsDone), Some((WM_USER_SWITCH_APPS_DONE, 0)));
        assert_eq!(
            message(SwitchAppsCancel),
            Some((WM_USER_SWITCH_APPS_CANCEL, 0))
        );
        assert_eq!(
            message(SwitchWindows { reverse: true }),
            Some((WM_USER_SWITCH_WINDOWS, 1))
        );
        assert_eq!(
            message(SwitchWindowsDone),
            Some((WM_USER_SWITCH_WINDOWS_DONE, 0))
        );
    }
}
