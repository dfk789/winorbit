use crate::config::{edit_config_file, Config};
use crate::foreground::ForegroundWatcher;
use crate::input_reset::InputResetWatcher;
use crate::keyboard::{reset_input_state, KeyboardListener};
use crate::painter::{find_clicked_app_index, GdiAAPainter};
use crate::preview::{PreviewSource, WindowPreviewSource};
use crate::startup::Startup;
use crate::switch_apps::{
    representative_window_index, AppSwitchEntry, AppSwitchWindow, SwitchAppsState,
};
use crate::trayicon::TrayIcon;
use crate::utils::{
    check_error, get_app_icon, get_foreground_window, get_window_user_data, is_iconic_window,
    is_running_as_admin, list_windows, set_foreground_window, set_window_user_data,
};

use anyhow::{anyhow, bail, Result};
use indexmap::IndexSet;
use std::collections::HashMap;
use windows::core::{w, PCWSTR};
use windows::Win32::{
    Foundation::{GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, GetWindowLongPtrW,
        LoadCursorW, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW,
        SetWindowLongPtrW, TranslateMessage, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, GWL_STYLE,
        HICON, HTCLIENT, IDC_ARROW, MSG, WINDOW_STYLE, WM_COMMAND, WM_ERASEBKGND, WM_LBUTTONUP,
        WM_NCHITTEST, WM_RBUTTONUP, WM_WTSSESSION_CHANGE, WNDCLASSW, WS_CAPTION, WS_EX_LAYERED,
        WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WTS_CONSOLE_CONNECT, WTS_REMOTE_CONNECT, WTS_SESSION_LOCK,
        WTS_SESSION_UNLOCK,
    },
};

pub const NAME: PCWSTR = w!("WinOrbit");
pub const WM_USER_TRAYICON: u32 = 6000;
pub const WM_USER_REGISTER_TRAYICON: u32 = 6001;
pub const WM_USER_SWITCH_APPS: u32 = 6010;
pub const WM_USER_SWITCH_APPS_DONE: u32 = 6011;
pub const WM_USER_SWITCH_APPS_CANCEL: u32 = 6012;
pub const WM_USER_SWITCH_WINDOWS: u32 = 6020;
pub const WM_USER_SWITCH_WINDOWS_DONE: u32 = 6021;
/// Clean bare Win tap (Start suppressed); `lparam` is the key-up's
/// `KBDLLHOOKSTRUCT.time`. Only logged until the overview exists.
pub const WM_USER_OVERVIEW_TOGGLE: u32 = 6030;
/// Second tap of a double tap (Windows opens Start natively); `lparam` as above.
pub const WM_USER_OVERVIEW_SECOND_TAP: u32 = 6031;
pub const IDM_EXIT: u32 = 1;
pub const IDM_STARTUP: u32 = 2;
pub const IDM_CONFIGURE: u32 = 3;

pub fn start(config: &Config) -> Result<()> {
    info!("start config={config:?}");
    App::start(config)
}

/// Listen to this message to recreate the tray icon since the taskbar has been recreated.
static mut WM_TASKBARCREATED: u32 = 0;

pub struct App {
    hwnd: HWND,
    is_admin: bool,
    trayicon: Option<TrayIcon>,
    startup: Startup,
    config: Config,
    switch_windows_state: SwitchWindowsState,
    switch_apps_state: Option<SwitchAppsState>,
    cached_icons: HashMap<String, HICON>,
    painter: GdiAAPainter,
    /// Owned by the app so it is stopped before the app state is freed.
    keyboard_listener: KeyboardListener,
    /// Lock/unlock and desktop-switch signals; unregistered when the app is
    /// freed after the message loop, on this thread.
    _input_reset: InputResetWatcher,
}

impl App {
    pub fn start(config: &Config) -> Result<()> {
        let hwnd = Self::create_window()?;
        let painter = GdiAAPainter::new(hwnd)?;

        let _foreground_watcher = ForegroundWatcher::init(
            &config.switch_windows_blacklist,
            config.overview.super_tap().is_some(),
        )?;
        if let Some(error) = &config.overview.error {
            warn!("{error}");
        }
        let keyboard_listener =
            KeyboardListener::init(hwnd, &config.to_hotkeys(), config.overview.super_tap())?;
        let input_reset = InputResetWatcher::init(hwnd);

        let trayicon = match config.trayicon {
            true => Some(TrayIcon::create()),
            false => None,
        };

        let is_admin = is_running_as_admin()?;
        debug!("is_admin {is_admin}");

        let startup = Startup::init(is_admin)?;

        let mut app = App {
            hwnd,
            is_admin,
            trayicon,
            startup,
            config: config.clone(),
            switch_windows_state: SwitchWindowsState {
                cache: None,
                modifier_released: true,
            },
            switch_apps_state: None,
            cached_icons: Default::default(),
            painter,
            keyboard_listener,
            _input_reset: input_reset,
        };

        app.set_trayicon();

        let app_ptr = Box::into_raw(Box::new(app)) as _;
        check_error(|| set_window_user_data(hwnd, app_ptr))
            .map_err(|err| anyhow!("Failed to set window ptr, {err}"))?;

        let result = Self::eventloop();
        // The app is freed only here, after the message loop, never from inside
        // a message handler that may itself be running under a `get_app` borrow.
        drop(take_app(hwnd));
        result
    }

    fn eventloop() -> Result<()> {
        let mut message = MSG::default();
        loop {
            let ret = unsafe { GetMessageW(&mut message, None, 0, 0) };
            match ret.0 {
                -1 => {
                    unsafe { GetLastError() }.ok()?;
                }
                0 => break,
                _ => unsafe {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                },
            }
        }

        Ok(())
    }

    fn create_window() -> Result<HWND> {
        unsafe { WM_TASKBARCREATED = RegisterWindowMessageW(w!("TaskbarCreated")) };

        let hinstance = unsafe { GetModuleHandleW(None) }
            .map_err(|err| anyhow!("Failed to get current module handle, {err}"))?;

        let hcursor = unsafe { LoadCursorW(None, IDC_ARROW) }
            .map_err(|err| anyhow!("Failed to load arrow cursor, {err}"))?;

        let window_class = WNDCLASSW {
            hCursor: hcursor,
            hInstance: HINSTANCE(hinstance.0),
            lpszClassName: NAME,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(App::window_proc),
            ..Default::default()
        };

        let atom = check_error(|| unsafe { RegisterClassW(&window_class) })
            .map_err(|err| anyhow!("Failed to register class, {err}"))?;

        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                PCWSTR(atom as _),
                NAME,
                WINDOW_STYLE(0),
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                None,
                None,
                Some(hinstance.into()),
                None,
            )
        }
        .map_err(|err| anyhow!("Failed to create windows, {err}"))?;

        // hide caption
        let mut style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32;
        style &= !WS_CAPTION.0;
        unsafe { SetWindowLongPtrW(hwnd, GWL_STYLE, style as _) };

        Ok(hwnd)
    }

    fn set_trayicon(&mut self) {
        if let Some(trayicon) = self.trayicon.as_mut() {
            match trayicon.register(self.hwnd) {
                Ok(()) => info!("trayicon registered"),
                Err(err) => {
                    if !trayicon.exist() {
                        error!("{err}, retrying in 3 second");
                        let hwnd = self.hwnd.0 as isize;
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_secs(3));
                            let _ = unsafe {
                                PostMessageW(
                                    Some(HWND(hwnd as _)),
                                    WM_USER_REGISTER_TRAYICON,
                                    WPARAM(0),
                                    LPARAM(0),
                                )
                            };
                        });
                    }
                }
            }
        }
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match Self::handle_message(hwnd, msg, wparam, lparam) {
            Ok(ret) => ret,
            Err(err) => {
                error!("{err}");
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
    }

    fn handle_message(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Result<LRESULT> {
        match msg {
            WM_USER_TRAYICON => {
                let app = get_app(hwnd)?;
                if let Some(trayicon) = app.trayicon.as_mut() {
                    let keycode = lparam.0 as u32;
                    if keycode == WM_LBUTTONUP || keycode == WM_RBUTTONUP {
                        trayicon.show(app.startup.is_enable)?;
                    }
                }
                return Ok(LRESULT(0));
            }
            WM_USER_SWITCH_APPS => {
                debug!("message WM_USER_SWITCH_APPS");
                let app = get_app(hwnd)?;
                let reverse = lparam.0 == 1;
                app.switch_apps(reverse)?;
                if let Some(state) = &app.switch_apps_state {
                    app.painter.paint(state);
                }
            }
            WM_USER_SWITCH_APPS_DONE => {
                debug!("message WM_USER_SWITCH_APPS_DONE");
                let app = get_app(hwnd)?;
                app.do_switch_app();
            }
            WM_USER_SWITCH_APPS_CANCEL => {
                debug!("message WM_USER_SWITCH_APPS_CANCEL");
                let app = get_app(hwnd)?;
                app.cancel_switch_app();
            }
            WM_USER_SWITCH_WINDOWS => {
                debug!("message WM_USER_SWITCH_WINDOWS");
                let app = get_app(hwnd)?;
                let reverse = lparam.0 == 1;
                if app.switch_apps_state.is_some() {
                    // Inline same-app cycling: update the selected window within
                    // the overlay without foregrounding or dismissing.
                    if let Some(state) = app.switch_apps_state.as_mut() {
                        state.cycle_window(reverse);
                        app.painter.paint(state);
                    }
                } else {
                    let hwnd = get_foreground_window();
                    app.switch_windows(hwnd, reverse)?;
                }
            }
            WM_USER_SWITCH_WINDOWS_DONE => {
                debug!("message WM_USER_SWITCH_WINDOWS_DONE");
                let app = get_app(hwnd)?;
                app.switch_windows_state.modifier_released = true;
            }
            WM_USER_OVERVIEW_TOGGLE => {
                info!("bare Win tap (time {}): overview toggle", lparam.0 as u32);
            }
            WM_USER_OVERVIEW_SECOND_TAP => {
                info!(
                    "bare Win double tap (time {}): Start left to Windows",
                    lparam.0 as u32
                );
            }
            WM_WTSSESSION_CHANGE => {
                let event = wparam.0 as u32;
                let reason = match event {
                    WTS_SESSION_LOCK => Some("session lock"),
                    WTS_SESSION_UNLOCK => Some("session unlock"),
                    WTS_CONSOLE_CONNECT => Some("console connect"),
                    WTS_REMOTE_CONNECT => Some("remote connect"),
                    _ => None,
                };
                if let Some(reason) = reason {
                    reset_input_state(reason);
                    if event == WTS_SESSION_LOCK {
                        // The hook dropped the hold, so no release will commit.
                        let app = get_app(hwnd)?;
                        app.cancel_switch_app();
                        app.switch_windows_state.modifier_released = true;
                    }
                }
            }
            WM_NCHITTEST => {
                return Ok(LRESULT(HTCLIENT as _));
            }
            WM_LBUTTONUP => {
                let app = get_app(hwnd)?;
                app.click();
            }
            WM_COMMAND => {
                let value = wparam.0 as u32;
                let kind = ((value >> 16) & 0xffff) as u16;
                let id = value & 0xffff;
                if kind == 0 {
                    match id {
                        IDM_EXIT => {
                            // Stop keyboard messages now; the app itself is
                            // freed after the message loop ends.
                            if let Ok(app) = get_app(hwnd) {
                                app.keyboard_listener.quiesce();
                            }
                            unsafe { PostQuitMessage(0) }
                        }
                        IDM_STARTUP => {
                            let app = get_app(hwnd)?;
                            app.startup.toggle()?;
                        }
                        IDM_CONFIGURE => {
                            if let Err(err) = edit_config_file() {
                                alert!("{err}");
                            }
                        }
                        _ => {}
                    }
                }
            }
            WM_ERASEBKGND => {
                return Ok(LRESULT(0));
            }
            _ if msg == WM_USER_REGISTER_TRAYICON || unsafe { msg == WM_TASKBARCREATED } => {
                let app = get_app(hwnd)?;
                app.set_trayicon();
            }
            _ => {}
        }
        Ok(unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) })
    }

    fn switch_windows(&mut self, hwnd: HWND, reverse: bool) -> Result<bool> {
        let windows = list_windows(
            self.config.switch_windows_ignore_minimal,
            self.config.switch_windows_only_current_desktop(),
            self.is_admin,
        )?;
        debug!(
            "switch windows: hwnd:{hwnd:?} reverse:{reverse} state:{:?}",
            self.switch_windows_state
        );
        let module_path = match windows
            .iter()
            .find(|(_, v)| v.iter().any(|(id, _)| *id == hwnd))
            .map(|(k, _)| k.clone())
        {
            Some(v) => v,
            None => return Ok(false),
        };
        match windows.get(&module_path) {
            None => Ok(false),
            Some(windows) => {
                let windows_len = windows.len();
                if windows_len == 1 {
                    return Ok(false);
                }
                let current_id = windows[0].0;
                let mut index = 1;
                let mut state_id = current_id;
                let mut state_windows = vec![];
                if windows_len > 2 {
                    if let Some((cache_module_path, cache_id, cache_index, cache_windows)) =
                        self.switch_windows_state.cache.as_ref()
                    {
                        if cache_module_path == &module_path {
                            if self.switch_windows_state.modifier_released {
                                if *cache_id != current_id {
                                    if let Some((i, _)) =
                                        windows.iter().enumerate().find(|(_, (v, _))| v == cache_id)
                                    {
                                        index = i;
                                    }
                                }
                            } else {
                                state_id = *cache_id;
                                let mut windows_set: IndexSet<isize> =
                                    windows.iter().map(|(v, _)| v.0 as _).collect();
                                for id in cache_windows {
                                    if windows_set.contains(id) {
                                        state_windows.push(*id);
                                        windows_set.swap_remove(id);
                                    }
                                }
                                state_windows.extend(windows_set);
                                index = if reverse {
                                    if *cache_index == 0 {
                                        windows_len - 1
                                    } else {
                                        cache_index - 1
                                    }
                                } else if *cache_index >= windows_len - 1 {
                                    0
                                } else {
                                    cache_index + 1
                                };
                            }
                        }
                    }
                }
                if state_windows.is_empty() {
                    state_windows = windows.iter().map(|(v, _)| v.0 as _).collect();
                }
                let hwnd = HWND(state_windows[index] as _);
                self.switch_windows_state = SwitchWindowsState {
                    cache: Some((module_path.clone(), state_id, index, state_windows)),
                    modifier_released: false,
                };
                set_foreground_window(hwnd);

                Ok(true)
            }
        }
    }

    fn switch_apps(&mut self, reverse: bool) -> Result<()> {
        debug!(
            "switch apps: reverse:{reverse}, state:{:?}, render_mode:{:?}, show_window_count:{}",
            self.switch_apps_state,
            self.config.switch_apps_render_mode,
            self.config.switch_apps_show_window_count
        );
        if let Some(state) = self.switch_apps_state.as_mut() {
            if reverse {
                if state.index == 0 {
                    state.index = state.apps.len() - 1;
                } else {
                    state.index -= 1;
                }
            } else if state.index == state.apps.len() - 1 {
                state.index = 0;
            } else {
                state.index += 1;
            };
            state.reset_window_index();
            debug!("switch apps: new index:{}", state.index);
            return Ok(());
        }
        let windows = list_windows(
            self.config.switch_apps_ignore_minimal,
            self.config.switch_apps_only_current_desktop(),
            self.is_admin,
        )?;
        let preview_source = WindowPreviewSource::new(self.config.switch_apps_render_mode);
        let mut apps = vec![];
        for (module_path, hwnds) in windows.iter() {
            let windows: Vec<AppSwitchWindow> = hwnds
                .iter()
                .map(|(hwnd, title)| AppSwitchWindow::new(*hwnd, title.clone()))
                .collect();
            let representative_hwnd = representative_window_index(
                &windows,
                self.config.switch_apps_representative_window,
                |window| is_iconic_window(window.hwnd),
            )
            .map(|index| windows[index].hwnd)
            .unwrap_or(windows[0].hwnd);
            // Preview and activation stay aligned on the same representative window.
            let preview = preview_source.preview_for_hwnd(representative_hwnd);
            let module_hicon = self
                .cached_icons
                .entry(module_path.clone())
                .or_insert_with(|| {
                    get_app_icon(
                        &self.config.switch_apps_override_icons,
                        module_path,
                        representative_hwnd,
                    )
                });
            apps.push(AppSwitchEntry::new(
                module_path.clone(),
                *module_hicon,
                representative_hwnd,
                preview,
                windows,
            ));
        }
        let num_apps = apps.len() as i32;
        if num_apps == 0 {
            return Ok(());
        }

        let index = if apps.len() == 1 {
            0
        } else if reverse {
            apps.len() - 1
        } else {
            1
        };

        let state = SwitchAppsState {
            apps,
            index,
            window_index: 0,
            render_mode: self.config.switch_apps_render_mode,
            show_window_count: self.config.switch_apps_show_window_count,
            overlay_scale: self.config.switch_apps_overlay_scale,
            backdrop_opacity: self.config.switch_apps_backdrop_opacity,
            backdrop_color: self.config.switch_apps_backdrop_color,
        };
        self.switch_apps_state = Some(state);
        debug!("switch apps, new state:{:?}", self.switch_apps_state);
        Ok(())
    }

    fn click(&mut self) {
        if let Some(state) = self.switch_apps_state.as_mut() {
            if let Some(i) = find_clicked_app_index(state) {
                state.index = i;
                self.do_switch_app();
            }
        }
    }

    fn do_switch_app(&mut self) {
        if let Some(state) = self.switch_apps_state.take() {
            if let Some(hwnd) = state.selected_hwnd() {
                set_foreground_window(hwnd);
            }
            self.painter.unpaint(state);
        }
    }

    fn cancel_switch_app(&mut self) {
        if let Some(state) = self.switch_apps_state.take() {
            self.painter.unpaint(state);
        }
    }
}

/// Returns the app stored in the window's user data.
///
/// The pointer is null before `App::start` stores it and after `take_app`
/// clears it, so messages still queued from the keyboard hook, the tray retry
/// thread or `TaskbarCreated` broadcasts are rejected instead of dereferenced.
fn get_app(hwnd: HWND) -> Result<&'static mut App> {
    let ptr = check_error(|| get_window_user_data(hwnd))
        .map_err(|err| anyhow!("Failed to get window ptr, {err}"))?;
    if ptr == 0 {
        bail!("App is not available");
    }
    // SAFETY: a non-null pointer was stored by `App::start` from `Box::into_raw`
    // and stays valid until `take_app` clears it on this same UI thread.
    Ok(unsafe { &mut *(ptr as *mut App) })
}

/// Clears the window's app pointer and returns ownership of the app, if any.
fn take_app(hwnd: HWND) -> Option<Box<App>> {
    let ptr = get_window_user_data(hwnd);
    if ptr == 0 {
        return None;
    }
    if let Err(err) = check_error(|| set_window_user_data(hwnd, 0)) {
        // Leaking is safer than freeing an app the window may still reach.
        error!("Failed to clear window ptr, {err}");
        return None;
    }
    // SAFETY: the pointer came from `Box::into_raw` in `App::start` and has just
    // been removed from the window, so this is the only owner.
    Some(unsafe { Box::from_raw(ptr as *mut App) })
}

#[derive(Debug)]
struct SwitchWindowsState {
    cache: Option<(String, HWND, usize, Vec<isize>)>,
    modifier_released: bool,
}
