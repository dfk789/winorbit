use crate::{
    app::{
        WM_USER_OVERVIEW_SECOND_TAP, WM_USER_OVERVIEW_TOGGLE, WM_USER_SWITCH_APPS,
        WM_USER_SWITCH_APPS_CANCEL, WM_USER_SWITCH_APPS_DONE, WM_USER_SWITCH_WINDOWS,
        WM_USER_SWITCH_WINDOWS_DONE,
    },
    config::{Hotkey, SuperTapConfig, SWITCH_APPS_HOTKEY_ID, SWITCH_WINDOWS_HOTKEY_ID},
    foreground::{IS_FOREGROUND_IN_BLACKLIST, IS_FOREGROUND_START},
    super_key::{KeyStateProbe, SuperTap, TapOutcome, VK_LWIN, VK_RWIN},
};

use anyhow::{anyhow, bail, Result};
use parking_lot::Mutex;
use std::sync::{
    atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering},
    mpsc::{self, Receiver, RecvTimeoutError},
    LazyLock,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::Win32::{
    Foundation::{GetLastError, HWND, LPARAM, LRESULT, WPARAM},
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::{
        Input::KeyboardAndMouse::{
            GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
            KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, SCANCODE_LSHIFT, SCANCODE_RSHIFT, VIRTUAL_KEY,
            VK_CONTROL, VK_ESCAPE,
        },
        WindowsAndMessaging::{
            CallNextHookEx, DispatchMessageW, GetMessageW, PeekMessageW, PostMessageW,
            PostThreadMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
            HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, LLKHF_LOWER_IL_INJECTED,
            LLKHF_UP, MSG, PM_NOREMOVE, WH_KEYBOARD_LL, WM_APP, WM_QUIT,
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

/// Thread message asking the hook thread to reopen Start (see `StartReplay`).
const WM_HOOK_REPLAY_START: u32 = WM_APP + 0x30;

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
    /// Bumped by `reset_input_state` (session lock/unlock). The hook drops
    /// all physical key state on its next event once this differs from
    /// `KeyboardState::epoch`.
    epoch: AtomicU32,
    state: Mutex<KeyboardState>,
    /// Longest hook callback so far, in microseconds; logged at shutdown.
    max_callback_us: AtomicU64,
}

impl HookShared {
    fn new() -> Self {
        Self {
            target: AtomicIsize::new(0),
            epoch: AtomicU32::new(0),
            state: Mutex::new(KeyboardState::new(&[], None)),
            max_callback_us: AtomicU64::new(0),
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
        foreground: Foreground,
        sink_for: impl FnOnce(isize) -> S,
        io: &mut impl HookIo,
    ) -> bool {
        let Some(mut state) = self.state.try_lock() else {
            return false;
        };
        // Retried even after quiesce, so the mask key gets every chance to go up.
        state.flush_pending_mask_up(io);
        let epoch = self.epoch.load(Ordering::Acquire);
        if state.epoch != epoch {
            state.reset_physical_state();
            state.epoch = epoch;
        }
        let target = self.target.load(Ordering::Acquire);
        if target == 0 {
            return false;
        }
        state.handle_key(event, foreground, &mut sink_for(target), io)
    }
}

static HOOK: LazyLock<HookShared> = LazyLock::new(HookShared::new);

/// At most one listener (or detached hook thread) may own `HOOK` at a time.
static LISTENER_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Tells the hook that key-ups may have been lost (session lock/unlock, a
/// switch to the secure desktop). Never blocks: the hook drops its physical
/// key state on its next event. Safe to call at any time, from any thread.
///
/// This is the hook's only recovery from lost key-ups. It never consults
/// `GetAsyncKeyState`, which reports an injected key-up exactly like a
/// physical one and so would let injected input change hook decisions.
pub fn reset_input_state(reason: &str) {
    HOOK.epoch.fetch_add(1, Ordering::AcqRel);
    info!("input reset: {reason}");
}

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
    pub fn init(
        hwnd: HWND,
        hotkeys: &[&Hotkey],
        super_tap: Option<SuperTapConfig>,
    ) -> Result<Self> {
        if LISTENER_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            bail!("Keyboard listener is already running");
        }
        match Self::start(hwnd, hotkeys, super_tap) {
            Ok(listener) => Ok(listener),
            Err(err) => {
                HOOK.quiesce();
                LISTENER_ACTIVE.store(false, Ordering::Release);
                Err(err)
            }
        }
    }

    fn start(hwnd: HWND, hotkeys: &[&Hotkey], super_tap: Option<SuperTapConfig>) -> Result<Self> {
        // Fresh state on every start, so nothing leaks across restarts. This
        // lock is uncontended: holding `LISTENER_ACTIVE` means no hook thread
        // exists, because a detached one keeps the slot taken forever.
        let mut state = KeyboardState::new(hotkeys, super_tap);
        state.epoch = HOOK.epoch.load(Ordering::Acquire);
        *HOOK.state.lock() = state;
        HOOK.max_callback_us.store(0, Ordering::Relaxed);
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
        info!(
            "keyboard listener start, bare Win tap {}",
            match super_tap {
                Some(tap) => format!("owned (double tap {} ms)", tap.double_tap_ms),
                None => "native".into(),
            }
        );

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
        report_mask_cleanup(finish_shutdown(&HOOK, thread_exited, &mut SystemIo));
        info!(
            "keyboard hook longest callback {} us",
            HOOK.max_callback_us.load(Ordering::Relaxed)
        );
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
    // Nothing else to undo: S06D never consumes a Win event and never holds a
    // synthetic key down, so only a pending mask key-up can be outstanding.
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
            _ if message.hwnd.is_invalid() && message.message == WM_HOOK_REPLAY_START => {
                // Outside the hook callback, so the injected keys land after
                // the Win key-up that triggered the replay.
                replay_start_menu();
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

/// Injects Ctrl+Esc so Windows opens Start, releasing whatever went down if
/// only part of it could be inserted.
fn replay_start_menu() {
    let keys = [
        (VK_CONTROL.0, 0x1d, false),
        (VK_ESCAPE.0, 0x01, false),
        (VK_ESCAPE.0, 0x01, true),
        (VK_CONTROL.0, 0x1d, true),
    ];
    let sent = send_keys(&keys);
    if sent == keys.len() {
        info!("reopened Start after the overview action could not be delivered");
        return;
    }
    error!(
        "could not reopen Start: injected {sent} of {} keys",
        keys.len()
    );
    // Release keys whose key-down went in: Esc (if down but not up), then Ctrl.
    let mut release = vec![];
    if sent == 2 {
        release.push(keys[2]);
    }
    if (1..4).contains(&sent) {
        release.push(keys[3]);
    }
    if !release.is_empty() && send_keys(&release) != release.len() {
        error!("could not release keys after a failed Start replay");
    }
}

/// `SendInput` of (vk, scan code, is key-up) keyboard events tagged with
/// `OWN_INJECTION_MARKER`. At most 4 events, without allocating, because it
/// also runs inside the hook callback.
fn send_keys(keys: &[(u16, u16, bool)]) -> usize {
    let mut inputs = [INPUT::default(); 4];
    let count = keys.len().min(inputs.len());
    for (input, &(vk, scan, is_up)) in inputs.iter_mut().zip(&keys[..count]) {
        *input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: scan,
                    dwFlags: match is_up {
                        false => KEYBD_EVENT_FLAGS(0),
                        true => KEYEVENTF_KEYUP,
                    },
                    dwExtraInfo: OWN_INJECTION_MARKER,
                    ..Default::default()
                },
            },
        };
    }
    unsafe { SendInput(&inputs[..count], std::mem::size_of::<INPUT>() as i32) as usize }
}

/// What the foreground watcher last reported, read by the hook as plain flags.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Foreground {
    /// The foreground app is in `[switch-windows] blacklist`.
    is_in_blacklist: bool,
    /// Start or Search is the foreground window.
    is_start: bool,
}

/// One physical key event, as seen by the hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KeyEvent {
    scan_code: u32,
    /// Only used to tell apart modifier sides that share a scan code.
    is_extended: bool,
    is_pressed: bool,
    /// Virtual-key code, used by bare Win tap recognition.
    vk: u8,
    /// `KBDLLHOOKSTRUCT.time` (ms, wraps).
    time: u32,
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
            vk: data.vkCode as u8,
            time: data.time,
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

/// Asks the hook thread to reopen Start once the current callback has
/// returned. Used when Start was suppressed but the overview action could not
/// be delivered. Never injects from inside the callback: input injected there
/// lands *before* the event in flight, so Windows would see Win+Ctrl+Esc.
trait StartReplay {
    /// Returns false if the request could not be queued.
    fn request_start(&mut self) -> bool;
}

/// Everything the hook needs from the system besides the action sink.
trait HookIo: KeyInjector + KeyStateProbe + StartReplay {}

impl<T: KeyInjector + KeyStateProbe + StartReplay> HookIo for T {}

/// Production `HookIo`: `SendInput`, `GetAsyncKeyState` (only as the
/// end-of-probation veto, see `super_key`), and a thread message to the
/// current (hook) thread.
struct SystemIo;

impl KeyInjector for SystemIo {
    fn send(&mut self, keys: &[MaskKey]) -> usize {
        let mut events = [(0, 0, false); 2];
        let count = keys.len().min(events.len());
        for (event, key) in events.iter_mut().zip(&keys[..count]) {
            *event = (VK_MASK, 0, *key == MaskKey::Up);
        }
        let sent = send_keys(&events[..count]);
        if sent != count {
            warn!("injected {sent} of {count} modifier mask events");
        }
        sent
    }
}

impl KeyStateProbe for SystemIo {
    fn is_down(&self, vk: u8) -> bool {
        (unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000) != 0
    }
}

impl StartReplay for SystemIo {
    fn request_start(&mut self) -> bool {
        // Only called on the hook thread, whose loop handles the message.
        let thread_id = unsafe { GetCurrentThreadId() };
        unsafe { PostThreadMessageW(thread_id, WM_HOOK_REPLAY_START, WPARAM(0), LPARAM(0)) }.is_ok()
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
    /// Bare Win tap recognition; `None` (not even constructed) when disabled.
    super_tap: Option<SuperTap>,
    /// `HookShared::epoch` this state was last reset for.
    epoch: u32,
}

impl KeyboardState {
    fn new(hotkeys: &[&Hotkey], super_tap: Option<SuperTapConfig>) -> Self {
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
            super_tap: super_tap.map(|config| SuperTap::new(config.double_tap_ms)),
            epoch: 0,
        }
    }

    /// Forgets which keys are down and any hold in progress, without posting
    /// anything. Used after a session lock/unlock, where key-ups go to the
    /// secure desktop. The UI cancels its own switcher on every reset. A pending mask
    /// key-up is kept: it is still owed.
    fn reset_physical_state(&mut self) {
        self.shift = SidesDown::default();
        for hold in self.holds.iter_mut() {
            hold.sides = SidesDown::default();
            hold.is_masked = false;
            hold.is_owned = false;
        }
        for state in self.hotkeys.iter_mut() {
            state.is_hotkey_pressed = false;
            state.release_intent = ReleaseIntent::None;
        }
        if let Some(super_tap) = self.super_tap.as_mut() {
            super_tap.reset();
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
        if !self.inject_mask(injector) {
            return false;
        }
        self.holds[hold].is_masked = true;
        true
    }

    /// Injects a mask key-down/up. Returns false if nothing was inserted.
    fn inject_mask(&mut self, injector: &mut impl KeyInjector) -> bool {
        let inserted = injector.send(&[MaskKey::Down, MaskKey::Up]);
        if inserted == 0 {
            return false;
        }
        // With only the key-down inserted the app already saw modifier+mask, so
        // the modifier is masked. The key-up is retried (best effort) until it
        // goes in; see `MaskCleanup::StillDown`.
        if inserted == 1 && injector.send(&[MaskKey::Up]) != 1 {
            self.is_mask_up_pending = true;
        }
        true
    }

    /// Acts on a clean, trusted bare Win tap, on its key-up. The key-up itself
    /// always passes to Windows; only the mask decides whether Start opens.
    /// First and second taps are both owned and handled the same way; what a
    /// second tap means is decided by the overview controller.
    ///
    /// Transaction, so Start and the overview input are never both delivered
    /// nor both lost:
    /// 1. Inject the mask. Input injected inside the callback lands before the
    ///    key-up in flight, so Windows sees Win+vkE8 and does not open Start.
    ///    Nothing inserted: post nothing; Start opens natively.
    /// 2. Post the overview input (never waits for the UI).
    /// 3. Post failed after the mask went in: ask the hook thread to reopen
    ///    Start once this callback has returned.
    fn handle_super_tap(
        &mut self,
        tap: TapOutcome,
        sink: &mut impl ActionSink,
        io: &mut impl HookIo,
    ) {
        let (action, time) = match tap {
            TapOutcome::FirstTap { time } => (HotKeyAction::OverviewToggle { time }, time),
            TapOutcome::SecondTap { time } => (HotKeyAction::OverviewSecondTap { time }, time),
        };
        if !self.inject_mask(io) {
            return;
        }
        if sink.post(action) {
            if let (TapOutcome::FirstTap { .. }, Some(super_tap)) = (tap, self.super_tap.as_mut()) {
                super_tap.tap_delivered(time);
            }
        } else if !io.request_start() {
            error!("bare Win tap: overview input not delivered and Start could not be reopened");
        }
    }

    fn end_holds(&mut self, ended_holds: u32, sink: &mut impl ActionSink) {
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
        self.end_holds(ended_holds, sink);
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
    /// - Win key events are never consumed. Bare tap recognition only
    ///   observes; a Win hold that used a hotkey is a chord, so it can never
    ///   also be a tap.
    fn handle_key(
        &mut self,
        event: KeyEvent,
        foreground: Foreground,
        sink: &mut impl ActionSink,
        io: &mut impl HookIo,
    ) -> bool {
        if let Some(bit) = side_bit([SCANCODE_LSHIFT, SCANCODE_RSHIFT], event) {
            self.shift.apply(bit, event.is_pressed);
        }
        let reverse = self.shift.is_down();

        let tap = match self.super_tap.as_mut() {
            Some(super_tap) => super_tap.on_key(event.vk, event.is_pressed, event.time, io),
            None => None,
        };
        let is_modifier = self.handle_modifier_key(event, sink);
        // With Start (or Search) open, a tap is how Windows closes it, so it
        // is left to Windows. Not logged: this runs inside the hook callback.
        if let Some(tap) = tap.filter(|_| !foreground.is_start) {
            self.handle_super_tap(tap, sink, io);
        }
        if is_modifier || event.vk == VK_LWIN || event.vk == VK_RWIN {
            return false;
        }

        for index in 0..self.hotkeys.len() {
            let allow_switch = self.hotkeys[index].hotkey.id != SWITCH_WINDOWS_HOTKEY_ID
                || !foreground.is_in_blacklist;
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

            if !self.ensure_masked(hold, io) {
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
    SwitchApps {
        reverse: bool,
    },
    SwitchAppsDone,
    SwitchAppsCancel,
    SwitchWindows {
        reverse: bool,
    },
    SwitchWindowsDone,
    /// Clean bare Win tap; Start was suppressed. `time` is the key-up's
    /// `KBDLLHOOKSTRUCT.time`.
    OverviewToggle {
        time: u32,
    },
    /// Clean bare Win tap within the double-tap window of a delivered
    /// `OverviewToggle`; Start was suppressed as well.
    OverviewSecondTap {
        time: u32,
    },
}

/// Hook-thread handling of one raw event. Injected input is rejected here,
/// before the state is even locked, so it can never change modifier, Shift or
/// hotkey state, start/advance/cancel/commit a switch, inject a mask, post a
/// UI action, or be consumed: it always passes on to the next hook.
fn handle_hook_data<S: ActionSink>(
    shared: &HookShared,
    data: &KBDLLHOOKSTRUCT,
    foreground: Foreground,
    sink_for: impl FnOnce(isize) -> S,
    io: &mut impl HookIo,
) -> bool {
    match KeyEvent::from_hook_data(data) {
        Some(event) => shared.on_key(event, foreground, sink_for, io),
        None => false,
    }
}

unsafe extern "system" fn keyboard_proc(code: i32, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
    if code != HC_ACTION as i32 {
        return CallNextHookEx(None, code, w_param, l_param);
    }
    // No routine logging here: this runs for every key in the system.
    let started = Instant::now();
    let kbd_data: &KBDLLHOOKSTRUCT = &*(l_param.0 as *const _);
    let foreground = Foreground {
        is_in_blacklist: IS_FOREGROUND_IN_BLACKLIST.load(Ordering::Relaxed),
        is_start: IS_FOREGROUND_START.load(Ordering::Relaxed),
    };
    let consume = handle_hook_data(
        &HOOK,
        kbd_data,
        foreground,
        |target| WindowSink(HWND(target as _)),
        &mut SystemIo,
    );
    HOOK.max_callback_us
        .fetch_max(started.elapsed().as_micros() as u64, Ordering::Relaxed);
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
        HotKeyAction::OverviewToggle { time } => {
            Some((WM_USER_OVERVIEW_TOGGLE, LPARAM(time as isize)))
        }
        HotKeyAction::OverviewSecondTap { time } => {
            Some((WM_USER_OVERVIEW_SECOND_TAP, LPARAM(time as isize)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        action_message, finish_shutdown, handle_hook_data, wait_for_thread_exit, ActionSink,
        Foreground, HookShared, HotKeyAction, KeyEvent, KeyInjector, KeyboardListener,
        KeyboardState, MaskCleanup, MaskKey, StartReplay, LISTENER_ACTIVE, OWN_INJECTION_MARKER,
        VK_MASK,
    };
    use crate::app::{
        WM_USER_OVERVIEW_SECOND_TAP, WM_USER_OVERVIEW_TOGGLE, WM_USER_SWITCH_APPS,
        WM_USER_SWITCH_APPS_CANCEL, WM_USER_SWITCH_APPS_DONE, WM_USER_SWITCH_WINDOWS,
        WM_USER_SWITCH_WINDOWS_DONE,
    };
    use crate::config::SuperTapConfig;
    use crate::config::{Hotkey, SWITCH_APPS_HOTKEY_ID, SWITCH_WINDOWS_HOTKEY_ID};
    use crate::super_key::KeyStateProbe;
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
    const CTRL: u32 = 0x1d;
    const KEY_1: u32 = 0x02;
    const D: u32 = 0x20;
    const E: u32 = 0x12;
    const L: u32 = 0x26;
    const O: u32 = 0x18;
    const R: u32 = 0x13;
    const S: u32 = 0x1f;
    const X: u32 = 0x2d;
    const ENTER: u32 = 0x1c;
    const LEFT: u32 = 0x4b;
    /// An otherwise unused key (F24) for ending probation.
    const SYNC: u32 = 0x76;

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

    /// Fake system side of the hook (`HookIo`).
    ///
    /// Injection: records every request and the keys actually inserted. Each
    /// call inserts the next scripted count, or everything once the script is
    /// exhausted. With `fail_up`, every mask key-up fails forever.
    ///
    /// Start replay: counts requests; `fail_replay` makes them fail.
    struct FakeInjector {
        script: VecDeque<usize>,
        fail_up: bool,
        requests: Vec<Vec<MaskKey>>,
        inserted: Vec<MaskKey>,
        replays: usize,
        fail_replay: bool,
        /// What `GetAsyncKeyState` reports: updated after the hook has seen
        /// each event, injected ones included, as Windows does.
        async_down: [bool; 256],
    }

    impl Default for FakeInjector {
        fn default() -> Self {
            Self {
                script: VecDeque::new(),
                fail_up: false,
                requests: vec![],
                inserted: vec![],
                replays: 0,
                fail_replay: false,
                async_down: [false; 256],
            }
        }
    }

    impl KeyStateProbe for FakeInjector {
        fn is_down(&self, vk: u8) -> bool {
            let any = |sides: [usize; 2]| sides.iter().any(|vk| self.async_down[*vk]);
            match vk {
                0x10 => any([0xA0, 0xA1]),
                0x11 => any([0xA2, 0xA3]),
                0x12 => any([0xA4, 0xA5]),
                _ => self.async_down[vk as usize],
            }
        }
    }

    impl StartReplay for FakeInjector {
        fn request_start(&mut self) -> bool {
            if self.fail_replay {
                return false;
            }
            self.replays += 1;
            true
        }
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

    /// US-layout virtual-key code for the scan codes these tests use, as
    /// Windows reports it in `KBDLLHOOKSTRUCT.vkCode`.
    fn vk_of(scan_code: u32, is_extended: bool) -> u8 {
        match (scan_code, is_extended) {
            (ALT, false) => 0xA4,
            (ALT, true) => 0xA5,
            (CTRL, false) => 0xA2,
            (CTRL, true) => 0xA3,
            (SHIFT, _) => 0xA0,
            (RSHIFT, _) => 0xA1,
            (LWIN, _) => 0x5B,
            (RWIN, _) => 0x5C,
            (TAB, _) => 0x09,
            (BACKTICK, _) => 0xC0,
            (ESC, _) => 0x1B,
            (A, _) => 0x41,
            (KEY_1, _) => 0x31,
            (D, _) => 0x44,
            (E, _) => 0x45,
            (L, _) => 0x4C,
            (O, _) => 0x4F,
            (R, _) => 0x52,
            (S, _) => 0x53,
            (X, _) => 0x58,
            (ENTER, _) => 0x0D,
            (LEFT, true) => 0x25,
            (SYNC, _) => 0x87,
            _ => 0xFF,
        }
    }

    fn key_event(scan_code: u32, is_extended: bool, is_pressed: bool) -> KeyEvent {
        KeyEvent {
            scan_code,
            is_extended,
            is_pressed,
            vk: vk_of(scan_code, is_extended),
            time: 0,
        }
    }

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
            vkCode: vk_of(scan_code, is_extended) as u32,
            scanCode: scan_code,
            flags: KBDLLHOOKSTRUCT_FLAGS(flags),
            ..Default::default()
        }
    }

    /// Shared hook state for one listener, driven through the same
    /// `HookShared::on_key` entry point the real hook uses.
    fn shared_with(hotkeys: [&str; 2], super_tap: Option<SuperTapConfig>) -> HookShared {
        let windows = Hotkey::create(SWITCH_WINDOWS_HOTKEY_ID, "switch windows", hotkeys[0])
            .expect("switch-windows hotkey should parse");
        let apps = Hotkey::create(SWITCH_APPS_HOTKEY_ID, "switch apps", hotkeys[1])
            .expect("switch-apps hotkey should parse");
        let shared = HookShared::new();
        *shared.state.lock() = KeyboardState::new(&[&windows, &apps], super_tap);
        shared.target.store(1, Ordering::Release);
        shared
    }

    struct Harness {
        shared: HookShared,
        sink: FakeSink,
        injector: FakeInjector,
        blacklisted: bool,
        /// Start is the foreground window.
        start_open: bool,
        /// `KBDLLHOOKSTRUCT.time` of the next event; starts near wrap-around.
        now: u32,
    }

    impl Harness {
        fn new() -> Self {
            Self::with_hotkeys(["alt+`", "alt+tab"])
        }

        fn with_hotkeys(hotkeys: [&str; 2]) -> Self {
            Self::build(hotkeys, None)
        }

        /// Bare Win tap ownership enabled, with default hotkeys.
        fn with_overview(double_tap_ms: u32) -> Self {
            Self::build(["alt+`", "alt+tab"], Some(SuperTapConfig { double_tap_ms }))
        }

        /// With overview enabled the hook starts in probation; tests start
        /// trusted unless they use `build_in_probation`.
        fn build(hotkeys: [&str; 2], super_tap: Option<SuperTapConfig>) -> Self {
            let mut h = Self::build_in_probation(hotkeys, super_tap);
            if super_tap.is_some() {
                h.sync_tap();
                assert!(h.is_trusted());
            }
            h
        }

        fn build_in_probation(hotkeys: [&str; 2], super_tap: Option<SuperTapConfig>) -> Self {
            Self {
                shared: shared_with(hotkeys, super_tap),
                sink: FakeSink::default(),
                injector: FakeInjector::default(),
                blacklisted: false,
                start_open: false,
                now: u32::MAX - 5_000,
            }
        }

        /// Time passes without input.
        fn wait(&mut self, ms: u32) {
            self.now = self.now.wrapping_add(ms);
        }

        fn state(&self) -> MutexGuard<'_, KeyboardState> {
            self.shared.state.lock()
        }

        /// Feeds one raw hook event through the same entry point as the real
        /// hook, including the injected-input filter.
        fn raw(&mut self, mut data: KBDLLHOOKSTRUCT) -> bool {
            // Only physical events advance the clock, so a run with injected
            // events interleaved sees exactly the same physical timestamps.
            if data.flags.0 & (LLKHF_INJECTED.0 | LLKHF_LOWER_IL_INJECTED.0) == 0 {
                self.now = self.now.wrapping_add(10);
            }
            data.time = self.now;
            let sink = &mut self.sink;
            let consumed = handle_hook_data(
                &self.shared,
                &data,
                Foreground {
                    is_in_blacklist: self.blacklisted,
                    is_start: self.start_open,
                },
                move |_| sink,
                &mut self.injector,
            );
            // Windows' key state changes after the hooks have seen the event,
            // injected or not (a consumed key never changes it).
            if !consumed {
                self.injector.async_down[data.vkCode as usize & 0xFF] =
                    data.flags.0 & LLKHF_UP.0 == 0;
            }
            consumed
        }

        /// The key is physically down without the hook having seen it.
        fn hold(&mut self, scan_code: u32, is_extended: bool) {
            self.injector.async_down[vk_of(scan_code, is_extended) as usize] = true;
        }

        /// A plain key tap: ends probation when nothing else is held.
        fn sync_tap(&mut self) {
            self.tap(SYNC);
        }

        fn is_trusted(&self) -> bool {
            let state = self.state();
            let tap = state.super_tap.as_ref().expect("tap state");
            tap.sync() == crate::super_key::PhysicalSync::Trusted
        }

        /// A key event without injected flags, as a physical key produces.
        /// "Physical" in these tests means exactly that.
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
        let shared: &'static HookShared =
            Box::leak(Box::new(shared_with(["alt+`", "alt+tab"], None)));
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
            let event = key_event(scan_code, false, is_pressed);
            assert!(!shared.on_key(event, Foreground::default(), |_| &mut sink, &mut injector));
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
        let event = key_event(TAB, false, true);
        assert!(!h.shared.on_key(
            event,
            Foreground::default(),
            |_| &mut sink,
            &mut FakeInjector::default()
        ));
        drop(held);
        assert!(sink.posted.is_empty());
    }

    #[test]
    fn listener_slot_stays_taken_while_a_listener_or_detached_hook_owns_it() {
        // A detached hook thread never releases the slot, so `start` (which
        // takes the state lock) can never run alongside it.
        LISTENER_ACTIVE.store(true, Ordering::Release);
        let result = KeyboardListener::init(HWND::default(), &[], None);
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
        assert_eq!(event(TAB, false, true), Some(key_event(TAB, false, true)));
        assert_eq!(event(TAB, false, false), Some(key_event(TAB, false, false)));
        // Right Alt and both Win keys arrive extended.
        assert_eq!(event(ALT, true, true), Some(key_event(ALT, true, true)));
        assert_eq!(event(RWIN, true, false), Some(key_event(RWIN, true, false)));
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
        // injected flag combination: from idle, from a physical hold of the
        // configured modifier, and from a live switch session started with
        // it. Each modifier side is used for the hold.
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
        let modifiers = [
            (["alt+`", "alt+tab"], ALT, false),
            (["alt+`", "alt+tab"], ALT, true),
            (["win+`", "win+tab"], LWIN, true),
            (["win+`", "win+tab"], RWIN, true),
        ];
        #[derive(Clone, Copy, Debug)]
        enum Setup {
            Idle,
            Hold,
            Session,
        }
        for (hotkeys, modifier, is_extended) in modifiers {
            for setup in [Setup::Idle, Setup::Hold, Setup::Session] {
                let mut h = Harness::with_hotkeys(hotkeys);
                let idle = h.snapshot();
                match setup {
                    Setup::Idle => {}
                    Setup::Hold => {
                        assert!(!h.event(modifier, is_extended, true));
                        assert_ne!(h.snapshot(), idle, "{hotkeys:?}: no hold");
                    }
                    Setup::Session => {
                        assert!(!h.event(modifier, is_extended, true));
                        assert!(h.tap(TAB), "{hotkeys:?}: Tab not consumed");
                        assert_eq!(h.sink.posted, vec![SwitchApps { reverse: false }]);
                    }
                }
                let posted = h.sink.posted.clone();
                let requests = h.injector.requests.clone();
                for flags in INJECTED_VARIANTS {
                    for (scan_code, key_extended) in keys {
                        for is_pressed in [true, false] {
                            let data = hook_data(scan_code, key_extended, is_pressed, flags);
                            let before = h.snapshot();
                            assert!(!h.raw(data), "{hotkeys:?} {setup:?}: consumed {data:?}");
                            assert_eq!(h.snapshot(), before, "{hotkeys:?} {setup:?}: {data:?}");
                            assert_eq!(h.sink.posted, posted, "{hotkeys:?} {setup:?}: {data:?}");
                            assert_eq!(
                                h.injector.requests, requests,
                                "{hotkeys:?} {setup:?}: {data:?}"
                            );
                        }
                    }
                }
                // The physical state survived intact: releasing the physical
                // modifier ends the hold, committing a live session.
                assert!(!h.event(modifier, is_extended, false));
                match setup {
                    Setup::Idle | Setup::Hold => assert_eq!(h.sink.posted, posted),
                    Setup::Session => assert_eq!(
                        h.sink.posted,
                        vec![SwitchApps { reverse: false }, SwitchAppsDone]
                    ),
                }
                assert_eq!(
                    h.snapshot(),
                    idle,
                    "{hotkeys:?} {setup:?}: not back to idle"
                );
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

    // Bare Win tap (TASK-S06D)

    use HotKeyAction::{OverviewSecondTap, OverviewToggle};

    impl Harness {
        /// Bare Win tap ownership enabled, with the given hotkeys.
        fn with_hotkeys_and_overview(hotkeys: [&str; 2]) -> Self {
            Self::build(hotkeys, Some(SuperTapConfig { double_tap_ms: 400 }))
        }

        /// Taps a Win key (both arrive extended); returns whether each event
        /// was consumed.
        fn win_tap(&mut self, win: u32) -> [bool; 2] {
            [self.down_ext(win), self.up_ext(win)]
        }

        /// Overview actions posted so far, by kind.
        fn taps(&self) -> Vec<&'static str> {
            self.sink
                .posted
                .iter()
                .filter_map(|action| match action {
                    OverviewToggle { .. } => Some("toggle"),
                    OverviewSecondTap { .. } => Some("second"),
                    _ => None,
                })
                .collect()
        }

        fn bump_epoch(&self) {
            self.shared.epoch.fetch_add(1, Ordering::AcqRel);
        }
    }

    #[test]
    fn bare_win_tap_masks_on_release_and_posts_the_overview() {
        for win in [LWIN, RWIN] {
            let mut h = Harness::with_overview(400);
            assert!(!h.down_ext(win));
            assert!(h.injector.requests.is_empty(), "{win:#x}: masked on press");
            assert!(h.sink.posted.is_empty());
            assert!(!h.up_ext(win), "{win:#x}: Win key-up consumed");
            assert_eq!(h.injector.requests, vec![vec![Down, Up]], "{win:#x}");
            assert_eq!(
                h.sink.posted,
                vec![OverviewToggle { time: h.now }],
                "{win:#x}"
            );
            assert_eq!(h.injector.replays, 0);
        }
    }

    #[test]
    fn key_repeat_and_long_holds_are_one_tap() {
        let mut h = Harness::with_overview(400);
        for _ in 0..30 {
            assert!(!h.down_ext(LWIN));
        }
        h.wait(10_000);
        assert!(!h.up_ext(LWIN));
        assert_eq!(h.taps(), vec!["toggle"]);
        assert_eq!(h.injector.requests, vec![vec![Down, Up]]);
    }

    /// One key event: (scan code, extended, pressed).
    type KeySeq = Vec<(u32, bool, bool)>;
    /// A step run between two taps.
    type Between<'a> = &'a dyn Fn(&mut Harness);

    /// Native Windows-key chords, as (scan code, extended, pressed) events,
    /// with `W` standing for the Win key under test.
    fn native_chords() -> Vec<(&'static str, KeySeq)> {
        const W: u32 = 0;
        let chord = |keys: &[(u32, bool)]| {
            let mut events = vec![(W, true, true)];
            events.extend(keys.iter().map(|&(key, ext)| (key, ext, true)));
            events.extend(keys.iter().rev().map(|&(key, ext)| (key, ext, false)));
            events.push((W, true, false));
            events
        };
        vec![
            ("Win+L", chord(&[(L, false)])),
            ("Win+D", chord(&[(D, false)])),
            ("Win+E", chord(&[(E, false)])),
            ("Win+R", chord(&[(R, false)])),
            ("Win+X", chord(&[(X, false)])),
            ("Win+1", chord(&[(KEY_1, false)])),
            ("Win+A (printable)", chord(&[(A, false)])),
            ("Win+Left", chord(&[(LEFT, true)])),
            ("Win+Shift+S", chord(&[(SHIFT, false), (S, false)])),
            ("Win+Ctrl+O", chord(&[(CTRL, false), (O, false)])),
            ("Win+Tab (not configured)", chord(&[(TAB, false)])),
            ("Win+` (not configured)", chord(&[(BACKTICK, false)])),
            (
                "Win+E, E released after Win",
                vec![
                    (W, true, true),
                    (E, false, true),
                    (W, true, false),
                    (E, false, false),
                ],
            ),
            (
                "Shift held, then a Win tap",
                vec![
                    (SHIFT, false, true),
                    (W, true, true),
                    (W, true, false),
                    (SHIFT, false, false),
                ],
            ),
            (
                "Shift pressed and released during the hold",
                vec![
                    (W, true, true),
                    (SHIFT, false, true),
                    (SHIFT, false, false),
                    (W, true, false),
                ],
            ),
            (
                "both Win keys overlap",
                vec![
                    (LWIN, true, true),
                    (RWIN, true, true),
                    (RWIN, true, false),
                    (LWIN, true, false),
                ],
            ),
            (
                "one side released while the other is held",
                vec![
                    (LWIN, true, true),
                    (RWIN, true, true),
                    (LWIN, true, false),
                    (RWIN, true, false),
                ],
            ),
        ]
    }

    #[test]
    fn native_chords_are_never_consumed_masked_or_posted() {
        for win in [LWIN, RWIN] {
            for (name, events) in native_chords() {
                let mut h = Harness::with_overview(400);
                for (key, ext, is_pressed) in events {
                    let key = if key == 0 { win } else { key };
                    assert!(
                        !h.event(key, ext, is_pressed),
                        "{name} ({win:#x}): consumed {key:#x}"
                    );
                    assert!(
                        h.injector.requests.is_empty(),
                        "{name} ({win:#x}): masked at {key:#x}"
                    );
                    assert!(h.sink.posted.is_empty(), "{name} ({win:#x}): posted");
                }
                // The chord leaves nothing behind: the next tap is clean.
                h.win_tap(win);
                assert_eq!(h.taps(), vec!["toggle"], "{name} ({win:#x}): next tap");
            }
        }
    }

    #[test]
    fn win_hotkeys_own_their_hold_and_release_never_taps() {
        for (hotkeys, key, switch, done) in [
            (
                ["win+`", "win+tab"],
                TAB,
                SwitchApps { reverse: false },
                SwitchAppsDone,
            ),
            (
                ["win+`", "win+tab"],
                BACKTICK,
                SwitchWindows { reverse: false },
                SwitchWindowsDone,
            ),
        ] {
            for win in [LWIN, RWIN] {
                let mut h = Harness::with_hotkeys_and_overview(hotkeys);
                assert!(!h.down_ext(win));
                assert!(h.tap(key), "{key:#x}: hotkey not consumed");
                assert!(!h.up_ext(win), "Win key-up consumed");
                assert_eq!(h.sink.posted, vec![switch, done], "{key:#x} {win:#x}");
                // Only the hotkey's own mask, no tap mask on release.
                assert_eq!(h.injector.requests, vec![vec![Down, Up]]);

                // A bare tap in the same configuration is still a tap.
                h.win_tap(win);
                assert_eq!(h.taps(), vec!["toggle"]);
            }
        }
    }

    #[test]
    fn injected_input_never_starts_breaks_or_ends_a_tap() {
        for flags in INJECTED_VARIANTS {
            let mut h = Harness::with_overview(400);
            let injected = |h: &mut Harness, key, ext, is_pressed| {
                assert!(!h.raw(hook_data(key, ext, is_pressed, flags)));
            };

            // Injected Win tap alone: nothing.
            let idle = h.snapshot();
            injected(&mut h, LWIN, true, true);
            injected(&mut h, LWIN, true, false);
            assert_eq!(h.snapshot(), idle, "{flags:#x}");
            assert!(h.sink.posted.is_empty() && h.injector.requests.is_empty());

            // Injected keys during a physical tap do not make it a chord.
            h.down_ext(LWIN);
            injected(&mut h, A, false, true);
            injected(&mut h, A, false, false);
            h.up_ext(LWIN);
            assert_eq!(h.taps(), vec!["toggle"], "{flags:#x}");

            // An injected Win key-up does not end a physical hold.
            h.wait(1_000);
            h.down_ext(RWIN);
            injected(&mut h, RWIN, true, false);
            assert_eq!(h.taps(), vec!["toggle"], "{flags:#x}: injected up ended it");
            h.up_ext(RWIN);
            assert_eq!(h.taps(), vec!["toggle", "toggle"], "{flags:#x}");

            // An injected overview-style action is just another injected key.
            assert_eq!(h.injector.requests.len(), 2);
        }
    }

    #[test]
    fn mask_failure_leaves_start_native_and_posts_nothing() {
        let mut h = Harness::with_overview(400);
        h.injector.script = [0].into();
        assert_eq!(h.win_tap(LWIN), [false, false]);
        assert_eq!(h.injector.requests, vec![vec![Down, Up]]);
        assert!(h.injector.inserted.is_empty());
        assert!(
            h.sink.posted.is_empty(),
            "overview posted although Start opens"
        );
        assert_eq!(h.injector.replays, 0);

        // An undelivered tap does not pair: the next quick tap is a first tap.
        h.wait(50);
        h.win_tap(LWIN);
        assert_eq!(h.taps(), vec!["toggle"]);
        assert_eq!(h.injector.requests.len(), 2);
    }

    #[test]
    fn partial_mask_still_suppresses_start_and_releases_the_mask_key() {
        let mut h = Harness::with_overview(400);
        h.injector.script = [1].into();
        h.win_tap(LWIN);
        assert_eq!(h.injector.requests, vec![vec![Down, Up], vec![Up]]);
        assert_eq!(h.injector.inserted, vec![Down, Up]);
        assert_eq!(h.taps(), vec!["toggle"]);

        // The key-up keeps failing: retried on the next event and at shutdown.
        let mut h = Harness::with_overview(400);
        h.injector.fail_up = true;
        h.win_tap(LWIN);
        assert_eq!(h.taps(), vec!["toggle"]);
        assert!(h.state().is_mask_up_pending);
        h.down(A);
        assert_eq!(h.injector.requests.last(), Some(&vec![Up]));
        h.injector.fail_up = false;
        h.up(A);
        assert!(!h.state().is_mask_up_pending);
        assert_eq!(h.injector.inserted, vec![Down, Up]);
    }

    #[test]
    fn undelivered_overview_reopens_start_from_the_hook_thread() {
        let mut h = Harness::with_overview(400);
        h.fail_posts(|action| matches!(action, OverviewToggle { .. }));
        assert_eq!(h.win_tap(LWIN), [false, false]);
        assert_eq!(h.injector.requests, vec![vec![Down, Up]]);
        assert_eq!(h.injector.replays, 1, "Start suppressed but not reopened");
        assert!(h.sink.posted.is_empty());

        // Not delivered, so no double-tap window: the next quick tap is masked.
        h.deliver_posts();
        h.wait(50);
        h.win_tap(LWIN);
        assert_eq!(h.taps(), vec!["toggle"]);
        assert_eq!(h.injector.replays, 1);

        // If even the replay cannot be queued, nothing else is attempted.
        let mut h = Harness::with_overview(400);
        h.fail_posts(|action| matches!(action, OverviewToggle { .. }));
        h.injector.fail_replay = true;
        h.win_tap(LWIN);
        assert_eq!(h.injector.requests, vec![vec![Down, Up]]);
        assert!(h.sink.posted.is_empty());
        h.deliver_posts();
        h.injector.fail_replay = false;
        h.wait(1_000);
        h.win_tap(LWIN);
        assert_eq!(h.taps(), vec!["toggle"], "not idle after a failed replay");
    }

    #[test]
    fn rapid_second_tap_is_owned_too() {
        let mut h = Harness::with_overview(400);
        h.win_tap(LWIN);
        h.wait(100);
        assert_eq!(h.win_tap(LWIN), [false, false]);
        assert_eq!(h.taps(), vec!["toggle", "second"]);
        // Both taps are masked: Start never opens over the overview.
        assert_eq!(h.injector.requests, vec![vec![Down, Up], vec![Down, Up]]);
        assert_eq!(
            h.sink.posted.last(),
            Some(&OverviewSecondTap { time: h.now }),
            "second tap carries its key-up time"
        );

        // A third quick tap starts a new pair.
        h.wait(100);
        h.win_tap(LWIN);
        assert_eq!(h.taps(), vec!["toggle", "second", "toggle"]);
        assert_eq!(h.injector.requests.len(), 3);
        assert_eq!(h.injector.replays, 0);
    }

    #[test]
    fn second_tap_has_the_same_failure_semantics_as_the_first() {
        // Mask not inserted: Start opens natively, nothing posted.
        let mut h = Harness::with_overview(400);
        h.win_tap(LWIN);
        h.injector.script = [0].into();
        h.wait(100);
        h.win_tap(LWIN);
        assert_eq!(h.taps(), vec!["toggle"]);
        assert_eq!(h.injector.replays, 0);

        // Masked but not delivered: Start is reopened from the hook thread.
        let mut h = Harness::with_overview(400);
        h.fail_posts(|action| matches!(action, OverviewSecondTap { .. }));
        h.win_tap(LWIN);
        h.wait(100);
        h.win_tap(LWIN);
        assert_eq!(h.taps(), vec!["toggle"]);
        assert_eq!(h.injector.requests, vec![vec![Down, Up], vec![Down, Up]]);
        assert_eq!(h.injector.replays, 1, "Start suppressed but not reopened");

        // Partial mask: treated as masked, its key-up retried.
        let mut h = Harness::with_overview(400);
        h.win_tap(LWIN);
        h.injector.script = [1].into();
        h.wait(100);
        h.win_tap(LWIN);
        assert_eq!(h.taps(), vec!["toggle", "second"]);
        assert_eq!(h.injector.requests.last(), Some(&vec![Up]));
    }

    #[test]
    fn slow_taps_interrupted_pairs_and_disabled_double_tap_each_toggle() {
        let slow = |h: &mut Harness| h.wait(500);
        let key_between = |h: &mut Harness| {
            h.tap(A);
        };
        let quick = |h: &mut Harness| h.wait(50);
        let cases: [(&str, u32, Between); 3] = [
            ("slow", 400, &slow),
            ("key between", 400, &key_between),
            ("double tap disabled", 0, &quick),
        ];
        for (name, double_tap_ms, between) in cases {
            let mut h = Harness::with_overview(double_tap_ms);
            h.win_tap(LWIN);
            between(&mut h);
            h.win_tap(LWIN);
            assert_eq!(h.taps(), vec!["toggle", "toggle"], "{name}");
            assert_eq!(h.injector.requests.len(), 2, "{name}");
        }
    }

    #[test]
    fn quiesce_in_any_phase_stops_masking_and_posting() {
        // Before the press, mid-hold, and between the taps of a double tap.
        for phase in 0..3 {
            let mut h = Harness::with_overview(400);
            if phase == 2 {
                h.win_tap(LWIN);
                h.wait(50);
            }
            let requests = h.injector.requests.clone();
            let posted = h.sink.posted.clone();
            if phase == 0 {
                h.shared.quiesce();
            }
            assert!(!h.down_ext(LWIN));
            if phase != 0 {
                h.shared.quiesce();
            }
            assert!(!h.up_ext(LWIN), "phase {phase}: Win key-up consumed");
            assert_eq!(h.injector.requests, requests, "phase {phase}: masked");
            assert_eq!(h.sink.posted, posted, "phase {phase}: posted");
            assert_eq!(h.injector.replays, 0);
        }
    }

    #[test]
    fn shutdown_mid_hold_has_nothing_to_release() {
        let mut h = Harness::with_overview(400);
        h.down_ext(LWIN);
        h.shared.quiesce();
        let mut injector = FakeInjector::default();
        assert_eq!(
            finish_shutdown(&h.shared, true, &mut injector),
            MaskCleanup::NotNeeded
        );
        assert!(injector.requests.is_empty(), "shutdown injected a key");
        // The Win key-up after shutdown is passed and ignored.
        assert!(!h.up_ext(LWIN));
        assert!(h.sink.posted.is_empty() && h.injector.requests.is_empty());
    }

    #[test]
    fn after_a_reset_the_first_ambiguous_gesture_is_native() {
        // Win+L locks the PC; the lock screen takes both key-ups, then the
        // session notification bumps the epoch.
        let mut h = Harness::with_overview(400);
        h.down_ext(LWIN);
        assert!(!h.down(L));
        h.injector.async_down[0x5B] = false;
        h.injector.async_down[0x4C] = false;
        h.bump_epoch();
        h.wait(30_000);
        assert_eq!(h.win_tap(LWIN), [false, false]);
        assert!(h.sink.posted.is_empty(), "post-reset gesture owned");
        assert!(h.injector.requests.is_empty(), "post-reset gesture masked");
        assert_eq!(h.injector.replays, 0);

        // Its key-up was a clean boundary: the next tap is owned.
        h.win_tap(LWIN);
        assert_eq!(h.taps(), vec!["toggle"]);

        // Chords after recovery stay native.
        let requests = h.injector.requests.clone();
        h.down_ext(LWIN);
        assert!(!h.tap(E));
        h.up_ext(LWIN);
        assert_eq!(h.taps(), vec!["toggle"]);
        assert_eq!(h.injector.requests, requests);
    }

    #[test]
    fn keys_held_across_a_reset_never_make_a_tap() {
        // (held key, released during the Win hold?)
        for (held, ext) in [(SHIFT, false), (CTRL, false), (ALT, false), (RWIN, true)] {
            for release_during_hold in [false, true] {
                let name = format!("{held:#x} released during hold: {release_during_hold}");
                let mut h = Harness::with_overview(400);
                h.hold(held, ext);
                h.bump_epoch();
                for _ in 0..3 {
                    h.down_ext(LWIN);
                    if release_during_hold {
                        // Its physical key-up was never paired with a down.
                        h.event(held, ext, false);
                    }
                    h.up_ext(LWIN);
                    h.wait(1_000);
                    if release_during_hold {
                        h.hold(held, ext);
                    }
                }
                assert!(h.sink.posted.is_empty(), "{name}: owned");
                assert!(h.injector.requests.is_empty(), "{name}: masked");
                assert_eq!(h.injector.replays, 0, "{name}: replayed");
                assert!(!h.is_trusted(), "{name}: trusted while a key is held");

                // Released for real: one more native gesture, then owned.
                h.event(held, ext, false);
                h.win_tap(LWIN);
                h.wait(1_000);
                h.win_tap(LWIN);
                assert_eq!(h.taps(), vec!["toggle"], "{name}: never recovered");
            }
        }
    }

    #[test]
    fn an_unpaired_physical_key_up_during_a_hold_is_a_chord() {
        let mut h = Harness::with_overview(400);
        h.down_ext(LWIN);
        // Its key-down went to an elevated window or the secure desktop.
        assert!(!h.up(SHIFT));
        assert!(!h.up_ext(LWIN));
        assert!(h.sink.posted.is_empty() && h.injector.requests.is_empty());
        assert!(!h.is_trusted(), "an unpaired key-up is evidence of desync");
        h.sync_tap();
        h.win_tap(LWIN);
        assert_eq!(h.taps(), vec!["toggle"]);
    }

    #[test]
    fn injected_input_cannot_end_probation_or_create_ownership() {
        for flags in INJECTED_VARIANTS {
            let mut h = Harness::with_overview(400);
            h.bump_epoch();
            // Injected taps of every kind, including Win taps and key-ups that
            // leave nothing believed down: still in probation.
            for (key, ext) in [
                (SYNC, false),
                (A, false),
                (SHIFT, false),
                (LWIN, true),
                (RWIN, true),
            ] {
                assert!(!h.raw(hook_data(key, ext, true, flags)));
                assert!(!h.raw(hook_data(key, ext, false, flags)));
            }
            h.down(A);
            assert!(
                !h.is_trusted(),
                "{flags:#x}: injected input ended probation"
            );
            assert!(!h.raw(hook_data(A, false, false, flags)));
            h.up(A);
            // The physical key-up is the boundary, not the injected one.
            assert!(h.is_trusted(), "{flags:#x}: physical boundary ignored");
            assert!(h.sink.posted.is_empty() && h.injector.requests.is_empty());
        }
    }

    #[test]
    fn residual_an_injected_up_of_the_held_key_hides_it_from_the_veto() {
        // Documented limitation: Windows' own key state is the veto, and an
        // injected key-up of exactly the key held across the reset clears it
        // there too (Windows then also treats that key as up).
        let mut h = Harness::with_overview(400);
        h.hold(SHIFT, false);
        h.bump_epoch();
        assert!(!h.raw(hook_data(SHIFT, false, false, LLKHF_INJECTED.0)));
        h.sync_tap();
        assert!(h.is_trusted());
    }

    #[test]
    fn reset_ends_hotkey_holds_without_posting() {
        // Mid-switch: Alt's key-up is lost; after the reset Tab is plain Tab.
        let mut h = Harness::with_overview(400);
        h.down(ALT);
        assert!(h.tap(TAB));
        h.bump_epoch();
        assert!(!h.tap(TAB), "plain Tab consumed after the reset");
        assert_eq!(h.sink.posted, vec![SwitchApps { reverse: false }]);
        assert_eq!(h.injector.requests, vec![vec![Down, Up]]);

        // A stale Win side must not turn plain Tab into win+tab, and the
        // ambiguous Win gesture after the reset is native.
        for win in [LWIN, RWIN] {
            let mut h = Harness::with_hotkeys_and_overview(["win+`", "win+tab"]);
            h.down_ext(win);
            assert!(h.tap(TAB), "{win:#x}: win+tab not owned");
            h.bump_epoch();
            assert!(!h.tap(TAB), "{win:#x}: plain Tab consumed after the reset");
            assert!(!h.tap(BACKTICK));
            h.up_ext(win);
            assert_eq!(h.sink.posted, vec![SwitchApps { reverse: false }]);
            assert_eq!(h.taps(), Vec::<&str>::new());

            // Win hotkeys work again right away; bare taps after probation.
            h.down_ext(win);
            assert!(h.tap(TAB));
            h.up_ext(win);
            assert_eq!(
                h.sink.posted[1..],
                [SwitchApps { reverse: false }, SwitchAppsDone],
                "{win:#x}"
            );
            h.win_tap(win);
            assert_eq!(h.taps(), vec!["toggle"], "{win:#x}");
        }
    }

    #[test]
    fn a_reset_takes_effect_before_any_ui_cleanup() {
        // The epoch bump alone (no UI message processed, UI stalled) is enough
        // for the very next event to see the reset.
        let mut h = Harness::with_overview(400);
        h.down_ext(LWIN);
        h.bump_epoch();
        assert!(!h.up_ext(LWIN));
        assert!(h.sink.posted.is_empty() && h.injector.requests.is_empty());
        assert!(!h.is_trusted());
    }

    #[test]
    fn missed_reset_only_ever_fails_open_to_native_start() {
        // Stale Win side (its key-up went to an elevated window): the next
        // press is taken as a repeat, so that tap is left to Windows; the one
        // after it works.
        let mut h = Harness::with_overview(400);
        h.down_ext(LWIN);
        h.tap(E);
        assert_eq!(h.win_tap(LWIN), [false, false]);
        assert!(h.sink.posted.is_empty() && h.injector.requests.is_empty());
        h.win_tap(LWIN);
        assert_eq!(h.taps(), vec!["toggle"]);

        // Stale non-Win key: taps are left to Windows until it is pressed and
        // released again.
        let mut h = Harness::with_overview(400);
        h.down(CTRL);
        for _ in 0..3 {
            assert_eq!(h.win_tap(LWIN), [false, false]);
            h.wait(1_000);
        }
        assert!(h.sink.posted.is_empty() && h.injector.requests.is_empty());
        h.tap(CTRL);
        h.win_tap(LWIN);
        assert_eq!(h.taps(), vec!["toggle"]);
    }

    #[test]
    fn injected_key_up_does_not_make_a_chord_look_clean() {
        for flags in INJECTED_VARIANTS {
            let mut h = Harness::with_overview(400);
            h.down(SHIFT);
            assert!(!h.raw(hook_data(SHIFT, false, false, flags)));
            h.win_tap(LWIN);
            h.up(SHIFT);
            assert!(
                h.sink.posted.is_empty(),
                "{flags:#x}: Shift+Win became a tap"
            );
            assert!(h.injector.requests.is_empty(), "{flags:#x}: masked");
        }
    }

    /// Builds a fresh harness for one run.
    type Build = fn() -> Harness;

    /// One step of a physical sequence.
    #[derive(Clone, Copy, Debug)]
    enum Phys {
        Key(u32, bool, bool),
        Wait(u32),
        /// An input reset signal (epoch bump).
        Reset,
    }

    /// What the hook decided for a run: whether each physical event was
    /// consumed, the actions posted, the injection requests, Start replays.
    type Decisions = (Vec<bool>, Vec<HotKeyAction>, Vec<Vec<MaskKey>>, usize);

    fn run_physical(
        build: Build,
        steps: &[Phys],
        injected: Option<(usize, u32, bool, bool, u32)>,
    ) -> Decisions {
        let mut h = build();
        let mut consumed = vec![];
        for index in 0..=steps.len() {
            if let Some((at, key, ext, is_pressed, flags)) = injected {
                if at == index {
                    assert!(
                        !h.raw(hook_data(key, ext, is_pressed, flags)),
                        "injected consumed"
                    );
                }
            }
            match steps.get(index) {
                Some(Phys::Key(key, ext, is_pressed)) => {
                    consumed.push(h.event(*key, *ext, *is_pressed))
                }
                Some(Phys::Wait(ms)) => h.wait(*ms),
                Some(Phys::Reset) => h.bump_epoch(),
                None => {}
            }
        }
        (
            consumed,
            h.sink.posted,
            h.injector.requests,
            h.injector.replays,
        )
    }

    #[test]
    fn injected_events_never_change_any_decision() {
        use Phys::{Key, Wait};
        let tap = |win| [Key(win, true, true), Key(win, true, false)];
        let sequences: Vec<(&str, Vec<Phys>)> = vec![
            ("LWin tap", tap(LWIN).to_vec()),
            (
                "double tap",
                [&tap(LWIN)[..], &[Wait(100)], &tap(LWIN)].concat(),
            ),
            (
                "RWin then LWin",
                [&tap(RWIN)[..], &[Wait(1_000)], &tap(LWIN)].concat(),
            ),
            (
                "Win+E",
                vec![
                    Key(LWIN, true, true),
                    Key(E, false, true),
                    Key(E, false, false),
                    Key(LWIN, true, false),
                ],
            ),
            (
                "Shift then Win",
                vec![
                    Key(SHIFT, false, true),
                    Key(LWIN, true, true),
                    Key(LWIN, true, false),
                    Key(SHIFT, false, false),
                ],
            ),
            (
                "Alt+Tab+Tab",
                vec![
                    Key(ALT, false, true),
                    Key(TAB, false, true),
                    Key(TAB, false, false),
                    Key(TAB, false, true),
                    Key(TAB, false, false),
                    Key(ALT, false, false),
                ],
            ),
            (
                "Alt+Tab, Esc",
                vec![
                    Key(ALT, false, true),
                    Key(TAB, false, true),
                    Key(TAB, false, false),
                    Key(ESC, false, true),
                    Key(ESC, false, false),
                    Key(ALT, false, false),
                ],
            ),
            (
                "Win+Tab",
                vec![
                    Key(LWIN, true, true),
                    Key(TAB, false, true),
                    Key(TAB, false, false),
                    Key(LWIN, true, false),
                ],
            ),
        ];
        let harnesses: [(&str, Build); 3] = [
            ("overview", || Harness::with_overview(400)),
            ("disabled", Harness::new),
            ("win hotkeys + overview", || {
                Harness::with_hotkeys_and_overview(["win+`", "win+tab"])
            }),
        ];
        for (harness, build) in harnesses {
            for (name, steps) in &sequences {
                let baseline = run_physical(build, steps, None);
                let mut keys = vec![(LWIN, true), (RWIN, true), (SHIFT, false), (ALT, false)];
                for step in steps {
                    if let Key(key, ext, _) = step {
                        if !keys.contains(&(*key, *ext)) {
                            keys.push((*key, *ext));
                        }
                    }
                }
                for flags in INJECTED_VARIANTS {
                    for at in 0..=steps.len() {
                        for (key, ext) in &keys {
                            for is_pressed in [true, false] {
                                let injected = Some((at, *key, *ext, is_pressed, flags));
                                assert_eq!(
                                    run_physical(build, steps, injected),
                                    baseline,
                                    "{harness}, {name}: injected {key:#x} pressed={is_pressed} \
                                     flags={flags:#x} before step {at}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn injected_events_after_a_reset_can_only_make_gestures_more_native() {
        use Phys::{Key, Reset, Wait};
        let tap = [Key(LWIN, true, true), Key(LWIN, true, false)];
        let type_e = [Key(E, false, true), Key(E, false, false)];
        let sequences: Vec<(&str, Vec<Phys>)> = vec![
            (
                "reset, tap, tap",
                [&[Reset][..], &tap, &[Wait(1_000)], &tap].concat(),
            ),
            ("reset, type, tap", [&[Reset][..], &type_e, &tap].concat()),
            (
                "reset, tap, double tap",
                [&[Reset][..], &tap, &[Wait(1_000)], &tap, &[Wait(100)], &tap].concat(),
            ),
        ];
        let owned = |d: &Decisions| d.1.len() + d.2.len();
        let build: Build = || Harness::with_overview(400);
        for (name, steps) in &sequences {
            let baseline = run_physical(build, steps, None);
            assert!(owned(&baseline) > 0, "{name}: baseline never recovers");
            for flags in INJECTED_VARIANTS {
                for at in 0..=steps.len() {
                    for (key, ext) in [
                        (LWIN, true),
                        (RWIN, true),
                        (SHIFT, false),
                        (E, false),
                        (SYNC, false),
                    ] {
                        for is_pressed in [true, false] {
                            let with =
                                run_physical(build, steps, Some((at, key, ext, is_pressed, flags)));
                            let what = format!(
                                "{name}: injected {key:#x} pressed={is_pressed} flags={flags:#x} before step {at}"
                            );
                            assert_eq!(with.0, baseline.0, "{what}: consume changed");
                            assert!(owned(&with) <= owned(&baseline), "{what}: more owned");
                            assert_eq!(with.3, baseline.3, "{what}: replay changed");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn tap_with_start_open_is_left_to_windows_to_close_it() {
        let mut h = Harness::with_overview(250);
        // Start is visibly the foreground window (opened with Ctrl+Esc): taps
        // close/reopen it natively, unmasked and unposted, however fast.
        h.start_open = true;
        for wait in [50, 1_000] {
            h.wait(wait);
            assert_eq!(h.win_tap(LWIN), [false, false]);
        }
        assert!(h.sink.posted.is_empty());
        assert!(h.injector.requests.is_empty());

        // Once Start has closed, taps are owned again; the native taps above
        // never opened a double-tap window.
        h.start_open = false;
        h.wait(50);
        h.win_tap(LWIN);
        h.wait(50);
        h.win_tap(LWIN);
        assert_eq!(h.taps(), vec!["toggle", "second"]);
    }

    #[test]
    fn restarted_state_starts_in_probation() {
        let mut h = Harness::with_overview(400);
        h.down_ext(LWIN);
        h.down(SHIFT);
        let fresh = KeyboardState::new(&[], Some(SuperTapConfig { double_tap_ms: 400 }));
        let tap = fresh.super_tap.as_ref().expect("tap state when enabled");
        assert_eq!(tap.phase(), crate::super_key::SuperPhase::Idle);
        assert_eq!(tap.sync(), crate::super_key::PhysicalSync::Probation);
        *h.state() = fresh;
        h.up(SHIFT);
        h.up_ext(LWIN);
        h.win_tap(LWIN);
        assert!(h.sink.posted.is_empty(), "first gesture after start owned");
        h.win_tap(LWIN);
        assert_eq!(h.taps(), vec!["toggle"]);
    }

    #[test]
    fn disabled_overview_builds_no_tap_state_and_leaves_win_native() {
        let mut h = Harness::new();
        assert!(h.state().super_tap.is_none());
        for win in [LWIN, RWIN] {
            for _ in 0..3 {
                assert_eq!(h.win_tap(win), [false, false]);
                h.wait(100);
            }
        }
        h.down_ext(LWIN);
        h.wait(5_000);
        h.up_ext(LWIN);
        assert!(h.sink.posted.is_empty());
        assert!(h.injector.requests.is_empty());
        assert_eq!(h.injector.replays, 0);
    }

    #[test]
    fn overview_actions_map_to_their_messages() {
        let message = |action| action_message(action).map(|(msg, lparam)| (msg, lparam.0 as u32));
        assert_eq!(
            message(OverviewToggle { time: 0xFFFF_FFF0 }),
            Some((WM_USER_OVERVIEW_TOGGLE, 0xFFFF_FFF0))
        );
        assert_eq!(
            message(OverviewSecondTap { time: 1234 }),
            Some((WM_USER_OVERVIEW_SECOND_TAP, 1234))
        );
    }
}
