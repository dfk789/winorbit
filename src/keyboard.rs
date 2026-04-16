use crate::{
    app::{
        WM_USER_SWITCH_APPS, WM_USER_SWITCH_APPS_CANCEL, WM_USER_SWITCH_APPS_DONE,
        WM_USER_SWITCH_WINDOWS, WM_USER_SWITCH_WINDOWS_DONE,
    },
    config::{Hotkey, SWITCH_APPS_HOTKEY_ID, SWITCH_WINDOWS_HOTKEY_ID},
    foreground::IS_FOREGROUND_IN_BLACKLIST,
};

use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use std::sync::LazyLock;
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Input::KeyboardAndMouse::{SCANCODE_LSHIFT, SCANCODE_RSHIFT},
        WindowsAndMessaging::{
            CallNextHookEx, SendMessageW, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK,
            KBDLLHOOKSTRUCT, LLKHF_UP, WH_KEYBOARD_LL,
        },
    },
};

static KEYBOARD_STATE: LazyLock<Mutex<Vec<HotKeyState>>> = LazyLock::new(|| Mutex::new(Vec::new()));
static mut WINDOW: HWND = HWND(0 as _);
static mut IS_SHIFT_PRESSED: bool = false;

#[derive(Debug)]
pub struct KeyboardListener {
    hook: HHOOK,
}

impl KeyboardListener {
    pub fn init(hwnd: HWND, hotkeys: &[&Hotkey]) -> Result<Self> {
        unsafe { WINDOW = hwnd }

        let keyboard_state = hotkeys
            .iter()
            .map(|hotkey| HotKeyState::new((*hotkey).clone()))
            .collect();
        *KEYBOARD_STATE.lock() = keyboard_state;

        let hook = unsafe {
            let hinstance = { GetModuleHandleW(None) }
                .map_err(|err| anyhow!("Failed to get module handle, {err}"))?;
            SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(keyboard_proc),
                Some(hinstance.into()),
                0,
            )
        }
        .map_err(|err| anyhow!("Failed to set windows hook, {err}"))?;
        info!("keyboard listener start");

        Ok(Self { hook })
    }
}

impl Drop for KeyboardListener {
    fn drop(&mut self) {
        debug!("keyboard listener destroyed");
        if !self.hook.is_invalid() {
            let _ = unsafe { UnhookWindowsHookEx(self.hook) };
        }
    }
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
    if [SCANCODE_LSHIFT, SCANCODE_RSHIFT].contains(&scan_code) {
        IS_SHIFT_PRESSED = is_key_pressed();
    }
    let reverse = IS_SHIFT_PRESSED;
    let mut keyboard_state = KEYBOARD_STATE.lock();
    for state in keyboard_state.iter_mut() {
        if let Some(action) = state.handle_modifier_event(scan_code, is_key_pressed()) {
            is_modifier = true;
            dispatch_action(action);
        } else if state.hotkey.modifier.contains(&scan_code) {
            is_modifier = true;
        }
    }
    if !is_modifier {
        for state in keyboard_state.iter_mut() {
            if let Some(action) = state.handle_hotkey_event(
                scan_code,
                is_key_pressed(),
                reverse,
                state.hotkey.id != SWITCH_WINDOWS_HOTKEY_ID || !IS_FOREGROUND_IN_BLACKLIST,
            ) {
                dispatch_action(action);
                return LRESULT(1);
            }
            if let Some(action) = state.handle_cancel_event(scan_code, is_key_pressed()) {
                dispatch_action(action);
                return LRESULT(1);
            }
        }
    }
    CallNextHookEx(None, code, w_param, l_param)
}

fn dispatch_action(action: HotKeyAction) {
    match action {
        HotKeyAction::Consume => {}
        HotKeyAction::SwitchApps { reverse } => unsafe {
            let _ = SendMessageW(
                WINDOW,
                WM_USER_SWITCH_APPS,
                None,
                Some(LPARAM(reverse as isize)),
            );
        },
        HotKeyAction::SwitchAppsDone => unsafe {
            let _ = SendMessageW(WINDOW, WM_USER_SWITCH_APPS_DONE, None, None);
        },
        HotKeyAction::SwitchAppsCancel => unsafe {
            let _ = SendMessageW(WINDOW, WM_USER_SWITCH_APPS_CANCEL, None, None);
        },
        HotKeyAction::SwitchWindows { reverse } => unsafe {
            let _ = SendMessageW(
                WINDOW,
                WM_USER_SWITCH_WINDOWS,
                None,
                Some(LPARAM(reverse as isize)),
            );
        },
        HotKeyAction::SwitchWindowsDone => unsafe {
            let _ = SendMessageW(WINDOW, WM_USER_SWITCH_WINDOWS_DONE, None, None);
        },
    };
}

#[cfg(test)]
mod tests {
    use super::{HotKeyAction, HotKeyState};
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
}
